//! The pinned manifest, and the store that installs from it.
//!
//! SPEC §0.2a: *no network egress except (a) model downloads from pinned URLs
//! with pinned hashes*. This module is the whole of clause (a). Every artefact
//! she can ever fetch is a `const` in [`MANIFEST`] with an immutable-revision
//! URL, a sha256 and a byte length; there is no code path that downloads
//! anything that is not in that table, and no code path that installs a file
//! whose hash does not match the one compiled into the binary.
//!
//! ## Why a table of consts rather than a config file
//!
//! A manifest read from disk is a manifest an attacker can edit, and a manifest
//! read from the network is the thing the pin exists to avoid. Making it a
//! `const` means changing what she is allowed to download is a code change that
//! shows up in a diff and a release, which is exactly the visibility a "she
//! phones home" claim needs to be checkable against.
//!
//! It also means the *length* is pinned, not just the hash, and that turns out
//! to matter more than it sounds. Knowing the length up front is what lets
//! [`ModelStore::ensure`] tell an interrupted download apart from a corrupt one
//! without hashing: short is resumable, long is junk, exact is worth hashing.
//! A store that only knew the hash would have to hash a half-downloaded 300 MB
//! file to discover it was half-downloaded, and would then have no honest
//! reason not to throw it away.
//!
//! ## Verify, then move
//!
//! Downloads land in `<file>.part` and are renamed into place only after the
//! completed part hashes to the pin. Nothing else in this crate looks at
//! `.part` files, so an interrupted download — a `SIGKILL`, a closed lid, a
//! full disk — can never be mistaken for an installed model. The failure mode
//! that ordering rules out is the nasty one: a truncated ONNX file that loads,
//! initialises, and then produces noise.
//!
//! A hash mismatch is a **refusal**, not a retry. The `.part` is deleted and
//! [`crate::VoiceError::ModelCorrupt`] comes back; nothing loops. If the bytes
//! at a pinned, immutable URL are not the bytes we pinned, the interesting
//! possibilities are "the CDN is lying to you" and "the manifest is wrong", and
//! neither is improved by asking again.
//!
//! ## Where it writes
//!
//! [`crate::data_dir`]`().join("models")`, which honours `NX_WISP_CONFIG_DIR`.
//! Never the repository, never `target/`. A test that forgets to set the
//! override would write a 300 MB file into the operator's real store, so the
//! suite uses [`ModelStore::at`] with a `tempfile::TempDir` and the tests that
//! exercise [`ModelStore::open`] set the environment variable first.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fetch::Fetcher;
use crate::{Result, VoiceError};

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

/// What an artefact is *for*, which decides who has to have it.
///
/// Not a file-type tag — the Piper voice and the Kokoro voice are both `.onnx`
/// and are both [`ModelKind::TtsVoice`]. The distinction that matters to a
/// caller is "is this thing a voice the operator can pick", and F35's voice
/// packs are built out of exactly that answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelKind {
    /// A synthesis model. One of these plus its config is a usable voice.
    TtsVoice,
    /// The JSON that tells the engine the model's sample rate, phoneme table
    /// and speaker map. Useless alone, and the model is useless without it.
    TtsConfig,
    /// A speech-recognition model.
    SttModel,
    /// A speaker embedding — Kokoro's style vectors. Small, and one per voice.
    VoiceData,
}

/// One pinned artefact.
///
/// Everything is `&'static str` because everything here is compiled in. That
/// also makes the type cheap enough for a voice pack to hold a slice of these
/// by reference, and it makes a test able to construct one from literals
/// without needing to be in the real manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelEntry {
    /// Stable id. Used by voice packs, the cost meter and the flight recorder,
    /// so it outlives any particular URL.
    pub id: &'static str,
    /// Filename inside the store. Flat — no directories, because the id already
    /// namespaces and a nested layout only creates ways to half-create a tree.
    pub file: &'static str,
    /// A pinned, immutable revision. Never a branch: `main` moves, and a pin to
    /// something that moves is not a pin.
    pub url: &'static str,
    /// Lowercase hex sha256 of the whole file.
    pub sha256: &'static str,
    pub bytes: u64,
    pub kind: ModelKind,
    /// SPDX-ish, for the about box. Model weights carry licences and the
    /// operator is entitled to see them without reading this file.
    pub license: &'static str,
}

impl ModelEntry {
    /// Human-sized, for the "this will cost you 300 MB" prompt.
    pub fn mib(&self) -> f64 {
        self.bytes as f64 / (1024.0 * 1024.0)
    }
}

/// The HuggingFace revisions everything below is pinned to.
///
/// Public because the about box and a bug report both want to say *which*
/// revision she is pinned to, and named rather than inlined so that a manifest
/// bump is one obvious line per repository. They are not interpolated into the
/// URLs below — a `const` URL cannot be `format!`ed — so a test asserts that
/// every URL agrees with the constant for its repository, which is what catches
/// a half-finished bump.
///
/// Resolved **2026-08-23** from `https://huggingface.co/api/models/<repo>`.
pub const PIPER_REV: &str = "f5a6e9094787fd865d65cb024472f977f9c542b5";
/// See [`PIPER_REV`].
pub const KOKORO_REV: &str = "1939ad2a8e416c0acfeecc08a694d14ef25f2231";
/// See [`PIPER_REV`].
pub const WHISPER_REV: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";

