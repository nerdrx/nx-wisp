//! SPEC §3.3 / §3.7 — what the senses may see, and what it costs in consent.

use serde::{Deserialize, Serialize};

/// How much permission a sense or tool needs before it may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Consent {
    /// Runs unprompted. Ships enabled.
    Ambient,
    /// The operator must enable it. Ships disabled.
    Explicit,
    /// Mic, clipboard, screen. Ships disabled, and requires a visible tell on
    /// the character herself for the entire time it is active (SPEC §0.3).
    Invasive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SenseId {
    Idle,
    ActiveWindow,
    WindowGeometry,
    Media,
    Audio,
    Notifications,
    Vitals,
    Workspace,
    Clipboard,
    Microphone,
    Screen,
    Fleet,
}

impl SenseId {
    pub fn consent(self) -> Consent {
        match self {
            SenseId::Idle
            | SenseId::ActiveWindow
            | SenseId::WindowGeometry
            | SenseId::Media
            | SenseId::Audio
            | SenseId::Notifications
            | SenseId::Vitals
            | SenseId::Workspace
            | SenseId::Fleet => Consent::Ambient,
            SenseId::Clipboard | SenseId::Microphone | SenseId::Screen => Consent::Invasive,
        }
    }
}

/// A closed enum, deliberately — in the spirit of NX Orbit's `ObsKind`. Adding a
/// variant is a spec amendment, not an implementation detail. In particular
/// there is no variant for inferred judgements about the operator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Observation {
    /// The operator went idle or came back.
    Idle { idle: bool, for_ms: u64 },
    /// Focus moved to a different surface.
    Focus { app_id: String, title: String },
    /// A window's geometry changed. She uses these as terrain.
    Window { id: u64, x: i32, y: i32, w: u32, h: u32, gone: bool },
    /// Something is playing.
    Media { player: String, title: String, artist: String, playing: bool },
    /// Audio activity level, 0..=100.
    AudioLevel { out: u8, mic_live: bool },
    /// A desktop notification arrived.
    Notification { app: String, summary: String, body: String },
    /// Machine vitals sample.
    Vitals { cpu_pct: u8, gpu_pct: u8, vram_used_mib: u64, temp_c: u8, on_battery: bool },
    /// Virtual desktop changed.
    Workspace { index: u32, name: String },
    /// A watched repository or directory changed.
    Files { path: String, dirty: bool },
    /// Transcribed speech from the operator (Invasive).
    Speech { text: String, final_: bool },
    /// Clipboard content changed (Invasive). Never persisted by default.
    Clipboard { len: usize, kind: String },
    /// Another NX app said something on the Connector bus.
    Fleet { app: String, field: String, value: String },
}

impl Observation {
    pub fn sense(&self) -> SenseId {
        match self {
            Observation::Idle { .. } => SenseId::Idle,
            Observation::Focus { .. } => SenseId::ActiveWindow,
            Observation::Window { .. } => SenseId::WindowGeometry,
            Observation::Media { .. } => SenseId::Media,
            Observation::AudioLevel { .. } => SenseId::Audio,
            Observation::Notification { .. } => SenseId::Notifications,
            Observation::Vitals { .. } => SenseId::Vitals,
            Observation::Workspace { .. } => SenseId::Workspace,
            Observation::Files { .. } => SenseId::Vitals,
            Observation::Speech { .. } => SenseId::Microphone,
            Observation::Clipboard { .. } => SenseId::Clipboard,
            Observation::Fleet { .. } => SenseId::Fleet,
        }
    }
}
