//! What the probes saw. Plain data, no logic, no I/O.
//!
//! A [`Snapshot`] is the *entire* input to the tier ladder. If a decision needs
//! a fact, the fact goes here first — that is what keeps [`crate::ladder`] pure
//! and what lets the tests drive the governor with no GPU at all.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Millis;

/// Everything the governor knows about the machine at one instant.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Monotonic ms since process start. Drives the dwell timers.
    pub at: Millis,
    /// Every render-capable DRM card found, in stable PCI order.
    pub gpus: Vec<GpuReading>,
    pub cpu: CpuReading,
    pub mem: MemReading,
    pub power: PowerReading,
    pub procs: ProcReading,
    pub surface: SurfaceReading,
    pub operator: OperatorReading,
}

impl Snapshot {
    /// The card we would like to use when we are allowed to be expensive.
    pub fn discrete(&self) -> Option<&GpuReading> {
        self.gpus.iter().find(|g| g.id.kind == GpuKind::Discrete)
    }
    /// The card we hide on when the operator is gaming.
    pub fn integrated(&self) -> Option<&GpuReading> {
        self.gpus.iter().find(|g| g.id.kind == GpuKind::Integrated)
    }
    /// The card the governor watches for pressure: the discrete one if there is
    /// one, then any card we could not identify (which we always treat as
    /// discrete rather than risk borrowing it), then whatever single card this
    /// machine has — a laptop APU, say.
    pub fn primary(&self) -> Option<&GpuReading> {
        self.discrete()
            .or_else(|| self.gpus.iter().find(|g| g.id.kind == GpuKind::Unknown))
            .or_else(|| self.gpus.first())
    }
}

// ---------------------------------------------------------------------------
// GPU
// ---------------------------------------------------------------------------

/// Discrete or integrated. Decided by [`crate::probe::gpu`] from a weighted set
/// of sysfs signals — never by card index, which is not stable across machines
/// (on the operator's desktop the *integrated* Radeon is `card0`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuKind {
    /// A card with its own memory, its own fan and its own power budget.
    Discrete,
    /// A card carving VRAM out of system RAM.
    Integrated,
    /// Signals disagreed. Treated as discrete for safety: we would rather leave
    /// a card alone than accidentally render a game's worth of work on it.
    #[default]
    Unknown,
}

impl GpuKind {
    /// May we treat this card as "free to use while the operator is gaming"?
    pub fn is_safe_to_borrow(self) -> bool {
        matches!(self, GpuKind::Integrated)
    }
}

/// Stable identity of one render-capable card.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuId {
    /// The `N` in `/sys/class/drm/cardN`. Informational only.
    pub card_index: u32,
    /// `amdgpu`, `i915`, `xe`, `nvidia`, `nouveau`, ...
    pub driver: String,
    /// `0000:03:00.0`. This, not the card index, is the identity.
    pub pci_slot: String,
    /// `/dev/dri/renderD128`. Note this does **not** track the card index — on
    /// the operator's desktop `card1` (the 7900 XTX) owns `renderD128`.
    pub render_node: Option<PathBuf>,
    /// PCI vendor id, e.g. `0x1002` for AMD. Matches `wgpu::AdapterInfo::vendor`.
    pub vendor_id: u32,
    /// PCI device id, e.g. `0x744c` for Navi 31. Matches `wgpu::AdapterInfo::device`.
    pub device_id: u32,
    pub kind: GpuKind,
    /// Index into the PCI-sorted card list. Best-effort proxy for the order a
    /// Vulkan loader will enumerate physical devices in; see
    /// [`crate::device::GpuTarget::index_in`] for the exact answer.
    pub enumeration_index: usize,
}

/// One card's live state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuReading {
    pub id: GpuId,
    /// `gpu_busy_percent`, 0..=100.
    pub busy_pct: u8,
    pub vram_used_mib: u64,
    pub vram_total_mib: u64,
    /// GTT (system memory the GPU has mapped). Informational.
    pub gtt_used_mib: u64,
    /// Hottest reported sensor on the card, °C.
    pub temp_c: Option<i16>,
}

impl GpuReading {
    pub fn vram_free_mib(&self) -> u64 {
        self.vram_total_mib.saturating_sub(self.vram_used_mib)
    }
    pub fn vram_used_pct(&self) -> u8 {
        if self.vram_total_mib == 0 {
            return 0;
        }
        ((self.vram_used_mib.min(self.vram_total_mib) * 100) / self.vram_total_mib) as u8
    }
}

// ---------------------------------------------------------------------------
// CPU / RAM
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CpuReading {
    /// Logical cores. Never assumed; read from the machine.
    pub cores: u32,
    /// 1-minute load average.
    pub load1: f32,
    /// Aggregate busy percentage across all cores, 0..=100, from `/proc/stat`
    /// deltas. `None` on the very first sample, when there is no delta yet.
    pub busy_pct: Option<u8>,
    /// `/proc/pressure/cpu` `some avg10`. The honest "is anything waiting" number.
    pub psi_some_avg10: f32,
}