/// Every artefact she is allowed to download. Nothing else, ever.
///
/// ## Piper — the default engine
///
/// Two medium-quality VITS voices, `en_US-amy-medium` and `en_GB-alba-medium`,
/// so that F35's voice packs are a list with more than one thing in it and the
/// switching path gets exercised for real. Both are 22.05 kHz, both are ~63 MB,
/// and each is two files: the ONNX graph and the `.onnx.json` that carries the
/// phoneme table and sample rate. Splitting them into two manifest rows rather
/// than one row with two URLs keeps the resume logic operating on one file at a
/// time, which is the only shape the `.part` protocol has.
///
/// The two configs are a few KB and are **not** LFS objects in the piper-voices
/// repository, so their hashes are not in the tree API's `lfs.oid` field. They
/// were downloaded at the pinned revision and hashed locally.
///
/// ## Kokoro — the quality pack, off by the T2 downgrade
///
/// `onnx-community/Kokoro-82M-v1.0-ONNX` publishes eight variants of the same
/// graph. The pick here is **`model_quantized.onnx`** — int8 dynamic
/// quantisation, 88 MB against the fp32 model's 310 MB. The fp16 and `*f16`
/// variants are smaller still but are the wrong trade on this machine: with no
/// ROCm there is no GPU execution provider, everything runs on the CPU, and
/// fp16 kernels on a CPU are emulated rather than fast. int8 is the variant
/// that is both smaller *and* quicker where this actually runs.
///
/// That repository ships one `.bin` style vector per voice rather than a single
/// combined voices file, so a Kokoro voice is a second manifest row next to the
/// model. `af_heart` is pinned as the default; more voices are more rows.
///
/// ## whisper.cpp — STT
///
/// `tiny.en` and `base.en` in GGML form. English-only because F28's push-to-talk
/// is for talking to her, and the multilingual models are twice the size for a
/// capability nobody asked for yet. `tiny.en` is the T2 fallback and `base.en`
/// the default; anything larger stops being a sensible thing to hold resident
/// next to a game.
pub const MANIFEST: &[ModelEntry] = &[
    ModelEntry {
        id: "piper-en_US-amy-medium",
        file: "en_US-amy-medium.onnx",
        url: "https://huggingface.co/rhasspy/piper-voices/resolve/f5a6e9094787fd865d65cb024472f977f9c542b5/en/en_US/amy/medium/en_US-amy-medium.onnx",
        sha256: "b3a6e47b57b8c7fbe6a0ce2518161a50f59a9cdd8a50835c02cb02bdd6206c18",
        bytes: 63_201_294,
        kind: ModelKind::TtsVoice,
        license: "MIT",
    },
    ModelEntry {
        id: "piper-en_US-amy-medium-config",
        file: "en_US-amy-medium.onnx.json",
        url: "https://huggingface.co/rhasspy/piper-voices/resolve/f5a6e9094787fd865d65cb024472f977f9c542b5/en/en_US/amy/medium/en_US-amy-medium.onnx.json",
        sha256: "95a23eb4d42909d38df73bb9ac7f45f597dbfcde2d1bf9526fdeaf5466977d77",
        bytes: 4_882,
        kind: ModelKind::TtsConfig,
        license: "MIT",
    },
    ModelEntry {
        id: "piper-en_GB-alba-medium",
        file: "en_GB-alba-medium.onnx",
        url: "https://huggingface.co/rhasspy/piper-voices/resolve/f5a6e9094787fd865d65cb024472f977f9c542b5/en/en_GB/alba/medium/en_GB-alba-medium.onnx",
        sha256: "401369c4a81d09fdd86c32c5c864440811dbdcc66466cde2d64f7133a66ad03b",
        bytes: 63_201_294,
        kind: ModelKind::TtsVoice,
        license: "MIT",
    },
    ModelEntry {
        id: "piper-en_GB-alba-medium-config",
        file: "en_GB-alba-medium.onnx.json",
        url: "https://huggingface.co/rhasspy/piper-voices/resolve/f5a6e9094787fd865d65cb024472f977f9c542b5/en/en_GB/alba/medium/en_GB-alba-medium.onnx.json",
        sha256: "aa965a2f02ecced632c2694e1fc72bbff6d65f265fab567ca945918c73dd89f4",
        bytes: 4_888,
        kind: ModelKind::TtsConfig,
        license: "MIT",
    },
    ModelEntry {
        id: "kokoro-82m-v1.0-q8",
        file: "kokoro-82m-v1.0-quantized.onnx",
        url: "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/1939ad2a8e416c0acfeecc08a694d14ef25f2231/onnx/model_quantized.onnx",
        sha256: "fbae9257e1e05ffc727e951ef9b9c98418e6d79f1c9b6b13bd59f5c9028a1478",
        bytes: 92_361_116,
        kind: ModelKind::TtsVoice,
        license: "Apache-2.0",
    },
    ModelEntry {
        id: "kokoro-voice-af_heart",
        file: "kokoro-af_heart.bin",
        url: "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/1939ad2a8e416c0acfeecc08a694d14ef25f2231/voices/af_heart.bin",
        sha256: "d583ccff3cdca2f7fae535cb998ac07e9fcb90f09737b9a41fa2734ec44a8f0b",
        bytes: 522_240,
        kind: ModelKind::VoiceData,
        license: "Apache-2.0",
    },
    ModelEntry {
        id: "whisper-tiny.en",
        file: "ggml-tiny.en.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-tiny.en.bin",
        sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
        bytes: 77_704_715,
        kind: ModelKind::SttModel,
        license: "MIT",
    },
    ModelEntry {
        id: "whisper-base.en",
        file: "ggml-base.en.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-base.en.bin",
        sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
        bytes: 147_964_211,
        kind: ModelKind::SttModel,
        license: "MIT",
    },
];

/// Look an artefact up by id. `None` for anything not pinned — which is the
/// only answer available, since an unpinned artefact cannot be fetched.
pub fn entry(id: &str) -> Option<&'static ModelEntry> {
    MANIFEST.iter().find(|e| e.id == id)
}

/// Everything of one kind, for a UI that wants to list the installable voices.
pub fn of_kind(kind: ModelKind) -> impl Iterator<Item = &'static ModelEntry> {
    MANIFEST.iter().filter(move |e| e.kind == kind)
}

// ---------------------------------------------------------------------------
// Progress and planning
// ---------------------------------------------------------------------------

/// How far a download has got.
///
/// Reported at least at the start and at the end of every [`ModelStore::ensure`]
/// that touches the network, and roughly every [`PROGRESS_STRIDE`] bytes in
/// between. Not per byte, and not on a timer: per byte would make the callback
/// the bottleneck on a fast link, and a timer would make the tests depend on a
/// clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub id: &'static str,
    pub done: u64,
    /// The whole resource, from the server if it said and from the pin
    /// otherwise. In practice always `Some`, because the pin always knows.
    pub total: Option<u64>,
    /// This transfer picked up where a previous one stopped.
    pub resumed: bool,
}

impl Progress {
    /// `0.0..=1.0`, or `None` when nothing knows the total.
    pub fn fraction(&self) -> Option<f64> {
        match self.total {
            Some(t) if t > 0 => Some((self.done as f64 / t as f64).clamp(0.0, 1.0)),
            _ => None,
        }
    }
}

/// How often progress is reported: once per mebibyte of body.
pub const PROGRESS_STRIDE: u64 = 1024 * 1024;

/// Read size. Big enough that the syscall is not the cost, small enough that a
/// shed download does not sit in a `read` for a noticeable time.
const CHUNK: usize = 64 * 1024;

