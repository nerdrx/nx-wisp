//! **F55 — the model fetcher**, entirely offline.
//!
//! Every rule SPEC §0.2a implies is exercised against a fake transport: pinned
//! hash, verify-then-move, resume, off-by-default. Nothing here opens a socket,
//! which is the point — the network path is a `Box<dyn Http>` precisely so that
//! the interesting failure modes (a mirror that serves the wrong bytes, a
//! connection that dies at 60%, a server that ignores `Range`) are tests rather
//! than hopes.

mod common;

use std::io::Read;
use std::sync::{Arc, Mutex};

use common::Fixture;
use sha2::{Digest, Sha256};
use wisp_mind::error::MindError;
use wisp_mind::fetch::{Fetched, Fetcher, Http, HttpStream, Progress};
use wisp_mind::models::{ModelEntry, ModelRegistry};

/// A server, with a personality.
#[derive(Clone, Default)]
struct FakeHttp {
    body: Arc<Vec<u8>>,
    /// Cut the stream off after this many bytes of the *response*.
    cut_after: Option<usize>,
    /// Pretend not to understand `Range`, the way some CDNs do.
    ignore_range: bool,
    /// Every `(url, from)` it was asked for.
    calls: Arc<Mutex<Vec<(String, u64)>>>,
    fail_with: Option<String>,
}

impl FakeHttp {
    fn serving(body: Vec<u8>) -> Self {
        FakeHttp {
            body: Arc::new(body),
            ..FakeHttp::default()
        }
    }
    fn cutting_after(mut self, n: usize) -> Self {
        self.cut_after = Some(n);
        self
    }
    fn ignoring_range(mut self) -> Self {
        self.ignore_range = true;
        self
    }
    fn calls(&self) -> Vec<(String, u64)> {
        self.calls.lock().expect("calls").clone()
    }
}

impl Http for FakeHttp {
    fn open(&self, url: &str, from: u64) -> Result<HttpStream, String> {
        self.calls
            .lock()
            .expect("calls")
            .push((url.to_string(), from));
        if let Some(e) = &self.fail_with {
            return Err(e.clone());
        }
        let start = if self.ignore_range { 0 } else { from as usize };
        let mut rest = self.body[start.min(self.body.len())..].to_vec();
        if let Some(n) = self.cut_after {
            rest.truncate(n);
        }
        Ok(HttpStream {
            total: Some(self.body.len() as u64),
            resumed: !self.ignore_range && from > 0,
            body: Box::new(std::io::Cursor::new(rest)) as Box<dyn Read + Send>,
        })
    }
}

