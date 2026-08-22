//! **F13 — the VRAM budget manager.**
//!
//! The feature that makes a local LLM livable on a gaming machine: watch for a
//! fullscreen game or a WiVRn session and evict the big model *before* it can
//! steal frames from the headset, not after the operator notices judder.
//!
//! `wisp-gov` does not own any VRAM, so this module only ever produces a
//! [`VramBudget`]. `wisp-mind` is the [`wisp_proto::Governed`] that acts on it,
//! and because downgrades are delivered synchronously (SPEC §3.1) the eviction
//! happens inside the same `step` that noticed the game.

use serde::{Deserialize, Serialize};
use wisp_proto::{Tier, TierReason};

use crate::{
    config::GovConfig,
    reading::{GpuKind, Snapshot},
};

/// How much VRAM she may hold, per card, right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VramBudget {
    /// Ceiling on the discrete card. Zero at T3/T4 — that is the whole feature.
    pub dgpu_mib: u64,
    /// Ceiling on the integrated card, where she hides at T2/T3.
    pub igpu_mib: u64,
    /// The discrete allowance dropped in this step and somebody must free
    /// memory *now*, synchronously, before returning to the event loop.
    pub evict_now: bool,
    /// Plain English for the readout and the flight recorder.
    pub note: String,
}

impl VramBudget {
    /// Ceiling for one card kind.
    pub fn for_kind(&self, kind: GpuKind) -> u64 {
        match kind {
            GpuKind::Integrated => self.igpu_mib,
            // An unidentified card is treated as discrete: we would rather
            // under-use a card than accidentally fight a game for it.
            GpuKind::Discrete | GpuKind::Unknown => self.dgpu_mib,
        }
    }
}

/// Compute the budget a tier implies for this machine. Pure.
///
/// At T0/T1 the allowance is *whatever is genuinely spare*: the card's total,
/// less the configured headroom, less whatever other processes are already
/// using. On the operator's 24560 MiB card with nothing else running that is
/// still enough for the 30B MoE; with a compositor and a browser on it, it
/// shrinks by itself instead of needing a hardcoded number.
/// `ours_dgpu_mib` is how much discrete VRAM our own process already holds, from
/// [`crate::probe::selfcost`]. Without it the allowance would collapse as our
/// own model filled the card and she would evict herself in a loop.
pub fn budget(
    tier: Tier,
    s: &Snapshot,
    cfg: &GovConfig,
    ours_dgpu_mib: u64,
    previous: Option<&VramBudget>,
) -> VramBudget {
    let dgpu = s.discrete();
    let igpu = s.integrated();

    let dgpu_mib = match tier {
        Tier::Lobotomised | Tier::Dormant => 0,
        Tier::Reduced => cfg.reduced_vram_budget_mib,
        Tier::Full | Tier::Feral => match dgpu.or_else(|| s.primary()) {
            Some(g) => {
                // `mem_info_vram_used` includes our own allocations, so this is
                // deliberately conservative: it shrinks as others take memory
                // and never grows past the physical card.
                let spare = g.vram_total_mib.saturating_sub(cfg.vram_headroom_mib);
                let others = g.vram_used_mib.saturating_sub(ours_dgpu_mib);
                spare.min(g.vram_total_mib.saturating_sub(others))
            }
            None => 0,
        },
    };

    let igpu_mib = match (tier, igpu) {
        (Tier::Dormant, _) | (_, None) => 0,
        (Tier::Lobotomised, Some(g)) => cfg.lobotomised_vram_budget_mib.min(g.vram_total_mib),
        (Tier::Reduced, Some(g)) => {
            // Leave the compositor its share of a small carve-out.
            g.vram_total_mib.saturating_sub(512).min(cfg.reduced_vram_budget_mib)
        }
        (_, Some(g)) => cfg.lobotomised_vram_budget_mib.min(g.vram_total_mib),
    };

    let evict_now = previous.is_some_and(|p| dgpu_mib < p.dgpu_mib);

    let note = match tier {
        Tier::Lobotomised | Tier::Dormant => {
            "zero discrete VRAM: the card belongs to whatever you started".to_string()
        }
        Tier::Reduced => format!(
            "reflex model only, capped at {dgpu_mib} MiB; the deliberate model stays mmapped in RAM"
        ),
        Tier::Full | Tier::Feral => format!("up to {dgpu_mib} MiB, released the moment you need it"),
    };

    VramBudget {
        dgpu_mib,
        igpu_mib,
        evict_now,
        note,
    }
}

/// Does this tier change mean a game or a headset is about to want the card?
/// Used by the governor to log the eviction with the right words.
pub fn eviction_reason(reason: &TierReason) -> Option<&'static str> {
    match reason {
        TierReason::VrSession => Some("a headset is about to want every frame"),
        TierReason::Fullscreen { .. } | TierReason::HeavyProcess { .. } => {
            Some("something fullscreen wants the card")
        }
        TierReason::VramPressure { .. } => Some("the card is out of memory"),
        TierReason::PowerCritical => Some("the machine is in trouble"),
        _ => None,
    }
}