/// What installing a set of artefacts would cost.
///
/// The point of this type is the question a UI has to ask *before* it starts:
/// "she needs 300 MB, is that alright?" Answering it must not itself be
/// expensive, so this is a presence check — it does not hash anything. A file
/// that is present but corrupt counts as installed here and is caught by
/// [`ModelStore::ensure`] when it actually matters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Not installed, in manifest order.
    pub missing: Vec<&'static ModelEntry>,
    /// Total size of everything in `missing`.
    pub bytes: u64,
    /// Of that, how much is already sitting in `.part` files from an earlier
    /// attempt and will not be fetched again.
    pub already: u64,
}

impl Plan {
    /// What actually has to come down the wire.
    pub fn remaining(&self) -> u64 {
        self.bytes.saturating_sub(self.already)
    }

    pub fn is_empty(&self) -> bool {
        self.missing.is_empty()
    }

    pub fn remaining_mib(&self) -> f64 {
        self.remaining() as f64 / (1024.0 * 1024.0)
    }
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// A directory of installed artefacts.
///
/// Cheap to construct and holds no handles, so a caller can make one per
/// operation rather than threading it around.
#[derive(Debug, Clone)]
pub struct ModelStore {
    root: PathBuf,
}

impl ModelStore {
    /// The real store: `data_dir()/models`, honouring `NX_WISP_CONFIG_DIR`.
    pub fn open() -> Self {
        ModelStore::at(crate::data_dir().join("models"))
    }

    /// A store somewhere else. What the tests use, with a `TempDir`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        ModelStore {
            root: root.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn entry(&self, id: &str) -> Option<&'static ModelEntry> {
        entry(id)
    }

    /// Where a pinned artefact lives once installed. `None` for an unknown id.
    pub fn path(&self, id: &str) -> Option<PathBuf> {
        entry(id).map(|e| self.path_of(e))
    }

    /// Where an artefact lives, for entries that need not be in [`MANIFEST`] —
    /// the tests, and eventually a voice pack that ships its own table.
    pub fn path_of(&self, e: &ModelEntry) -> PathBuf {
        self.root.join(e.file)
    }

    /// The download-in-progress file. Deliberately `<file>.part` and not a
    /// hidden name: an operator who finds one should be able to guess what it
    /// is and delete it.
    pub fn part_of(&self, e: &ModelEntry) -> PathBuf {
        self.root.join(format!("{}.part", e.file))
    }

    /// Is it installed? Presence only, and **only** of the final name — a
    /// `.part` is not a model.
    pub fn have(&self, id: &str) -> bool {
        entry(id).is_some_and(|e| self.have_entry(e))
    }

    pub fn have_entry(&self, e: &ModelEntry) -> bool {
        self.path_of(e).is_file()
    }

    /// Hash what is on disk against the pin.
    ///
    /// A mismatch is [`VoiceError::ModelCorrupt`] and stays that way: this
    /// function never deletes and never re-fetches. Repairing in place would
    /// mean a store that silently heals itself, and "it works now" is the worst
    /// possible answer to "why did the hash not match".
    pub fn verify(&self, id: &str) -> Result<()> {
        let e = entry(id).ok_or_else(|| VoiceError::ModelMissing(id.to_string()))?;
        self.verify_entry(e)
    }

    pub fn verify_entry(&self, e: &ModelEntry) -> Result<()> {
        let path = self.path_of(e);
        let meta = std::fs::metadata(&path)
            .map_err(|_| VoiceError::ModelMissing(e.id.to_string()))?;

        if self.stamp_vouches_for(e, &meta) {
            return Ok(());
        }

        let got = sha256_file(&path)?;
        if got != e.sha256 {
            return Err(VoiceError::ModelCorrupt {
                id: e.id.to_string(),
                want: e.sha256,
                got,
            });
        }
        self.stamp(e, &meta);
        Ok(())
    }

    /// What it would cost to install these ids. Unknown ids are skipped rather
    /// than reported: they cannot be downloaded, so they cannot be part of a
    /// download plan.
    pub fn plan<'a, I>(&self, ids: I) -> Plan
    where
        I: IntoIterator<Item = &'a str>,
    {
        let wanted: Vec<&'static ModelEntry> = ids.into_iter().filter_map(entry).collect();
        self.plan_entries(wanted)
    }

    /// What it would cost to install everything pinned.
    pub fn plan_all(&self) -> Plan {
        self.plan_entries(MANIFEST.iter())
    }

    fn plan_entries<I>(&self, entries: I) -> Plan
    where
        I: IntoIterator<Item = &'static ModelEntry>,
    {
        let mut plan = Plan::default();
        for e in entries {
            if self.have_entry(e) {
                continue;
            }
            plan.missing.push(e);
            plan.bytes += e.bytes;
            plan.already += self.part_len(e).min(e.bytes);
        }
        plan
    }

    /// Install `id` if it is not already installed and verified.
    ///
    /// The whole contract, in order:
    ///
    /// 1. present and verified → return, without one byte of network;
    /// 2. present and *not* verified → [`VoiceError::ModelCorrupt`], and the
    ///    file is left exactly where it is for the operator to look at;
    /// 3. otherwise download into `<file>.part`, resuming from whatever length
    ///    that file already has;
    /// 4. short → [`VoiceError::Fetch`], `.part` kept so the next call resumes;
    /// 5. exact length but wrong hash → `.part` deleted,
    ///    [`VoiceError::ModelCorrupt`], and **no file appears in the store**;
    /// 6. exact length and right hash → rename into place.
    ///
    /// One attempt. Retrying is the caller's decision, because the caller is
    /// the one that knows whether the operator is still watching.
    pub fn ensure(
        &self,
        id: &str,
        net: &mut dyn Fetcher,
        on: &mut dyn FnMut(Progress),
    ) -> Result<PathBuf> {
        let e = entry(id).ok_or_else(|| VoiceError::ModelMissing(id.to_string()))?;
        self.ensure_entry(e, net, on)
    }

