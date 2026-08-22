//! `wisp-gov` — the elastic resource governor. SPEC.md §0.1 and §3.1.
//!
//! > *She costs nothing when it matters.*
//!
//! This crate is the only place in the tree that is allowed to decide how much
//! of the machine NX Wisp may be. Everything else asks.
//!
//! The shape of it:
//!
//! ```text
//!   probes (real sysfs / procfs)          injected fakes (tests)
//!            \                                   /
//!             +--------------> Snapshot <-------+
//!                                  |
//!                            ladder::classify   (pure, no GPU, no clock)
//!                                  |
//!                            Ladder::step       (asymmetric hysteresis)
//!                                  |
//!            +---------------------+----------------------+
//!            |                     |                      |
//!      Registry::apply       device::select          vram::budget
//!   (Governed, sync on      (F61 dGPU <-> iGPU)     (F13 evict before
//!    downgrade)                                      it steals frames)
//! ```
//!
//! Every decision-making path here is a pure function over [`Snapshot`], so the
//! whole ladder is testable with no GPU, no compositor and no clock. The only
//! impure parts are the [`probe`] implementations, each of which sits behind a
//! trait so tests inject readings instead.
//!
//! ## Feature map (nx-wisp-plan.md §3.5)
//!
//! | Feature | Module |
//! |---|---|
//! | F13 VRAM budget manager | [`vram`] |
//! | F60 Tier ladder | [`ladder`] |
//! | F61 Two-GPU trick | [`device`] |
//! | F65 Hard ceilings | [`ceiling`] |
//! | F66 Cost meter | [`accounting`] |

pub mod accounting;
pub mod ceiling;
pub mod config;
pub mod device;
pub mod fakes;
pub mod governor;
pub mod ladder;
pub mod probe;
pub mod reading;
pub mod registry;
pub mod vram;

pub use accounting::{CostReport, FreeThresholds};
pub use ceiling::{EffectiveLimits, SchedClass, UnitSpec};
pub use config::{Band, Cadence, DwellConfig, GovConfig, ProcessSignatures};
pub use device::{DeviceChoice, GpuTarget};
pub use governor::{Governor, Step};
pub use ladder::{Ladder, TierChange, Verdict};
pub use probe::{selfcost::MeasuredCost, Probes, SnapshotSource};
pub use reading::{
    CpuReading, FullscreenSurface, GpuId, GpuKind, GpuReading, HeavyProc, HeavySource, MemReading,
    OperatorReading, PowerReading, ProcReading, Snapshot, SurfaceReading, VrRuntime, VrRuntimeKind,
};
pub use registry::{Registry, Shared};
pub use vram::VramBudget;

/// Monotonic milliseconds, from `wisp-proto`. Never wall-clock (SPEC §3.2).
pub type Millis = wisp_proto::Millis;

/// Everything that can go wrong reading the machine. Probe failure is never
/// fatal: the governor degrades to the most conservative reading it can defend.
#[derive(Debug, thiserror::Error)]
pub enum GovError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} did not parse as {want}: {got:?}")]
    Parse {
        path: String,
        want: &'static str,
        got: String,
    },
    #[error("no GPU found under {0}")]
    NoGpu(String),
}

pub type Result<T> = std::result::Result<T, GovError>;
