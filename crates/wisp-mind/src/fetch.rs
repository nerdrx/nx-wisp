//! **F55 — fetching a pinned model.**
//!
//! The only place in this crate that opens a socket, and the rules it plays by
//! are SPEC §0.2a's:
//!
//! 1. **Pinned URL, pinned hash.** Both come from [`crate::models`]; neither is
//!    ever built from anything a model said.
//! 2. **Off unless the operator said yes.** [`Fetcher::allow`] mirrors
//!    `wisp::config::ModelSettings::allow_downloads`, which ships `false`.
//! 3. **Verify, then move.** Bytes land in `<file>.part`. They become
//!    `<file>` only after the whole thing hashes to the pin, by a `rename` in
//!    the same directory — an atomic operation. There is no window in which a
//!    partial or wrong-hashed file is sitting where a loader would find it.
//! 4. **Resumable.** A 18 GiB download that dies at 90% resumes with a `Range`
//!    request rather than starting again.
//! 5. **No telemetry.** No headers beyond `Range` and a plain `User-Agent`; no
//!    query parameters; nothing about the machine leaves.
//!
//! The transport is behind [`Http`] so the whole of the above is testable
//! offline, including the resume path and the "the mirror served us something
//! else" path.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{MindError, Result};
use crate::models::ModelEntry;

/// What a byte source has to be able to do.
pub trait Http: Send + Sync {
    /// Open a stream starting at `from`. Returns the total length of the whole
    /// resource (not of the remainder) when the server reports it, and whether
    /// the server actually honoured the range.
    fn open(&self, url: &str, from: u64) -> std::result::Result<HttpStream, String>;
}

pub struct HttpStream {
    pub body: Box<dyn Read + Send>,
    /// Total size of the complete resource, if known.
    pub total: Option<u64>,
    /// True when the server replied `206` and the stream really does start at
    /// `from`. A server that ignores `Range` and sends `200` must be handled by
    /// restarting, not by appending — that is how a corrupt file is made.
    pub resumed: bool,
}

/// Progress, for the first-run UI. Reported often enough to animate and rarely
/// enough not to be the reason the download is slow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub name: String,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub resumed_from: u64,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.done_bytes as f64 / self.total_bytes as f64) as f32
    }
}

/// What `ensure` did, so a caller can tell "already had it" from "just spent
/// twenty minutes on it".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// Present and verified, nothing was transferred.
    AlreadyPresent(PathBuf),
    Downloaded {
        path: PathBuf,
        bytes: u64,
        resumed_from: u64,
    },
}

impl Fetched {
    pub fn path(&self) -> &Path {
        match self {
            Fetched::AlreadyPresent(p) => p,
            Fetched::Downloaded { path, .. } => path,
        }
    }
}

/// How often to call the progress callback.
const PROGRESS_EVERY: u64 = 4 * 1024 * 1024;
const CHUNK: usize = 256 * 1024;

pub struct Fetcher {
    http: Box<dyn Http>,
    /// SPEC §0.2a: nothing is fetched until the operator turns this on.
    allow: bool,
}

impl Fetcher {
    pub fn new(http: Box<dyn Http>, allow: bool) -> Self {
        Fetcher { http, allow }
    }

    /// The shipping one.
    #[cfg(feature = "download")]
    pub fn real(allow: bool) -> Self {
        Fetcher::new(Box::new(ureq_http::UreqHttp::default()), allow)
    }

    pub fn allowed(&self) -> bool {
        self.allow
    }