    pub fn ensure_entry(
        &self,
        e: &ModelEntry,
        net: &mut dyn Fetcher,
        on: &mut dyn FnMut(Progress),
    ) -> Result<PathBuf> {
        let final_path = self.path_of(e);
        if final_path.is_file() {
            self.verify_entry(e)?;
            return Ok(final_path);
        }

        // Safe on a store that has never existed. Doing this before the fetch
        // rather than before the first write means a missing directory fails
        // before a socket is opened rather than after 300 MB.
        std::fs::create_dir_all(&self.root)
            .map_err(|err| VoiceError::io(format!("creating {}", self.root.display()), err))?;

        let part = self.part_of(e);
        // A part longer than the pin cannot become right by adding bytes to it,
        // so do not offer it as a resume point.
        let mut done = self.part_len(e);
        if done > e.bytes {
            done = 0;
        }

        let fetched = net.get(e.url, done)?;

        // The server volunteered a length that disagrees with the pin: the URL
        // is serving something other than what was pinned. Nothing has been
        // written yet, and nothing will be.
        if let Some(total) = fetched.total {
            if total != e.bytes {
                return Err(VoiceError::Fetch {
                    url: e.url.to_string(),
                    why: format!("resource is {total} bytes, manifest pins {}", e.bytes),
                });
            }
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&part)
            .map_err(|err| VoiceError::io(format!("opening {}", part.display()), err))?;

        // The whole reason `resumed` exists. A server that ignored the range is
        // about to send the resource from byte zero, so the partial file has to
        // go — appending would produce a file of the right length made of the
        // wrong bytes, which is worse than not resuming at all.
        let resumed = fetched.resumed && done > 0;
        if !resumed {
            done = 0;
        }
        file.set_len(done)
            .map_err(|err| VoiceError::io(format!("truncating {}", part.display()), err))?;
        file.seek(SeekFrom::Start(done))
            .map_err(|err| VoiceError::io(format!("seeking {}", part.display()), err))?;

        let total = fetched.total.or(Some(e.bytes));
        let mut report = |done: u64| {
            on(Progress {
                id: e.id,
                done,
                total,
                resumed,
            })
        };
        report(done);

        let mut body = fetched.body;
        let mut buf = vec![0u8; CHUNK];
        let mut since = 0u64;
        let mut died: Option<String> = None;
        loop {
            match body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    file.write_all(&buf[..n]).map_err(|err| {
                        VoiceError::io(format!("writing {}", part.display()), err)
                    })?;
                    done += n as u64;
                    since += n as u64;
                    if since >= PROGRESS_STRIDE {
                        since = 0;
                        report(done);
                    }
                    // Stop rather than let a server that lied about its length
                    // fill the disk. The length check below turns this into a
                    // refusal.
                    if done > e.bytes {
                        break;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    died = Some(err.to_string());
                    break;
                }
            }
        }

        // Durability before the length check, so the bytes a resume will trust
        // are really on the platter and not in a cache that a power cut owns.
        file.sync_all()
            .map_err(|err| VoiceError::io(format!("flushing {}", part.display()), err))?;
        drop(file);
        report(done);

        if done < e.bytes {
            // Kept on purpose: this is the resume point.
            let why = match died {
                Some(err) => format!("connection ended after {done} of {} bytes: {err}", e.bytes),
                None => format!("connection ended after {done} of {} bytes", e.bytes),
            };
            tracing::info!(id = e.id, done, want = e.bytes, "download interrupted");
            return Err(VoiceError::Fetch {
                url: e.url.to_string(),
                why,
            });
        }

        if done > e.bytes {
            let _ = std::fs::remove_file(&part);
            return Err(VoiceError::Fetch {
                url: e.url.to_string(),
                why: format!("server sent more than the pinned {} bytes", e.bytes),
            });
        }

        let got = sha256_file(&part)?;
        if got != e.sha256 {
            // Deleted, not kept: a complete file with the wrong hash has no
            // resume point, and leaving it would give the next run something
            // to "resume" from that can never be right.
            let _ = std::fs::remove_file(&part);
            tracing::warn!(id = e.id, want = e.sha256, got = %got, "pinned hash did not match");
            return Err(VoiceError::ModelCorrupt {
                id: e.id.to_string(),
                want: e.sha256,
                got,
            });
        }

