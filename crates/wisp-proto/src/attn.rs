//! SPEC §3.4 — nothing reaches the operator except through here.

use serde::{Deserialize, Serialize};

/// How badly this wants to be heard. `wisp-attn` holds the token bucket and
/// decides; `wisp-mind` may not speak directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Urgency {
    /// Idle chatter. Dropped freely.
    Whim,
    /// Worth saying if she gets a cheap moment.
    Notable,
    /// The operator asked for it. Always delivered.
    Answer,
    /// Something is wrong and waiting would be worse than interrupting.
    Alarm,
}

impl Urgency {
    /// Attention tokens this costs from the bucket. `Answer` and `Alarm` are
    /// free because the operator either asked, or needs to know now.
    pub fn cost(self) -> u32 {
        match self {
            Urgency::Whim => 3,
            Urgency::Notable => 1,
            Urgency::Answer | Urgency::Alarm => 0,
        }
    }
    /// May this be spoken while the operator appears to be in flow?
    pub fn breaks_flow(self) -> bool {
        matches!(self, Urgency::Answer | Urgency::Alarm)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Utterance {
    pub text: String,
    pub urgency: Urgency,
    /// Don't consider saying this before now (monotonic ms).
    pub defer_until: Option<crate::Millis>,
    /// Drop it unsaid after this (monotonic ms). A thought that has gone stale
    /// is dropped and recorded as dropped — never silently resurrected.
    pub stale_after: Option<crate::Millis>,
    /// Play the matching expression on the rig while saying it.
    pub expression: Option<String>,
}

impl Utterance {
    pub fn new(text: impl Into<String>, urgency: Urgency) -> Self {
        Self { text: text.into(), urgency, defer_until: None, stale_after: None, expression: None }
    }
}
