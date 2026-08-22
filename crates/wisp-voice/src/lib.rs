//! `wisp-voice` — she talks, and she listens. SPEC.md §2, plan F28 and F31–F35.
//!
//! ```text
//!   text in ──▶ chunk ──▶ Tts ──▶ Pcm ──▶ Sink ──▶ speakers
//!   (F31)      (text)   (tts)     │      (sink)
//!                                 └──▶ lipsync ──▶ DriveTrack ──▶ the host's rig
//!                                       (F32)
//!   while any of that is true: Ducker holds other players down (F33)
//!
//!   microphone ──▶ MicPermit ──▶ Listener ──▶ Stt ──▶ Observation::Speech
//!    (F28)         (consent)      (mic)      (stt)     partial…partial…final
//!                       │
//!                       └──▶ the visible tell of SPEC §0.3, up for the whole time
//! ```
//!
//! ## The five things that shape this crate
//!
//! **1. Everything real sits behind a trait.** [`tts::Tts`], [`stt::Stt`],
//! [`sink::AudioSink`], [`mic::MicSource`], [`duck::Mixer`] and
//! [`fetch::Fetcher`] each have a deterministic in-tree fake, and the fakes are
//! `pub` rather than `cfg(test)` so the binary can run her with no models
//! installed. `cargo test -p wisp-voice` passes with no GPU, no model file, no
//! audio device and no network, because on the default feature set none of the
//! real backends are even compiled.
//!
//! **2. The microphone is unrepresentable without consent.** [`mic::Listener`]
//! cannot be constructed except from a [`MicPermit`], and the only thing in the
//! tree that produces a real one is `wisp_senses::ConsentLedger::grant`, which
//! raises the visible tell of SPEC §0.3 before it returns and lowers it when the
//! handle drops. This crate never opens a capture device on any other path.
//!
//! **3. She has to be sheddable mid-sentence.** SPEC §0.1 and §3.1: a downgrade
//! is synchronous and immediate, and work is *shed*, not queued. [`Voice`]
//! implements [`wisp_proto::Governed`] by cancelling the utterance in flight,
//! dropping the play queue, releasing the duck and — at T3 — dropping the mic
//! permit. There is no deferred-speech queue here; §3.5 grants one to
//! `wisp-mind` and to nothing else.
//!
//! **4. Ducking must survive her death.** A companion that permanently turns
//! your music down because she was `SIGKILL`ed mid-sentence is worse than one
//! that never speaks. [`duck::Ducker`] writes a journal before it touches a
//! volume and [`duck::Ducker::recover`] replays it on the next start.
//!
//! **5. Models are not in the AppImage.** [`models`] pins every artefact by
//! URL, sha256 and byte length (SPEC §0.2a), downloads on first use with resume,
//! verifies the hash and only then moves the file into place. Nothing is ever
//! written into the repository.
//!
//! ## Engine choices, and why
//!
//! The machine this was built against has an RX 7900 XTX with **no ROCm**. That
//! single fact decides the TTS engine:
//!
//! - `ort`'s GPU execution providers on Linux are CUDA, TensorRT, ROCm and
//!   MIGraphX. None of them exist here, so *any* ONNX TTS runs on the CPU.
//! - Kokoro-82M on CPU is roughly real-time. That is fine at T0/T1 and it is
//!   exactly what SPEC §0.1 forbids at T2, where she is supposed to get cheaper
//!   without going mute.
//! - Piper's VITS voices are ~20M parameters and synthesise at tens of times
//!   real time on one core, which is what "starts talking before the sentence is
//!   finished" needs when the governor has already taken the machine away.
//!
//! So **Piper is the default engine** and Kokoro-82M is an optional quality pack
//! pinned in the same manifest, selected at T0/T1 only. Both are ONNX; both go
//! through [`tts::Tts`]; the pack that names them is data ([`voices`], F35).
//!
//! STT is **whisper.cpp via `whisper-rs`, Vulkan backend** — Vulkan is the one
//! GPU API SPEC §1 allows, and whisper.cpp is the only local STT with a working
//! Vulkan path. Push-to-talk is the default and the wake word is opt-in, per
//! F28.

#![forbid(unsafe_code)]

pub mod audio;
pub mod barge;
pub mod duck;
pub mod fetch;
pub mod lipsync;
pub mod mic;
pub mod models;
pub mod sink;
pub mod speaker;
pub mod stt;
pub mod tell;
pub mod text;
pub mod tier;
pub mod tts;
pub mod voices;


#[cfg(feature = "consent")]
pub mod consent_adapter;

#[cfg(feature = "piper-tts")]
pub mod piper;

// The whisper.cpp backend is `stt::whisper`, nested in `stt.rs` rather than
// given its own file — it is small and it is meaningless apart from the trait
// it implements.

mod voice;

pub use audio::Pcm;
pub use barge::{BargeIn, BargePolicy, BargeSignal, CancelReason};
pub use duck::{DuckConfig, Ducker, Mixer, StreamKey};
pub use lipsync::{DriveFrame, DriveTrack, Viseme};
pub use mic::{Listener, MicPermit, MicSource};
pub use models::{ModelStore, MANIFEST};
pub use speaker::{Speaker, SpeechId, SpeechState};
pub use stt::{Stt, Transcript};
pub use text::Chunker;
pub use tts::{PhonemeSpan, PhonemeTiming, SynthParams, Synthesis, Tts};
pub use voice::{Voice, VoiceTick};
pub use voices::{Mood, VoicePack, VoiceRegistry};

