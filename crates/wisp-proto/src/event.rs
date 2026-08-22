//! SPEC §3.2 — the internal bus. Events are **facts about the past**, never
//! commands. Everything is recorded before dispatch.

use serde::{Deserialize, Serialize};

use crate::{gov::{Tier, TierReason}, sense::Observation, attn::Utterance, Millis};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub at: Millis,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    /// A sense reported something.
    Sensed(Observation),
    /// The governor moved her between tiers.
    TierChanged { from: Tier, to: Tier, reason: TierReason },
    /// Something asked to be said.
    Proposed(Utterance),
    /// The attention budget allowed it and she said it.
    Said { text: String },
    /// The attention budget refused or it went stale.
    Dropped { text: String, why: String },
    /// A tool ran.
    ToolCall { name: String, args: String, ok: bool },
    /// Cognition was deferred because of the tier (SPEC §3.5).
    Deferred { what: String, queued: usize },
    /// A deferred item was replayed, or dropped for staleness.
    Replayed { what: String, dropped: bool },
    /// A model was loaded or evicted.
    Model { name: String, loaded: bool, vram_mib: u64 },
    /// An invasive sense started or stopped, for the visible tell of §0.3.
    InvasiveActive { sense: crate::sense::SenseId, active: bool },
}
