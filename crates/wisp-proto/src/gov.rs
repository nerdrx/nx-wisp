//! SPEC §3.1 — the governor's verdict and the trait every subsystem honours.

use serde::{Deserialize, Serialize};

/// How much of the machine she is allowed to be right now.
///
/// Ordering is meaningful: `T0` is the most capable, `T4` the least, and
/// `Tier as u8` is stable and used in the flight recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum Tier {
    /// Machine idle and the operator away. Background cognition permitted.
    Feral = 0,
    /// Operator present, nothing heavy running.
    Full = 1,
    /// Something substantial started. Reflex model only; deliberate model
    /// evicted from VRAM but kept mmapped so the way back is ~1s.
    Reduced = 2,
    /// A game or a VR session owns the GPU. Zero dGPU use, model fully
    /// unloaded, behaviour trees and canned speech only.
    Lobotomised = 3,
    /// Explicitly silenced, or thermal/battery critical.
    Dormant = 4,
}

impl Tier {
    /// May she use the discrete GPU at all?
    pub fn may_use_dgpu(self) -> bool {
        matches!(self, Tier::Feral | Tier::Full | Tier::Reduced)
    }
    /// May a language model be resident in VRAM?
    pub fn may_hold_model(self) -> bool {
        matches!(self, Tier::Feral | Tier::Full | Tier::Reduced)
    }
    /// May she start new cognition, or must it be deferred (SPEC §3.5)?
    pub fn may_think(self) -> bool {
        matches!(self, Tier::Feral | Tier::Full | Tier::Reduced)
    }
    /// Target frame rate for the rig.
    pub fn target_fps(self) -> u32 {
        match self {
            Tier::Feral | Tier::Full => 60,
            Tier::Reduced => 30,
            Tier::Lobotomised => 15,
            Tier::Dormant => 0,
        }
    }
}

/// Why the governor chose the tier it chose. Carried into the flight recorder so
/// "she's at T3 because WiVRn is streaming" is answerable from data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TierReason {
    /// Nothing notable; the machine is quiet.
    Idle,
    /// The operator pinned this tier by hand.
    Pinned,
    /// A fullscreen surface owns an output.
    Fullscreen { app_id: String },
    /// A known game or VR runtime is running.
    HeavyProcess { name: String },
    /// A WiVRn session is streaming to a headset.
    VrSession,
    /// Sustained GPU utilisation above the threshold.
    GpuPressure { busy_pct: u8 },
    /// VRAM headroom exhausted.
    VramPressure { used_mib: u64, total_mib: u64 },
    /// Thermal or battery emergency.
    PowerCritical,
}

/// Worst-case resident cost of a subsystem at a tier. Used for accounting and
/// for the operator-facing cost meter, not for scheduling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
    pub ram_mib: u32,
    pub vram_mib: u32,
    /// Hundredths of one percent of a single core, so `50` == 0.5% of a core
    /// and `10_000` == one core fully saturated.
    pub cpu_centi_pct: u32,
}

impl Cost {
    pub const FREE: Cost = Cost { ram_mib: 0, vram_mib: 0, cpu_centi_pct: 0 };
}

impl std::ops::Add for Cost {
    type Output = Cost;
    fn add(self, o: Cost) -> Cost {
        // Saturating: a debug build must not panic while merely *accounting*
        // for costs. An implausible total is a reporting bug; a crash in the
        // governor is a product failure.
        Cost {
            ram_mib: self.ram_mib.saturating_add(o.ram_mib),
            vram_mib: self.vram_mib.saturating_add(o.vram_mib),
            cpu_centi_pct: self.cpu_centi_pct.saturating_add(o.cpu_centi_pct),
        }
    }
}

/// Implemented by every subsystem that can cost the machine anything.
///
/// **Downgrades are synchronous, immediate and infallible.** A subsystem that
/// cannot honour a downgrade sheds the work rather than queueing it — the sole
/// exception is `wisp-mind`, whose deferred queue is specified in SPEC §3.5.
pub trait Governed {
    fn set_tier(&mut self, tier: Tier, reason: &TierReason);
    fn cost_at(tier: Tier) -> Cost
    where
        Self: Sized;
}