impl CpuReading {
    /// Load normalised per core, so one number means the same thing on the
    /// operator's 32-thread desktop and on their laptop.
    pub fn load_per_core(&self) -> f32 {
        if self.cores == 0 {
            return 0.0;
        }
        self.load1 / self.cores as f32
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemReading {
    pub total_mib: u64,
    /// `MemAvailable`, which is the only field that means anything useful.
    pub available_mib: u64,
    /// `/proc/pressure/memory` `some avg10`. Above ~10 the machine is thrashing.
    pub psi_some_avg10: f32,
}

impl MemReading {
    pub fn available_pct(&self) -> u8 {
        if self.total_mib == 0 {
            return 100;
        }
        ((self.available_mib.min(self.total_mib) * 100) / self.total_mib) as u8
    }
}

// ---------------------------------------------------------------------------
// Power / thermals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerReading {
    pub cpu_temp_c: Option<i16>,
    pub gpu_temp_c: Option<i16>,
    /// True on a desktop, or on a laptop with the charger in.
    pub on_ac: bool,
    /// `None` on a desktop. Peripheral batteries (mice, headsets) are excluded
    /// by their `scope=Device` attribute — the operator's mouse must never put
    /// the wisp into T4.
    pub battery_pct: Option<u8>,
    pub battery_discharging: bool,
}

impl Default for PowerReading {
    fn default() -> Self {
        // A machine with no power_supply class at all is a desktop on mains.
        PowerReading {
            cpu_temp_c: None,
            gpu_temp_c: None,
            on_ac: true,
            battery_pct: None,
            battery_discharging: false,
        }
    }
}

impl PowerReading {
    pub fn hottest_c(&self) -> Option<i16> {
        match (self.cpu_temp_c, self.gpu_temp_c) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VrRuntimeKind {
    /// The operator's own VR streaming server. This one matters most.
    WiVRn,
    Monado,
    SteamVr,
    Alvr,
    Other,
}

impl VrRuntimeKind {
    /// Name as it should appear in the live readout the operator reads.
    pub fn display(self) -> &'static str {
        match self {
            VrRuntimeKind::WiVRn => "WiVRn",
            VrRuntimeKind::Monado => "Monado",
            VrRuntimeKind::SteamVr => "SteamVR",
            VrRuntimeKind::Alvr => "ALVR",
            VrRuntimeKind::Other => "a VR runtime",
        }
    }
}

/// A VR runtime that is alive. Alive alone is enough for T3 — the charter says
/// she costs nothing *when it matters*, and a headset about to be put on is
/// exactly when it matters. `streaming` only refines the wording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VrRuntime {
    pub kind: VrRuntimeKind,
    /// The process name as found, e.g. `wivrn-server`.
    pub proc_name: String,
    pub pid: u32,
    /// A headset is actually connected and frames are moving.
    pub streaming: bool,
}

/// Where the belief that a process is heavy came from. Kept so the flight
/// recorder can answer "why did you think that was a game?".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeavySource {
    /// Matched the operator's configured game list.
    KnownName,
    /// Launched out of a Steam library path.
    SteamLibrary,
    /// Running under Proton/Wine.
    Proton,
    /// Under gamescope, Lutris, Heroic or Bottles.
    GameLauncher,
    /// Nothing matched a signature; it is just eating the machine.
    CpuHog,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeavyProc {
    pub name: String,
    pub pid: u32,
    /// Percentage of one core, so 1600 is possible on a 16-thread box.
    pub cpu_pct: u16,
    pub source: HeavySource,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcReading {
    /// The most significant VR runtime found, if any.
    pub vr: Option<VrRuntime>,
    /// A process believed to be a game.
    pub game: Option<HeavyProc>,
    /// The single biggest CPU consumer that is not us. Used to *name* CPU
    /// pressure in the readout — "T2 because cargo is eating the CPU".
    pub top_cpu: Option<HeavyProc>,
}

// ---------------------------------------------------------------------------
// Surfaces (fed by wisp-senses from the KWin script — F21/F4)
// ---------------------------------------------------------------------------

/// A fullscreen toplevel, as reported by KWin's D-Bus scripting interface.
///
/// `wisp-gov` does not talk to KWin. `wisp-senses` owns that and pushes into
/// [`crate::probe::surface::FullscreenHook`]; this is the agreed input type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullscreenSurface {
    /// Wayland `app_id`, e.g. `steam_app_1091500`. May be empty for XWayland.
    pub app_id: String,
    pub title: String,
    /// KWin's output name, e.g. `DP-1`.
    pub output: String,
    /// Monotonic ms at which it went fullscreen.
    pub since: Millis,
    /// KWin reports the window is actively rendering rather than idle-fullscreen
    /// (a paused video player, say). Advisory.
    pub active: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceReading {
    pub fullscreen: Option<FullscreenSurface>,
}

// ---------------------------------------------------------------------------
// Operator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorReading {
    /// From `ext_idle_notifier_v1` via wisp-senses. 0 means "just moved".
    pub idle_ms: Millis,
    /// The session is locked. Stronger signal than idle time.
    pub locked: bool,
}
