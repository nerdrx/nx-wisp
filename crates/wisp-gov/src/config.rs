//! Every threshold and every timer the governor uses. Nothing here is a magic
//! number buried in a match arm, and **nothing here is specific to the
//! operator's desktop** — the defaults are ratios and percentages, and the
//! absolute numbers (VRAM, cores) come from the [`crate::reading::Snapshot`].

use serde::{Deserialize, Serialize};
use wisp_proto::Tier;

use crate::Millis;

/// How long a more-capable tier must be justified before we actually take it.
///
/// SPEC §3.1: *downgrades are applied synchronously and immediately; upgrades
/// are lazy.* These are the laziness. Indexed by the tier we would move **to**,
/// so climbing all the way back to Feral is deliberately the slowest move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DwellConfig {
    /// Dwell before entering T0 Feral.
    pub to_feral_ms: Millis,
    /// Dwell before entering T1 Full — the one the operator feels. A game exit
    /// should bring her back within a couple of seconds, not instantly (Alt-Tab
    /// out of a game and straight back must not cost a model load).
    pub to_full_ms: Millis,
    /// Dwell before entering T2 Reduced.
    pub to_reduced_ms: Millis,
    /// Dwell before climbing out of Dormant into Lobotomised.
    pub to_lobotomised_ms: Millis,
}

impl Default for DwellConfig {
    fn default() -> Self {
        DwellConfig {
            to_feral_ms: 60_000,
            to_full_ms: 8_000,
            to_reduced_ms: 4_000,
            to_lobotomised_ms: 2_000,
        }
    }
}

impl DwellConfig {
    pub fn for_tier(&self, tier: Tier) -> Millis {
        match tier {
            Tier::Feral => self.to_feral_ms,
            Tier::Full => self.to_full_ms,
            Tier::Reduced => self.to_reduced_ms,
            Tier::Lobotomised => self.to_lobotomised_ms,
            // There is no such thing as a lazy move *into* Dormant.
            Tier::Dormant => 0,
        }
    }

    /// Zero dwell everywhere. Only for tests that are not testing hysteresis.
    pub const INSTANT: DwellConfig = DwellConfig {
        to_feral_ms: 0,
        to_full_ms: 0,
        to_reduced_ms: 0,
        to_lobotomised_ms: 0,
    };
}

/// How often the governor should be polled, per tier.
///
/// Polling is not free. Measured on the operator's desktop (release build,
/// ~1000 processes): the `/sys/class/drm` sweep costs ~130 µs, the `/proc` sweep
/// ~2.9 ms, a whole [`crate::probe::Probes::poll`] ~4 ms. At 1 Hz that is 0.4%
/// of one core; at 4 Hz it would be 1.6%, which on its own would blow the T3
/// budget of ~0.5% before she had done anything at all.
///
/// So the governor slows down as it gets cheaper to be wrong. At T3 the only
/// thing it is waiting for is the game to exit, and it can afford to notice that
/// two seconds late; the *downgrade* direction is never late, because a
/// fullscreen surface and a VR session both arrive as pushed events on
/// [`crate::probe::surface::FullscreenHook`] and
/// [`crate::probe::surface::VrSessionHint`] rather than being waited for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cadence {
    pub t0_ms: Millis,
    pub t1_ms: Millis,
    pub t2_ms: Millis,
    pub t3_ms: Millis,
    pub t4_ms: Millis,
}

impl Default for Cadence {
    fn default() -> Self {
        Cadence {
            t0_ms: 2_000,
            t1_ms: 1_000,
            t2_ms: 1_000,
            t3_ms: 2_000,
            t4_ms: 5_000,
        }
    }
}

impl Cadence {
    pub fn for_tier(&self, tier: Tier) -> Millis {
        match tier {
            Tier::Feral => self.t0_ms,
            Tier::Full => self.t1_ms,
            Tier::Reduced => self.t2_ms,
            Tier::Lobotomised => self.t3_ms,
            Tier::Dormant => self.t4_ms,
        }
    }
}

/// A two-level threshold. `enter` trips the pressure on, `release` trips it off,
/// and `release < enter` is what stops a metric hovering on a boundary from
/// flapping the tier even before the dwell timer gets involved.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Band {
    pub enter: f32,
    pub release: f32,
}

impl Band {
    pub const fn new(enter: f32, release: f32) -> Self {
        Band { enter, release }
    }
    /// `already` is whether we are currently in the pressured state.
    pub fn tripped(&self, value: f32, already: bool) -> bool {
        if already {
            value > self.release
        } else {
            value >= self.enter
        }
    }
}