        std::fs::rename(&part, &final_path).map_err(|err| {
            VoiceError::io(
                format!("installing {} as {}", part.display(), final_path.display()),
                err,
            )
        })?;
        if let Ok(meta) = std::fs::metadata(&final_path) {
            self.stamp(e, &meta);
        }
        tracing::info!(id = e.id, path = %final_path.display(), "model installed");
        Ok(final_path)
    }

    /// [`ModelStore::ensure`] over the real network, when this build has one.
    ///
    /// Split out so that nothing else in the crate needs a `cfg`: the signature
    /// is identical either way, and a build without `net` simply reports the
    /// model as missing — which it is, and which is exactly what a caller
    /// already has to handle.
    #[cfg(feature = "net")]
    pub fn ensure_online(&self, id: &str, on: &mut dyn FnMut(Progress)) -> Result<PathBuf> {
        let mut net = crate::fetch::HttpFetcher::new();
        self.ensure(id, &mut net, on)
    }

    #[cfg(not(feature = "net"))]
    pub fn ensure_online(&self, id: &str, _on: &mut dyn FnMut(Progress)) -> Result<PathBuf> {
        // Not `NotCompiled`: from the caller's side the observable fact is that
        // the model is not there and this build cannot get it, and every call
        // site already has to cope with a store that has nothing in it.
        tracing::warn!(id, "this build has no network feature; cannot install");
        Err(VoiceError::ModelMissing(id.to_string()))
    }

    fn part_len(&self, e: &ModelEntry) -> u64 {
        std::fs::metadata(self.part_of(e)).map(|m| m.len()).unwrap_or(0)
    }

    // -- the verified-stamp cache -------------------------------------------

    fn stamp_path(&self) -> PathBuf {
        self.root.join(".verified.json")
    }

    /// Does the cache let us skip hashing this file?
    ///
    /// Only when the recorded size, mtime **and** hash all agree, and the
    /// recorded hash is the one the manifest pins. Every other outcome —
    /// missing file, unparseable JSON, an entry from an older manifest — is
    /// "hash it". The cache can only ever save work, never grant a pass.
    fn stamp_vouches_for(&self, e: &ModelEntry, meta: &std::fs::Metadata) -> bool {
        let Some(now) = Stamp::of(e, meta) else {
            return false;
        };
        self.stamps()
            .get(e.id)
            .is_some_and(|s| *s == now && s.sha256 == e.sha256)
    }

    fn stamps(&self) -> BTreeMap<String, Stamp> {
        std::fs::read(self.stamp_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Best effort. A store on a read-only mount still works, it just hashes
    /// every time, so none of this is allowed to turn into an error.
    fn stamp(&self, e: &ModelEntry, meta: &std::fs::Metadata) {
        let Some(stamp) = Stamp::of(e, meta) else {
            return;
        };
        let mut all = self.stamps();
        all.insert(e.id.to_string(), stamp);
        let Ok(json) = serde_json::to_vec_pretty(&all) else {
            return;
        };
        // Write-then-rename, so a crash mid-write leaves the old cache rather
        // than half a JSON document that then fails to parse forever.
        let tmp = self.root.join(".verified.json.tmp");
        if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, self.stamp_path()).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// What a file looked like when it last hashed correctly.
///
/// Size and mtime, because those are what change when something edits the file
/// underneath us, and they are free to read. Not an inode: a store that has
/// been copied between filesystems should re-hash, and it will.
///
/// The honest limit of this: an overwrite of exactly the same length, within
/// the same filesystem timestamp tick, is invisible to it. That is a race an
/// attacker with write access to the store already wins more easily by other
/// means, and the cost of closing it is hashing 300 MB on every start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    len: u64,
    /// Nanoseconds since the epoch. `u64` runs out in 2554.
    mtime_ns: u64,
    sha256: String,
}

impl Stamp {
    /// `None` when the filesystem will not tell us an mtime, in which case
    /// there is nothing to key a cache to and we must keep hashing.
    fn of(e: &ModelEntry, meta: &std::fs::Metadata) -> Option<Stamp> {
        let mtime = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some(Stamp {
            len: meta.len(),
            mtime_ns: mtime.as_nanos().try_into().ok()?,
            sha256: e.sha256.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

/// sha256 of a file, lowercase hex. Streamed — a 300 MB model must never be a
/// 300 MB allocation.
pub fn sha256_file(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    let mut f = std::fs::File::open(path)
        .map_err(|e| VoiceError::io(format!("opening {}", path.display()), e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| VoiceError::io(format!("reading {}", path.display()), e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// sha256 of a slice, lowercase hex. Mostly so a test can pin the blob it just
/// invented without a second implementation of hex.
pub fn sha256_of(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FakeNet;

    /// A blob that is bigger than one read but smaller than one progress
    /// stride, so the download loop really iterates.
    fn blob(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    /// An entry for a made-up artefact. Everything is `&'static str`, so a test
    /// can pin its own blob without touching `MANIFEST`.
    fn test_entry(sha: &'static str, bytes: u64) -> ModelEntry {
        ModelEntry {
            id: "test-blob",
            file: "test-blob.bin",
            url: "https://example.invalid/test-blob.bin",
            sha256: sha,
            bytes,
            kind: ModelKind::VoiceData,
            license: "CC0-1.0",
        }
    }

    /// 200 KiB: three progress reports at most, and enough to make a resume
    /// offset a number rather than a rounding error.
    const N: usize = 200 * 1024;

    /// The sha of `blob(N)`, computed once so the entry can be `&'static`.
    fn fixture() -> (Vec<u8>, ModelEntry) {
        let bytes = blob(N);
        // Leaked so it can be `&'static str`, which is what a real manifest
        // entry is. One small leak per test process is not a leak worth caring
        // about, and the alternative is making `ModelEntry` generic over a
        // lifetime for the sake of the tests.
        let sha: &'static str = Box::leak(sha256_of(&bytes).into_boxed_str());
        let e = test_entry(sha, bytes.len() as u64);
        (bytes, e)
    }

    fn store() -> (tempfile::TempDir, ModelStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = ModelStore::at(tmp.path().join("models"));
        (tmp, store)
    }

    fn quiet() -> impl FnMut(Progress) {
        |_| {}
    }

    // -- the manifest itself -------------------------------------------------

    #[test]
    fn every_pinned_artefact_has_a_lowercase_sha256_a_size_and_an_https_url() {
        for e in MANIFEST {
            assert_eq!(e.sha256.len(), 64, "{}: sha256 is not 64 chars", e.id);
            assert!(
                e.sha256
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{}: sha256 is not lowercase hex: {}",
                e.id,
                e.sha256
            );
            assert!(e.bytes > 0, "{}: pinned at zero bytes", e.id);
            assert!(e.url.starts_with("https://"), "{}: {}", e.id, e.url);
            assert!(!e.file.is_empty() && !e.file.contains('/'), "{}", e.id);
            assert!(!e.license.is_empty(), "{}: no licence", e.id);
        }
    }

    /// The rule the whole module exists for: a pin to a branch is not a pin,
    /// because the branch moves and the hash then stops matching for everyone.
    #[test]
    fn every_url_is_pinned_to_an_immutable_revision_and_not_to_a_branch() {
        for e in MANIFEST {
            let rev = e
                .url
                .split("/resolve/")
                .nth(1)
                .and_then(|rest| rest.split('/').next())
                .unwrap_or_else(|| panic!("{}: no /resolve/<rev>/ in {}", e.id, e.url));
            assert_ne!(rev, "main", "{}: pinned to a branch", e.id);
            assert_eq!(rev.len(), 40, "{}: {rev} is not a commit sha", e.id);
            assert!(
                rev.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{}: {rev} is not hex",
                e.id
            );
        }
    }

    /// A bump that updates the model URL and forgets the config URL leaves two
    /// files from two revisions in the same store, which is the sort of thing
    /// that only shows up as a phoneme table that does not match its model.
    #[test]
    fn a_manifest_bump_moves_every_url_of_a_repository_at_once() {
        for e in MANIFEST {
            let want = if e.url.contains("/rhasspy/piper-voices/") {
                PIPER_REV
            } else if e.url.contains("/Kokoro-82M-v1.0-ONNX/") {
                KOKORO_REV
            } else if e.url.contains("/whisper.cpp/") {
                WHISPER_REV
            } else {
                panic!("{}: {} is from a repository nothing pins", e.id, e.url)
            };
            assert!(
                e.url.contains(&format!("/resolve/{want}/")),
                "{}: half-finished manifest bump, {} is not at {want}",
                e.id,
                e.url
            );
        }
    }

    #[test]
    fn manifest_ids_and_filenames_are_unique() {
        let mut ids: Vec<&str> = MANIFEST.iter().map(|e| e.id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate id in MANIFEST");

        let mut files: Vec<&str> = MANIFEST.iter().map(|e| e.file).collect();
        files.sort_unstable();
        files.dedup();
        assert_eq!(files.len(), n, "two entries would install over each other");
    }

    #[test]
    fn every_piper_voice_is_pinned_together_with_its_config() {
        // A voice without its `.onnx.json` is a model that cannot be loaded, so
        // the two halves have to arrive in the manifest together or not at all.
        for e in of_kind(ModelKind::TtsVoice).filter(|e| e.id.starts_with("piper-")) {
            let config = format!("{}-config", e.id);
            assert!(entry(&config).is_some(), "{} has no pinned config", e.id);
        }
    }

    #[test]
    fn there_is_more_than_one_voice_so_switching_is_a_real_code_path() {
        assert!(of_kind(ModelKind::TtsVoice).count() >= 2);
        assert!(of_kind(ModelKind::SttModel).count() >= 2);
    }

    #[test]
    fn an_unpinned_id_is_simply_not_there() {
        assert!(entry("piper-en_US-amy-medium").is_some());
        assert!(entry("whatever-i-felt-like-downloading").is_none());
    }

    // -- the store -----------------------------------------------------------

    #[test]
    fn a_clean_download_verifies_and_lands_at_the_installed_path() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let mut net = FakeNet::new();
        net.serve(e.url, bytes.clone());

        let path = store.ensure_entry(&e, &mut net, &mut quiet()).unwrap();
        assert_eq!(path, store.path_of(&e));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(store.have_entry(&e));
        assert!(!store.part_of(&e).exists(), "the .part must be gone");
        store.verify_entry(&e).unwrap();
    }

    #[test]
    fn a_store_directory_that_does_not_exist_yet_is_created_rather_than_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ModelStore::at(tmp.path().join("deep").join("nested").join("models"));
        assert!(!store.root().exists());
        let (bytes, e) = fixture();
        let mut net = FakeNet::new();
        net.serve(e.url, bytes);
        store.ensure_entry(&e, &mut net, &mut quiet()).unwrap();
        assert!(store.path_of(&e).is_file());
    }

    #[test]
    fn an_interrupted_download_resumes_from_where_it_stopped() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let cut = 70_000u64;
        let mut net = FakeNet::new();
        net.serve_truncated(e.url, bytes.clone(), cut, 1);

        let first = store.ensure_entry(&e, &mut net, &mut quiet());
        assert!(matches!(first, Err(VoiceError::Fetch { .. })), "{first:?}");
        assert!(!store.have_entry(&e), "a stopped download is not a model");
        assert_eq!(
            std::fs::metadata(store.part_of(&e)).unwrap().len(),
            cut,
            "the partial file is the resume point"
        );

        let path = store.ensure_entry(&e, &mut net, &mut quiet()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            net.offsets(e.url),
            vec![0, cut],
            "the second request must ask for the tail, not the whole thing"
        );
    }

    #[test]
    fn a_resume_re_fetches_only_the_missing_bytes() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let cut = 70_000u64;
        let mut net = FakeNet::new();
        net.serve_truncated(e.url, bytes.clone(), cut, 1);
        let _ = store.ensure_entry(&e, &mut net, &mut quiet());

        // Count what the second attempt actually reports moving.
        let mut seen: Vec<Progress> = Vec::new();
        store
            .ensure_entry(&e, &mut net, &mut |p| seen.push(p))
            .unwrap();
        let first = seen.first().unwrap();
        assert_eq!(first.done, cut, "resume starts at the part length");
        assert!(first.resumed);
        assert_eq!(seen.last().unwrap().done, bytes.len() as u64);
    }

    /// The bug that makes naive resume worse than no resume: a `200` in answer
    /// to a `Range:` request, appended to what was already there.
    #[test]
    fn a_server_that_ignores_range_restarts_rather_than_producing_a_corrupt_file() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let cut = 70_000u64;

        // Stop half way with a server that does support ranges...
        let mut net = FakeNet::new();
        net.serve_truncated(e.url, bytes.clone(), cut, 1);
        let _ = store.ensure_entry(&e, &mut net, &mut quiet());
        assert_eq!(std::fs::metadata(store.part_of(&e)).unwrap().len(), cut);

        // ...and come back to one that does not.
        let mut rude = FakeNet::new();
        rude.serve_ignoring_range(e.url, bytes.clone());
        let path = store.ensure_entry(&e, &mut rude, &mut quiet()).unwrap();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "the file must be the resource, not the resource with a prefix glued on"
        );
        assert_eq!(rude.offsets(e.url), vec![cut], "we did ask for the tail");
    }

    #[test]
    fn a_restart_reports_progress_from_zero_and_not_from_the_stale_part_length() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let mut net = FakeNet::new();
        net.serve_truncated(e.url, bytes.clone(), 70_000, 1);
        let _ = store.ensure_entry(&e, &mut net, &mut quiet());

        let mut rude = FakeNet::new();
        rude.serve_ignoring_range(e.url, bytes.clone());
        let mut seen: Vec<Progress> = Vec::new();
        store
            .ensure_entry(&e, &mut rude, &mut |p| seen.push(p))
            .unwrap();
        assert_eq!(seen.first().unwrap().done, 0);
        assert!(!seen.first().unwrap().resumed);
    }

    #[test]
    fn a_body_with_the_right_length_and_the_wrong_bytes_is_refused_and_leaves_nothing_behind() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let mut wrong = bytes.clone();
        let last = wrong.len() - 1;
        wrong[last] ^= 0xff; // same length, so only the hash can catch it

        let mut net = FakeNet::new();
        net.serve(e.url, wrong);

        match store.ensure_entry(&e, &mut net, &mut quiet()) {
            Err(VoiceError::ModelCorrupt { id, want, got }) => {
                assert_eq!(id, e.id);
                assert_eq!(want, e.sha256);
                assert_ne!(got, e.sha256);
            }
            other => panic!("expected ModelCorrupt, got {other:?}"),
        }
        assert!(!store.part_of(&e).exists(), "the .part must be deleted");
        assert!(!store.path_of(&e).exists(), "nothing may appear in the store");
        assert!(!store.have_entry(&e));
    }

    #[test]
    fn a_hash_mismatch_is_not_retried_in_a_loop() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let mut wrong = bytes;
        wrong[0] ^= 0xff;
        let mut net = FakeNet::new();
        net.serve(e.url, wrong);
        let _ = store.ensure_entry(&e, &mut net, &mut quiet());
        assert_eq!(net.hits(e.url), 1, "one attempt, one refusal");
    }

    #[test]
    fn an_already_installed_and_correct_model_costs_zero_requests() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let mut net = FakeNet::new();
        net.serve(e.url, bytes);
        store.ensure_entry(&e, &mut net, &mut quiet()).unwrap();
        net.forget_log();

        for _ in 0..3 {
            store.ensure_entry(&e, &mut net, &mut quiet()).unwrap();
        }
        assert_eq!(net.requests(), 0, "an installed model is not a download");
    }

    #[test]
    fn a_corrupt_installed_model_is_reported_and_never_downloaded_over() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let mut net = FakeNet::new();
        net.serve(e.url, bytes.clone());
        store.ensure_entry(&e, &mut net, &mut quiet()).unwrap();
        net.forget_log();

        // Something rots the installed file: a bad sector, a helpful cleaner, a
        // half-finished manual copy.
        let mut rotten = bytes;
        rotten[10] ^= 0xff;
        std::fs::write(store.path_of(&e), &rotten).unwrap();

        match store.ensure_entry(&e, &mut net, &mut quiet()) {
            Err(VoiceError::ModelCorrupt { id, .. }) => assert_eq!(id, e.id),
            other => panic!("expected ModelCorrupt, got {other:?}"),
        }
        assert_eq!(net.requests(), 0, "a refusal is not a re-download");
        assert_eq!(
            std::fs::read(store.path_of(&e)).unwrap(),
            rotten,
            "the bad file is left for the operator to look at, not silently replaced"
        );
    }

    #[test]
    fn a_part_file_is_never_mistaken_for_an_installed_model() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        std::fs::create_dir_all(store.root()).unwrap();
        std::fs::write(store.part_of(&e), &bytes).unwrap();

        assert!(!store.have_entry(&e));
        assert!(matches!(
            store.verify_entry(&e),
            Err(VoiceError::ModelMissing(_))
        ));
        assert!(!store.plan_all().is_empty());
    }

    #[test]
    fn a_part_longer_than_the_pin_is_thrown_away_rather_than_resumed_from() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        std::fs::create_dir_all(store.root()).unwrap();
        // Junk from an older, larger revision of the same file.
        std::fs::write(store.part_of(&e), vec![0u8; bytes.len() + 4096]).unwrap();

        let mut net = FakeNet::new();
        net.serve(e.url, bytes.clone());
        let path = store.ensure_entry(&e, &mut net, &mut quiet()).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert_eq!(net.offsets(e.url), vec![0], "a too-long part is not a resume point");
    }

    #[test]
    fn a_url_that_serves_a_different_length_than_the_pin_is_refused_before_anything_is_written() {
        let (_tmp, store) = store();
        let (_bytes, e) = fixture();
        let mut net = FakeNet::new();
        net.serve(e.url, blob(N + 9));

        match store.ensure_entry(&e, &mut net, &mut quiet()) {
            Err(VoiceError::Fetch { why, .. }) => assert!(why.contains("manifest pins"), "{why}"),
            other => panic!("expected Fetch, got {other:?}"),
        }
        assert!(!store.part_of(&e).exists());
        assert!(!store.path_of(&e).exists());
    }

    #[test]
    fn a_url_nobody_serves_fails_without_creating_a_part_file() {
        let (_tmp, store) = store();
        let (_bytes, e) = fixture();
        let mut net = FakeNet::new();
        assert!(matches!(
            store.ensure_entry(&e, &mut net, &mut quiet()),
            Err(VoiceError::Fetch { .. })
        ));
        assert!(!store.part_of(&e).exists());
    }

    #[test]
    fn an_unknown_id_is_missing_rather_than_a_panic() {
        let (_tmp, store) = store();
        let mut net = FakeNet::new();
        assert!(matches!(
            store.ensure("no-such-model", &mut net, &mut quiet()),
            Err(VoiceError::ModelMissing(_))
        ));
        assert!(store.path("no-such-model").is_none());
        assert!(!store.have("no-such-model"));
        assert!(matches!(
            store.verify("no-such-model"),
            Err(VoiceError::ModelMissing(_))
        ));
    }

    // -- progress ------------------------------------------------------------

    #[test]
    fn progress_is_reported_often_enough_to_move_a_bar_and_nothing_like_per_byte() {
        let (_tmp, store) = store();
        // Big enough to cross the stride a few times.
        let bytes = blob(5 * PROGRESS_STRIDE as usize / 2);
        let sha: &'static str = Box::leak(sha256_of(&bytes).into_boxed_str());
        let e = test_entry(sha, bytes.len() as u64);
        let mut net = FakeNet::new();
        net.serve(e.url, bytes.clone());

        let mut seen: Vec<Progress> = Vec::new();
        store
            .ensure_entry(&e, &mut net, &mut |p| seen.push(p))
            .unwrap();

        assert!(seen.len() >= 3, "a bar needs intermediate reports: {}", seen.len());
        assert!(seen.len() < 64, "{} reports for 2.5 MiB is a firehose", seen.len());
        assert_eq!(seen.first().unwrap().done, 0);
        assert_eq!(seen.last().unwrap().done, bytes.len() as u64);
        assert!(
            seen.windows(2).all(|w| w[0].done <= w[1].done),
            "progress must never go backwards"
        );
        assert_eq!(seen.last().unwrap().fraction(), Some(1.0));
        assert!(seen.iter().all(|p| p.id == e.id));
    }

    // -- planning ------------------------------------------------------------

    #[test]
    fn a_plan_says_what_is_missing_and_what_it_would_cost_before_a_byte_is_spent() {
        let (_tmp, store) = store();
        let plan = store.plan_all();
        assert_eq!(plan.missing.len(), MANIFEST.len());
        assert_eq!(plan.bytes, MANIFEST.iter().map(|e| e.bytes).sum::<u64>());
        assert_eq!(plan.already, 0);
        assert_eq!(plan.remaining(), plan.bytes);
        assert!(plan.remaining_mib() > 100.0, "this is worth asking about first");
    }

    #[test]
    fn a_plan_discounts_the_bytes_a_part_file_already_holds() {
        let (_tmp, store) = store();
        let e = entry("whisper-tiny.en").unwrap();
        std::fs::create_dir_all(store.root()).unwrap();
        std::fs::write(store.part_of(e), vec![0u8; 4096]).unwrap();

        let plan = store.plan(["whisper-tiny.en"]);
        assert_eq!(plan.missing, vec![e]);
        assert_eq!(plan.bytes, e.bytes);
        assert_eq!(plan.already, 4096);
        assert_eq!(plan.remaining(), e.bytes - 4096);
    }

    #[test]
    fn a_plan_ignores_ids_that_are_not_pinned_because_they_cannot_be_fetched() {
        let (_tmp, store) = store();
        let plan = store.plan(["whisper-tiny.en", "something-i-made-up"]);
        assert_eq!(plan.missing.len(), 1);
    }

    #[test]
    fn an_installed_artefact_drops_out_of_the_plan() {
        let (_tmp, store) = store();
        let e = entry("whisper-tiny.en").unwrap();
        assert!(store.plan_all().missing.contains(&e));

        // Presence only, deliberately: answering "what would this cost" must
        // not itself cost a 300 MB hash. The content is checked when it is
        // about to be loaded, not when it is being counted.
        std::fs::create_dir_all(store.root()).unwrap();
        std::fs::write(store.path_of(e), b"not really a model").unwrap();

        let after = store.plan_all();
        assert!(!after.missing.contains(&e));
        assert_eq!(after.bytes, store.plan_all().bytes);
        assert_eq!(
            after.missing.len(),
            MANIFEST.len() - 1,
            "everything else is still missing"
        );
    }

    // -- the verified stamp --------------------------------------------------

    #[test]
    fn the_verified_stamp_saves_a_rehash_but_never_grants_a_pass() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let mut net = FakeNet::new();
        net.serve(e.url, bytes.clone());
        store.ensure_entry(&e, &mut net, &mut quiet()).unwrap();
        assert!(store.root().join(".verified.json").is_file());
        store.verify_entry(&e).unwrap();

        // Rewrite the file with different content of the same length, and move
        // the mtime by hand rather than trusting the filesystem's timestamp
        // granularity to notice two writes a microsecond apart.
        let mut rotten = bytes;
        rotten[0] ^= 0xff;
        std::fs::write(store.path_of(&e), &rotten).unwrap();
        touch_later(&store.path_of(&e));
        assert!(
            matches!(store.verify_entry(&e), Err(VoiceError::ModelCorrupt { .. })),
            "a stamp must never be able to bless the wrong bytes"
        );
    }

    #[test]
    fn a_stamp_stops_vouching_when_the_file_changes_length() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let mut net = FakeNet::new();
        net.serve(e.url, bytes.clone());
        store.ensure_entry(&e, &mut net, &mut quiet()).unwrap();

        let mut short = bytes;
        short.truncate(short.len() - 1);
        std::fs::write(store.path_of(&e), &short).unwrap();
        assert!(matches!(
            store.verify_entry(&e),
            Err(VoiceError::ModelCorrupt { .. })
        ));
    }

    /// Push a file's mtime a minute into the future, so a test can assert on
    /// "the file changed" without racing the clock.
    fn touch_later(path: &Path) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        f.set_times(std::fs::FileTimes::new().set_modified(later)).unwrap();
    }

    #[test]
    fn an_unreadable_stamp_file_means_hash_it_rather_than_trust_it() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let mut net = FakeNet::new();
        net.serve(e.url, bytes.clone());
        store.ensure_entry(&e, &mut net, &mut quiet()).unwrap();

        std::fs::write(store.root().join(".verified.json"), b"{ not json").unwrap();
        store.verify_entry(&e).unwrap(); // hashed for real, and correct
        assert!(store.have_entry(&e));
    }

    #[test]
    fn a_stamp_from_a_different_manifest_revision_does_not_vouch_for_the_new_pin() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let mut net = FakeNet::new();
        net.serve(e.url, bytes.clone());
        store.ensure_entry(&e, &mut net, &mut quiet()).unwrap();

        // Same id, same file on disk, but the manifest now pins something else.
        let bumped = test_entry(
            "0000000000000000000000000000000000000000000000000000000000000000",
            bytes.len() as u64,
        );
        assert!(matches!(
            store.verify_entry(&bumped),
            Err(VoiceError::ModelCorrupt { .. })
        ));
    }

    // -- where it writes -----------------------------------------------------

    /// SPEC §4. The dev build and the operator's installed copy must not be
    /// able to share a store, and a 300 MB model is the least forgiving way to
    /// find out that they do.
    #[test]
    fn the_store_honours_the_test_override_and_never_touches_the_real_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("NX_WISP_CONFIG_DIR");
        std::env::set_var("NX_WISP_CONFIG_DIR", tmp.path());

        // Read everything and put the variable back before asserting, so the
        // window in which this test owns a process-global is as short as the
        // one in `crate::tests::data_dir_honours_the_test_override`.
        let store = ModelStore::open();
        let installed = store.path("whisper-tiny.en").unwrap();
        match prev {
            Some(p) => std::env::set_var("NX_WISP_CONFIG_DIR", p),
            None => std::env::remove_var("NX_WISP_CONFIG_DIR"),
        }

        assert_eq!(store.root(), tmp.path().join("models"));
        assert!(installed.starts_with(tmp.path()));
        assert!(!installed.starts_with(dirs_home()), "{}", installed.display());
    }

    fn dirs_home() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/nonexistent"))
            .join(".local")
            .join("share")
    }

    #[test]
    fn nothing_is_ever_written_outside_the_store_root() {
        let (_tmp, store) = store();
        let (bytes, e) = fixture();
        let mut net = FakeNet::new();
        net.serve(e.url, bytes);
        store.ensure_entry(&e, &mut net, &mut quiet()).unwrap();

        let mut names: Vec<String> = std::fs::read_dir(store.root())
            .unwrap()
            .map(|d| d.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec![".verified.json", "test-blob.bin"]);
    }

    /// Without `net` there is no `HttpFetcher`, and this must still compile and
    /// give a caller an answer it already knows how to handle.
    #[cfg(not(feature = "net"))]
    #[test]
    fn a_build_without_the_net_feature_reports_the_model_missing_rather_than_failing_to_compile() {
        let (_tmp, store) = store();
        assert!(matches!(
            store.ensure_online("whisper-tiny.en", &mut quiet()),
            Err(VoiceError::ModelMissing(_))
        ));
    }

    // -- hashing -------------------------------------------------------------

    #[test]
    fn the_empty_hash_is_the_well_known_one_so_the_hex_encoding_is_not_the_bug() {
        assert_eq!(
            sha256_of(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hashing_a_file_and_hashing_its_bytes_agree() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x");
        let bytes = blob(CHUNK * 2 + 17); // crosses the read buffer twice
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(sha256_file(&path).unwrap(), sha256_of(&bytes));
    }
}