    /// Make sure `entry` is on disk and verified, fetching it if it is not.
    pub fn ensure(
        &self,
        entry: &ModelEntry,
        models_dir: impl AsRef<Path>,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Fetched> {
        let models_dir = models_dir.as_ref();
        std::fs::create_dir_all(models_dir).map_err(|e| MindError::io(models_dir, e))?;
        let final_path = entry.local_path(models_dir);

        if let Some(p) = self.already_good(entry, &final_path)? {
            return Ok(Fetched::AlreadyPresent(p));
        }
        if !self.allow {
            return Err(MindError::DownloadsDisabled(entry.name.clone()));
        }

        let part = part_path(&final_path);
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        // More bytes than the pinned size means this `.part` is not this model.
        let have = if have > entry.size_bytes { 0 } else { have };

        let stream = self
            .http
            .open(&entry.url, have)
            .map_err(|why| MindError::Fetch {
                name: entry.name.clone(),
                why,
            })?;

        // A server that ignored the Range header hands back the whole file; if
        // we appended that to what we already had we would produce a file of
        // the right length and the wrong contents, which the hash would catch —
        // but only after another 18 GiB. Truncate instead.
        let resumed_from = if stream.resumed { have } else { 0 };
        if let Some(total) = stream.total {
            if total != entry.size_bytes {
                return Err(MindError::SizeMismatch {
                    name: entry.name.clone(),
                    want: entry.size_bytes,
                    got: total,
                });
            }
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            // Explicitly *not* truncating: a resumed download reopens the
            // `.part` to append to it, and `set_len` below is what decides how
            // much of it survives.
            .truncate(false)
            .read(true)
            .write(true)
            .open(&part)
            .map_err(|e| MindError::io(&part, e))?;
        file.set_len(resumed_from)
            .map_err(|e| MindError::io(&part, e))?;
        file.seek(SeekFrom::Start(resumed_from))
            .map_err(|e| MindError::io(&part, e))?;

        let written = self.stream_to(
            entry,
            stream.body,
            &mut file,
            resumed_from,
            &part,
            progress,
        )?;
        file.flush().map_err(|e| MindError::io(&part, e))?;
        file.sync_all().map_err(|e| MindError::io(&part, e))?;
        drop(file);

        let total = resumed_from + written;
        if total != entry.size_bytes {
            // Keep the `.part`: a short read is exactly what resume is for.
            return Err(MindError::SizeMismatch {
                name: entry.name.clone(),
                want: entry.size_bytes,
                got: total,
            });
        }

        let got = hash_file(&part)?;
        if got != entry.sha256 {
            // Wrong bytes are never left lying around to be resumed onto.
            let _ = std::fs::remove_file(&part);
            return Err(MindError::HashMismatch {
                name: entry.name.clone(),
                want: entry.sha256.clone(),
                got,
            });
        }

        // Verify, *then* move. Same directory, so this is atomic.
        std::fs::rename(&part, &final_path).map_err(|e| MindError::io(&final_path, e))?;
        write_receipt(&final_path, entry)?;
        Ok(Fetched::Downloaded {
            path: final_path,
            bytes: written,
            resumed_from,
        })
    }

    fn stream_to(
        &self,
        entry: &ModelEntry,
        mut body: Box<dyn Read + Send>,
        file: &mut std::fs::File,
        resumed_from: u64,
        part: &Path,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<u64> {
        let mut buf = vec![0u8; CHUNK];
        let mut written = 0u64;
        let mut since_report = 0u64;
        progress(Progress {
            name: entry.name.clone(),
            done_bytes: resumed_from,
            total_bytes: entry.size_bytes,
            resumed_from,
        });
        loop {
            let n = body.read(&mut buf).map_err(|e| MindError::Fetch {
                name: entry.name.clone(),
                why: e.to_string(),
            })?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])
                .map_err(|e| MindError::io(part, e))?;
            written += n as u64;
            since_report += n as u64;
            if since_report >= PROGRESS_EVERY {
                since_report = 0;
                progress(Progress {
                    name: entry.name.clone(),
                    done_bytes: resumed_from + written,
                    total_bytes: entry.size_bytes,
                    resumed_from,
                });
            }
            if resumed_from + written > entry.size_bytes {
                return Err(MindError::SizeMismatch {
                    name: entry.name.clone(),
                    want: entry.size_bytes,
                    got: resumed_from + written,
                });
            }
        }
        progress(Progress {
            name: entry.name.clone(),
            done_bytes: resumed_from + written,
            total_bytes: entry.size_bytes,
            resumed_from,
        });
        Ok(written)
    }

    /// Is the file already there and provably the right one?
    ///
    /// Re-hashing 18 GiB on every start would add half a minute to boot, so a
    /// receipt beside the file records the hash we verified along with the size
    /// and mtime we verified it at. Any of those changing means hashing again —
    /// the receipt is a cache, never a substitute for the check.
    fn already_good(&self, entry: &ModelEntry, path: &Path) -> Result<Option<PathBuf>> {
        let Ok(md) = std::fs::metadata(path) else {
            return Ok(None);
        };
        if md.len() != entry.size_bytes {
            return Ok(None);
        }
        if receipt_matches(path, entry, &md) {
            return Ok(Some(path.to_path_buf()));
        }
        let got = hash_file(path)?;
        if got != entry.sha256 {
            return Err(MindError::HashMismatch {
                name: entry.name.clone(),
                want: entry.sha256.clone(),
                got,
            });
        }
        write_receipt(path, entry)?;
        Ok(Some(path.to_path_buf()))
    }

    /// Fetch every default the registry names for the roles given, in order.
    pub fn ensure_all(
        &self,
        entries: &[&ModelEntry],
        models_dir: impl AsRef<Path>,
        progress: &mut dyn FnMut(Progress),
    ) -> Vec<(String, Result<Fetched>)> {
        entries
            .iter()
            .map(|e| {
                (
                    e.name.clone(),
                    self.ensure(e, models_dir.as_ref(), progress),
                )
            })
            .collect()
    }
}

fn part_path(final_path: &Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

fn receipt_path(final_path: &Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_os_string();
    s.push(".verified");
    PathBuf::from(s)
}

fn receipt_matches(path: &Path, entry: &ModelEntry, md: &std::fs::Metadata) -> bool {
    let Ok(text) = std::fs::read_to_string(receipt_path(path)) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let same = |k: &str, want: &str| v.get(k).and_then(|x| x.as_str()) == Some(want);
    same("sha256", &entry.sha256)
        && v.get("size").and_then(|x| x.as_u64()) == Some(md.len())
        && v.get("mtime_ns").and_then(|x| x.as_i64()) == mtime_of(md)
}

/// Nanoseconds, not seconds: a corrupted file rewritten within the same second
/// as the one we verified would otherwise still match its receipt, and the
/// receipt would have turned into a way of *not* noticing. On a filesystem with
/// only second-granularity timestamps this degrades to the weaker check, which
/// is why the receipt is a cache and `already_good` still hashes whenever it
/// does not match.
fn mtime_of(md: &std::fs::Metadata) -> Option<i64> {
    md.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos() as i64)
}

fn write_receipt(path: &Path, entry: &ModelEntry) -> Result<()> {
    let md = std::fs::metadata(path).map_err(|e| MindError::io(path, e))?;
    let body = serde_json::json!({
        "sha256": entry.sha256,
        "size": md.len(),
        "mtime_ns": mtime_of(&md),
        "name": entry.name,
    });
    // Best effort: a missing receipt only costs a re-hash.
    let _ = std::fs::write(receipt_path(path), body.to_string());
    Ok(())
}

/// SHA-256 of a whole file, streamed.
pub fn hash_file(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    let mut f = std::fs::File::open(path).map_err(|e| MindError::io(path, e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf).map_err(|e| MindError::io(path, e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// The real transport
// ---------------------------------------------------------------------------

#[cfg(feature = "download")]
mod ureq_http {
    use super::{Http, HttpStream};

    /// `ureq`, configured to say as little about this machine as possible.
    pub struct UreqHttp {
        agent: ureq::Agent,
        /// Same request, IPv4 only — see `call`.
        v4: ureq::Agent,
        /// Sticky: once IPv6 has proven dead, an 18 GiB download must not pay
        /// a failed connect per resumed chunk.
        v6_is_dead: std::sync::atomic::AtomicBool,
    }

    impl Default for UreqHttp {
        fn default() -> Self {
            let base = || {
                ureq::Agent::config_builder()
                    .timeout_connect(Some(std::time::Duration::from_secs(20)))
                    // No global timeout: an 18 GiB download over a slow link is
                    // not a hung request. Progress stalling is the caller's
                    // problem.
                    .user_agent(concat!("nx-wisp/", env!("CARGO_PKG_VERSION")))
            };
            UreqHttp {
                agent: base().build().into(),
                v4: base()
                    .ip_family(ureq::config::IpFamily::Ipv4Only)
                    .build()
                    .into(),
                v6_is_dead: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    /// `ENETUNREACH` / `EHOSTUNREACH`, as `ureq` reports them. This machine has
    /// an IPv6 default route learned from a router advertisement and no working
    /// IPv6 path; `ureq` has no happy-eyeballs, so every fetch died with
    /// "Network is unreachable (os error 101)" while `curl` worked. wisp-voice's
    /// fetcher learned this first (its models downloaded fine); this one shipped
    /// without the lesson and the operator hit it within the hour — the voice
    /// rows said "already here, hash verified" while every model row said
    /// "Network is unreachable", which is this exact asymmetry.
    fn unreachable(why: &str) -> bool {
        let w = why.to_ascii_lowercase();
        w.contains("network is unreachable")
            || w.contains("no route to host")
            || w.contains("host is unreachable")
            || w.contains("address family not supported")
    }

    impl Http for UreqHttp {
        fn open(&self, url: &str, from: u64) -> Result<HttpStream, String> {
            // Belt and braces: `ModelRegistry::validated` already refused
            // anything that is not https, but this is the function that would
            // do the talking.
            if !url.starts_with("https://") {
                return Err(format!("refusing to fetch over plain HTTP: {url}"));
            }
            use std::sync::atomic::Ordering;
            let send = |agent: &ureq::Agent| {
                let mut req = agent.get(url);
                if from > 0 {
                    req = req.header("Range", &format!("bytes={from}-"));
                }
                req.call()
            };
            let resp = if self.v6_is_dead.load(Ordering::Relaxed) {
                send(&self.v4).map_err(|e| e.to_string())?
            } else {
                match send(&self.agent) {
                    Ok(r) => r,
                    Err(e) => {
                        let why = e.to_string();
                        if !unreachable(&why) {
                            return Err(why);
                        }
                        tracing::info!(url, %why, "no route on the preferred family; retrying over IPv4");
                        self.v6_is_dead.store(true, Ordering::Relaxed);
                        send(&self.v4)
                            .map_err(|e2| format!("{why}; and over IPv4: {e2}"))?
                    }
                }
            };
            let status = resp.status().as_u16();
            let resumed = status == 206;
            if from > 0 && !resumed {
                tracing::warn!(url, "server ignored Range; restarting the download");
            }
            let total = content_total(&resp, resumed);
            Ok(HttpStream {
                total,
                resumed,
                body: Box::new(resp.into_body().into_reader()),
            })
        }
    }

    /// The size of the *whole* resource. For a 206 that is the tail of
    /// `Content-Range`, not `Content-Length`.
    fn content_total<T>(resp: &ureq::http::Response<T>, resumed: bool) -> Option<u64> {
        let h = resp.headers();
        if resumed {
            let cr = h.get("content-range")?.to_str().ok()?;
            return cr.rsplit('/').next()?.trim().parse().ok();
        }
        h.get("content-length")?.to_str().ok()?.parse().ok()
    }
}