fn sha(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A registry entry for some bytes we made up.
fn entry(body: &[u8]) -> ModelEntry {
    ModelEntry {
        name: "test-model".into(),
        role: wisp_mind::backend::Role::Reflex,
        file: "test-model.gguf".into(),
        url: "https://example.invalid/test-model.gguf".into(),
        sha256: sha(body),
        size_bytes: body.len() as u64,
        vram_mib: 100,
        context_max: 4096,
        embedding_dim: 0,
        chat_template: None,
        license: None,
        default: false,
        notes: None,
    }
}

fn body(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

fn no_progress(_: Progress) {}

#[test]
fn nothing_is_fetched_until_the_operator_says_so() {
    let f = Fixture::new();
    let bytes = body(4096);
    let e = entry(&bytes);
    // SPEC §0.2a and `ModelSettings::allow_downloads`, which ships false.
    let fetcher = Fetcher::new(Box::new(FakeHttp::serving(bytes.clone())), false);
    let err = fetcher
        .ensure(&e, &f.models_dir, &mut no_progress)
        .unwrap_err();
    assert!(matches!(err, MindError::DownloadsDisabled(_)), "{err}");
    assert!(!e.local_path(&f.models_dir).exists());
}

#[test]
fn a_download_verifies_before_it_moves_anything_into_place() {
    let f = Fixture::new();
    let bytes = body(300_000);
    let e = entry(&bytes);
    let http = FakeHttp::serving(bytes.clone());
    let fetcher = Fetcher::new(Box::new(http.clone()), true);

    let mut seen: Vec<Progress> = Vec::new();
    let got = fetcher
        .ensure(&e, &f.models_dir, &mut |p| seen.push(p))
        .expect("fetch");
    assert!(matches!(got, Fetched::Downloaded { .. }));
    let path = e.local_path(&f.models_dir);
    assert_eq!(std::fs::read(&path).expect("read"), bytes);

    // No partial left behind, ever.
    assert!(!path.with_extension("gguf.part").exists());
    assert!(std::fs::read_dir(&f.models_dir)
        .expect("dir")
        .flatten()
        .all(|d| !d.file_name().to_string_lossy().ends_with(".part")));

    // Progress was reported, and it ends where it should.
    assert!(!seen.is_empty());
    let last = seen.last().expect("progress");
    assert_eq!(last.done_bytes, bytes.len() as u64);
    assert!((last.fraction() - 1.0).abs() < 1e-6);
}

#[test]
fn bytes_that_do_not_hash_to_the_pin_never_become_a_model() {
    let f = Fixture::new();
    let real = body(50_000);
    let mut e = entry(&real);
    // The mirror serves something else.
    let served = body(50_000).iter().map(|b| b ^ 0xff).collect::<Vec<u8>>();
    e.size_bytes = served.len() as u64;

    let fetcher = Fetcher::new(Box::new(FakeHttp::serving(served)), true);
    let err = fetcher
        .ensure(&e, &f.models_dir, &mut no_progress)
        .unwrap_err();
    match err {
        MindError::HashMismatch { want, got, .. } => assert_ne!(want, got),
        other => panic!("expected a hash mismatch, got {other}"),
    }
    assert!(
        !e.local_path(&f.models_dir).exists(),
        "a file that failed verification must not be sitting where a loader would find it"
    );
    // And the bad bytes are not left to be resumed onto.
    let part = f.models_dir.join("test-model.gguf.part");
    assert!(!part.exists(), "the .part must be deleted, not kept");
}

#[test]
fn a_download_that_dies_two_thirds_of_the_way_through_resumes() {
    let f = Fixture::new();
    let bytes = body(300_000);
    let e = entry(&bytes);

    // First attempt: the connection dies at 200 000 bytes.
    let flaky = FakeHttp::serving(bytes.clone()).cutting_after(200_000);
    let err = Fetcher::new(Box::new(flaky.clone()), true)
        .ensure(&e, &f.models_dir, &mut no_progress)
        .unwrap_err();
    assert!(matches!(err, MindError::SizeMismatch { .. }), "{err}");
    let part = f.models_dir.join("test-model.gguf.part");
    assert_eq!(
        std::fs::metadata(&part).expect("part kept").len(),
        200_000,
        "a short read keeps what it got — that is what resume is for"
    );

    // Second attempt: a healthy server, and it asks for the rest.
    let good = FakeHttp::serving(bytes.clone());
    let got = Fetcher::new(Box::new(good.clone()), true)
        .ensure(&e, &f.models_dir, &mut no_progress)
        .expect("resumes");
    match got {
        Fetched::Downloaded {
            bytes: n,
            resumed_from,
            ..
        } => {
            assert_eq!(resumed_from, 200_000);
            assert_eq!(n, 100_000, "only the remainder was transferred");
        }
        other => panic!("expected a resumed download, got {other:?}"),
    }
    assert_eq!(good.calls(), vec![(e.url.clone(), 200_000)]);
    assert_eq!(std::fs::read(e.local_path(&f.models_dir)).expect("read"), bytes);
}

#[test]
fn a_server_that_ignores_range_makes_us_start_again_rather_than_corrupt_the_file() {
    let f = Fixture::new();
    let bytes = body(120_000);
    let e = entry(&bytes);

    // Get a partial file on disk.
    Fetcher::new(
        Box::new(FakeHttp::serving(bytes.clone()).cutting_after(60_000)),
        true,
    )
    .ensure(&e, &f.models_dir, &mut no_progress)
    .expect_err("short");

    // Now a server that replies 200 to a Range request. Appending would produce
    // a file of the right *length* and the wrong contents.
    let got = Fetcher::new(
        Box::new(FakeHttp::serving(bytes.clone()).ignoring_range()),
        true,
    )
    .ensure(&e, &f.models_dir, &mut no_progress)
    .expect("restarts");
    match got {
        Fetched::Downloaded {
            resumed_from,
            bytes: n,
            ..
        } => {
            assert_eq!(resumed_from, 0, "it must start over");
            assert_eq!(n, 120_000);
        }
        other => panic!("expected a full download, got {other:?}"),
    }
    assert_eq!(std::fs::read(e.local_path(&f.models_dir)).expect("read"), bytes);
}

#[test]
fn a_model_already_on_disk_is_not_fetched_again() {
    let f = Fixture::new();
    let bytes = body(80_000);
    let e = entry(&bytes);
    let http = FakeHttp::serving(bytes.clone());
    let fetcher = Fetcher::new(Box::new(http.clone()), true);

    fetcher.ensure(&e, &f.models_dir, &mut no_progress).expect("first");
    assert_eq!(http.calls().len(), 1);

    // Second time: verified from the receipt, no transfer.
    let again = fetcher.ensure(&e, &f.models_dir, &mut no_progress).expect("second");
    assert!(matches!(again, Fetched::AlreadyPresent(_)));
    assert_eq!(http.calls().len(), 1, "nothing should have been requested");
}

#[test]
fn a_model_that_was_corrupted_on_disk_is_caught_rather_than_loaded() {
    let f = Fixture::new();
    let bytes = body(80_000);
    let e = entry(&bytes);
    let fetcher = Fetcher::new(Box::new(FakeHttp::serving(bytes.clone())), true);
    fetcher.ensure(&e, &f.models_dir, &mut no_progress).expect("first");

    // Something rewrote it — a bad disk, a half-finished copy, a sync tool.
    let path = e.local_path(&f.models_dir);
    let mut corrupt = bytes.clone();
    corrupt[40_000] ^= 0xff;
    std::fs::write(&path, &corrupt).expect("corrupt");
    // The receipt records size *and* mtime, so a rewrite invalidates it and the
    // hash is checked again.
    let err = fetcher.ensure(&e, &f.models_dir, &mut no_progress).unwrap_err();
    assert!(matches!(err, MindError::HashMismatch { .. }), "{err}");
}

#[test]
fn the_url_is_never_touched_when_the_file_is_already_good() {
    let f = Fixture::new();
    let bytes = body(1024);
    let e = entry(&bytes);
    std::fs::create_dir_all(&f.models_dir).expect("dir");
    std::fs::write(e.local_path(&f.models_dir), &bytes).expect("write");

    let http = FakeHttp {
        fail_with: Some("this must never be called".into()),
        ..FakeHttp::serving(bytes.clone())
    };
    // Downloads are *off*, and it still succeeds, because there is nothing to
    // download. First run with a model already sideloaded must work offline.
    let got = Fetcher::new(Box::new(http.clone()), false)
        .ensure(&e, &f.models_dir, &mut no_progress)
        .expect("present");
    assert!(matches!(got, Fetched::AlreadyPresent(_)));
    assert!(http.calls().is_empty());
}

#[test]
fn the_shipped_registry_would_be_a_first_run_of_a_known_size() {
    let r = ModelRegistry::builtin();
    let bytes = r.first_run_bytes();
    // Not an assertion about the exact models; an assertion that the number is
    // real and that somebody has to be told about it before it is spent.
    assert!(bytes > 1_000_000_000, "{bytes}");
    assert!(bytes < 64_000_000_000, "{bytes}");
}