/// Monotonic milliseconds, from `wisp-proto`. Never wall-clock: SPEC §3.2's
/// ordering guarantee has to survive a suspend, and so does a play queue.
pub type Millis = wisp_proto::Millis;

/// Everything this crate can fail at.
///
/// Deliberately coarse. A caller's only real decisions are "did the model need
/// downloading", "is she allowed to make noise right now" and "did the operator
/// revoke the microphone" — the rest is logged and shed.
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("{what}: {source}")]
    Io {
        what: String,
        #[source]
        source: std::io::Error,
    },

    /// A pinned artefact is missing, and `net` is off or the fetch failed.
    #[error("model {0} is not installed")]
    ModelMissing(String),

    /// The bytes on disk are not the bytes we pinned. Never repaired in place —
    /// SPEC §0.2a means a mismatch is a refusal, not a retry with a shrug.
    #[error("model {id}: expected sha256 {want}, got {got}")]
    ModelCorrupt {
        id: String,
        want: &'static str,
        got: String,
    },

    #[error("download failed for {url}: {why}")]
    Fetch { url: String, why: String },

    #[error("synthesis failed: {0}")]
    Synth(String),

    #[error("transcription failed: {0}")]
    Stt(String),

    #[error("audio sink: {0}")]
    Sink(String),

    #[error("mixer: {0}")]
    Mixer(String),

    /// The operator turned the microphone off while we held it.
    #[error("microphone consent was revoked")]
    ConsentRevoked,

    /// The governor has her below the tier this needs.
    #[error("not permitted at {tier:?}: {what}")]
    Tier {
        tier: wisp_proto::Tier,
        what: &'static str,
    },

    /// A real backend that this build was not compiled with.
    #[error("{0} is not compiled into this build")]
    NotCompiled(&'static str),

    #[error("no voice pack named {0}")]
    NoSuchVoice(String),
}

impl VoiceError {
    pub(crate) fn io(what: impl Into<String>, source: std::io::Error) -> Self {
        VoiceError::Io {
            what: what.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, VoiceError>;

/// Where her *data* lives — models and voice packs, not settings.
///
/// `NX_WISP_CONFIG_DIR` wins over XDG exactly as it does in
/// `wisp_senses::consent::config_dir`, and for the same reason: SPEC §4 says a
/// test must never be able to reach the operator's real state, and a 300 MB
/// model landing in the operator's real store because a test forgot is a
/// nastier version of the same bug. **Nothing here ever writes into the repo.**
pub fn data_dir() -> std::path::PathBuf {
    resolve_data_dir(
        std::env::var_os("NX_WISP_CONFIG_DIR"),
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
    )
}

/// The precedence rule, as a pure function of the three variables.
///
/// Split out so its test does not have to *set* an environment variable.
/// `cargo test` runs threads in one process, so a test that mutates the process
/// environment races every other test that reads it — and since SPEC §4 requires
/// every state-touching test in the tree to read `NX_WISP_CONFIG_DIR`, a test
/// that flipped it here could send another test's fixtures at the operator's
/// real store. Found while the ducking tests were being written, where it showed
/// up as a one-in-a-hundred flake.
fn resolve_data_dir(
    config_dir: Option<std::ffi::OsString>,
    xdg_data: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> std::path::PathBuf {
    use std::path::PathBuf;
    if let Some(d) = config_dir {
        return PathBuf::from(d);
    }
    if let Some(d) = xdg_data {
        return PathBuf::from(d).join("nx-wisp");
    }
    home.map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("share")
        .join("nx-wisp")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::OsString;
    use std::path::PathBuf;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    /// SPEC §4's rule, enforced on this crate's own store: `NX_WISP_CONFIG_DIR`
    /// wins over everything, so a test can never reach the operator's data.
    ///
    /// Note what this test does *not* do: touch the process environment. See
    /// [`resolve_data_dir`].
    #[test]
    fn the_test_override_beats_xdg_and_home() {
        assert_eq!(
            resolve_data_dir(os("/tmp/fixture"), os("/home/x/.local/share"), os("/home/x")),
            PathBuf::from("/tmp/fixture")
        );
    }

    #[test]
    fn xdg_beats_home_and_gets_its_own_subdirectory() {
        assert_eq!(
            resolve_data_dir(None, os("/home/x/.local/share"), os("/home/x")),
            PathBuf::from("/home/x/.local/share/nx-wisp")
        );
    }

    #[test]
    fn without_xdg_it_lands_where_the_spec_says_models_live() {
        assert_eq!(
            resolve_data_dir(None, None, os("/home/x")),
            PathBuf::from("/home/x/.local/share/nx-wisp")
        );
    }

    #[test]
    fn a_homeless_process_gets_a_relative_path_rather_than_a_panic() {
        assert_eq!(
            resolve_data_dir(None, None, None),
            PathBuf::from("./.local/share/nx-wisp")
        );
    }

    /// Whatever the environment says, the store is never inside the repository.
    #[test]
    fn models_never_live_in_the_repo() {
        let d = data_dir();
        assert!(
            !d.starts_with(env!("CARGO_MANIFEST_DIR")),
            "the data dir resolved into the source tree: {}",
            d.display()
        );
    }
}
