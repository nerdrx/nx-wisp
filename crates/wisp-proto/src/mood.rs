//! SPEC §3.8 — the mood vocabulary.
//!
//! Mood became cross-crate in fact before it was cross-crate in the contract:
//! `wisp-mind` decides it, `wisp-attn` gates behaviour on it, `wisp-voice`
//! colours speech with it, and `wisp-rig` draws it. Three identical copies of
//! this enum existed, plus a mapping table maintained in a fourth place, and
//! two separate implementers independently reported it. So the **vocabulary**
//! lives here.
//!
//! The **machine** does not. SPEC §2 is right that the mood FSM belongs to
//! cognition; only the words are shared.

use serde::{Deserialize, Serialize};

/// How she is feeling. Nine states, decided by `wisp-mind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub enum Mood {
    #[default]
    Calm,
    Curious,
    Playful,
    Smug,
    Sulky,
    Focused,
    Sleepy,
    Alarmed,
    Affectionate,
}

impl Mood {
    pub const ALL: [Mood; 9] = [
        Mood::Calm,
        Mood::Curious,
        Mood::Playful,
        Mood::Smug,
        Mood::Sulky,
        Mood::Focused,
        Mood::Sleepy,
        Mood::Alarmed,
        Mood::Affectionate,
    ];

    /// The expression a skin must wear for this mood.
    ///
    /// Nine moods, eight expressions — `Playful` and `Affectionate` share
    /// "delighted", because a skin should not have to draw a distinction its
    /// face cannot carry.
    ///
    /// The *names* live here rather than in `wisp-rig` so that this crate stays
    /// dependency-free, which SPEC §2 requires. `wisp-rig` owns the authoritative
    /// list and keeps a test asserting that every name this returns is in it —
    /// the dependency points the right way and the coupling stays checkable.
    pub fn expression(self) -> &'static str {
        match self {
            Mood::Calm => "neutral",
            Mood::Curious => "curious",
            Mood::Playful | Mood::Affectionate => "delighted",
            Mood::Smug => "smug",
            Mood::Sulky => "worried",
            Mood::Focused => "bored",
            Mood::Sleepy => "sleepy",
            Mood::Alarmed => "alarmed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mood_has_an_expression_and_two_deliberately_share_one() {
        let mut seen = std::collections::BTreeSet::new();
        for m in Mood::ALL {
            assert!(!m.expression().is_empty(), "{m:?} has no expression");
            seen.insert(m.expression());
        }
        assert_eq!(seen.len(), 8, "nine moods should land on exactly eight expressions");
        assert_eq!(Mood::Playful.expression(), Mood::Affectionate.expression());
    }

    #[test]
    fn all_lists_every_variant_exactly_once() {
        let set: std::collections::BTreeSet<_> = Mood::ALL.iter().collect();
        assert_eq!(set.len(), Mood::ALL.len());
        assert_eq!(Mood::default(), Mood::Calm);
    }
}