/// Names and path fragments that identify heavy processes. Entirely data, so
/// the operator can add their own without a rebuild, and so the process probe
/// has no hardcoded opinions about which games exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSignatures {
    /// Substrings matched case-insensitively against the process name.
    pub vr_wivrn: Vec<String>,
    pub vr_monado: Vec<String>,
    pub vr_steamvr: Vec<String>,
    pub vr_alvr: Vec<String>,
    /// Extra VR runtimes the operator added.
    pub vr_other: Vec<String>,
    /// Exact-ish game names the operator listed.
    pub games: Vec<String>,
    /// Path fragments in `/proc/<pid>/cmdline` that mean "this is a game".
    pub game_paths: Vec<String>,
    /// Launchers whose presence implies a game is running under them.
    pub game_launchers: Vec<String>,
    /// Never treated as heavy, whatever they do. Our own binary lives here.
    pub ignore: Vec<String>,
}

impl Default for ProcessSignatures {
    fn default() -> Self {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        ProcessSignatures {
            vr_wivrn: s(&["wivrn-server", "wivrn"]),
            vr_monado: s(&["monado-service", "monado"]),
            vr_steamvr: s(&["vrserver", "vrcompositor", "vrmonitor", "vrdashboard"]),
            vr_alvr: s(&["alvr_server", "alvr-server", "alvr_dashboard"]),
            vr_other: vec![],
            games: vec![],
            game_paths: s(&[
                "/steamapps/common/",
                "/steamapps/shadercache/",
                "proton",
                "wine64-preloader",
                "/lutris/",
                "/heroic/",
                "/Games/",
            ]),
            game_launchers: s(&["gamescope", "gamemoderun", "lutris-wrapper", "heroic"]),
            ignore: s(&["nx-wisp", "wisp"]),
        }
    }
}

/// The governor's whole policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GovConfig {
    pub dwell: DwellConfig,
    pub cadence: Cadence,

    /// Discrete-GPU busy percentage that means "something big is rendering".
    pub gpu_busy: Band,
    /// Fraction of VRAM in use that means we are crowding somebody out.
    pub vram_used_frac: Band,
    /// Absolute VRAM headroom we insist on leaving free on the discrete card,
    /// in MiB. Whichever of this and `vram_used_frac` trips first wins, so the
    /// policy is right on a 2 GiB laptop dGPU and on a 24 GiB desktop one.
    pub vram_headroom_mib: u64,
    /// 1-minute load average per core.
    pub cpu_load_per_core: Band,
    /// `/proc/pressure/cpu` `some avg10`.
    pub cpu_psi: Band,
    /// `/proc/pressure/memory` `some avg10`. Memory pressure is the one thing
    /// that can make a whole desktop stutter, so it is treated harshly.
    pub mem_psi: Band,

    /// Any sensor at or above this is an emergency: T4 Dormant.
    pub temp_critical_c: i16,
    /// Battery percentage at or below which, while discharging, she goes T4.
    pub battery_critical_pct: u8,
    /// While on battery and below this, she never climbs above T2.
    pub battery_frugal_pct: u8,

    /// Idle time after which the operator counts as away, unlocking T0 Feral.
    pub away_after_ms: Millis,

    /// VRAM the reflex model is allowed to hold on the discrete card at T2.
    pub reduced_vram_budget_mib: u64,
    /// VRAM the sprite-atlas rig (F71) may hold at T3, on whichever card it is
    /// hiding on. Never spent on the discrete card unless it is the only one.
    pub lobotomised_vram_budget_mib: u64,
    /// Smallest integrated-GPU VRAM that is worth running inference on. Below
    /// this the reflex model stays on the CPU rather than thrashing a 512 MiB
    /// carve-out.
    pub igpu_inference_min_mib: u64,

    pub procs: ProcessSignatures,
}

impl Default for GovConfig {
    fn default() -> Self {
        GovConfig {
            dwell: DwellConfig::default(),
            cadence: Cadence::default(),
            gpu_busy: Band::new(45.0, 25.0),
            vram_used_frac: Band::new(0.85, 0.70),
            vram_headroom_mib: 1536,
            cpu_load_per_core: Band::new(0.70, 0.45),
            cpu_psi: Band::new(20.0, 8.0),
            mem_psi: Band::new(10.0, 3.0),
            temp_critical_c: 95,
            battery_critical_pct: 10,
            battery_frugal_pct: 30,
            away_after_ms: 120_000,
            reduced_vram_budget_mib: 2048,
            lobotomised_vram_budget_mib: 256,
            igpu_inference_min_mib: 1024,
            procs: ProcessSignatures::default(),
        }
    }
}

impl GovConfig {
    /// Same policy, no dwell. For tests that are exercising [`crate::ladder`]'s
    /// classification rather than its hysteresis.
    pub fn instant() -> Self {
        GovConfig {
            dwell: DwellConfig::INSTANT,
            ..GovConfig::default()
        }
    }
}
