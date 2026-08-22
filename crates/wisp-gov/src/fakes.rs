//! Injectable readings.
//!
//! SPEC §4: *pure modules — including the tier ladder — are unit-tested with no
//! GPU and no compositor.* This module is how. It is public rather than
//! `cfg(test)` because `wisp-mind`, `wisp-voice` and `wisp-paint` all have to be
//! able to test their own [`wisp_proto::Governed`] behaviour under a scripted
//! tier trajectory without a machine underneath.
//!
//! Nothing here hardcodes the operator's desktop. [`two_gpu_machine`] takes the
//! VRAM sizes; [`Machine::desktop`] and [`Machine::laptop`] are convenience
//! presets built from that same parameterised constructor.

use std::path::PathBuf;

use crate::{
    probe::SnapshotSource,
    reading::{
        CpuReading, FullscreenSurface, GpuId, GpuKind, GpuReading, HeavyProc, HeavySource,
        MemReading, OperatorReading, PowerReading, ProcReading, Snapshot, SurfaceReading,
        VrRuntime, VrRuntimeKind,
    },
    Millis,
};

/// One discrete card and one integrated card, in PCI order, with the render
/// nodes deliberately crossed over the way they really are on the operator's
/// desktop (`card1` owns `renderD128`).
pub fn two_gpu_machine(dgpu_vram_mib: u64, igpu_vram_mib: u64) -> Vec<GpuReading> {
    vec![
        GpuReading {
            id: GpuId {
                card_index: 1,
                driver: "amdgpu".into(),
                pci_slot: "0000:03:00.0".into(),
                render_node: Some(PathBuf::from("/dev/dri/renderD128")),
                vendor_id: 0x1002,
                device_id: 0x744c,
                kind: GpuKind::Discrete,
                enumeration_index: 0,
            },
            busy_pct: 0,
            vram_used_mib: 512,
            vram_total_mib: dgpu_vram_mib,
            gtt_used_mib: 0,
            temp_c: Some(45),
        },
        GpuReading {
            id: GpuId {
                card_index: 0,
                driver: "amdgpu".into(),
                pci_slot: "0000:7b:00.0".into(),
                render_node: Some(PathBuf::from("/dev/dri/renderD129")),
                vendor_id: 0x1002,
                device_id: 0x13c0,
                kind: GpuKind::Integrated,
                enumeration_index: 1,
            },
            busy_pct: 0,
            vram_used_mib: 20,
            vram_total_mib: igpu_vram_mib,
            gtt_used_mib: 0,
            temp_c: Some(40),
        },
    ]
}

/// A single discrete card and nothing to hide on.
pub fn one_gpu_machine(vram_mib: u64) -> Vec<GpuReading> {
    let mut v = two_gpu_machine(vram_mib, 0);
    v.truncate(1);
    v
}

/// Builds [`Snapshot`]s. Every method mutates and returns `self`, so a test
/// reads as the story it is telling.
#[derive(Debug, Clone)]
pub struct Machine {
    snap: Snapshot,
}

impl Machine {
    /// A quiet two-GPU desktop with the operator at the keyboard. Shaped like
    /// the operator's machine but with the sizes passed in.
    pub fn new(cores: u32, ram_mib: u64, gpus: Vec<GpuReading>) -> Self {
        Machine {
            snap: Snapshot {
                at: 0,
                gpus,
                cpu: CpuReading {
                    cores,
                    load1: 0.2,
                    busy_pct: Some(3),
                    psi_some_avg10: 0.0,
                },
                mem: MemReading {
                    total_mib: ram_mib,
                    available_mib: ram_mib * 3 / 4,
                    psi_some_avg10: 0.0,
                },
                power: PowerReading {
                    cpu_temp_c: Some(45),
                    gpu_temp_c: Some(45),
                    on_ac: true,
                    battery_pct: None,
                    battery_discharging: false,
                },
                procs: ProcReading::default(),
                surface: SurfaceReading::default(),
                operator: OperatorReading {
                    idle_ms: 0,
                    locked: false,
                },
            },
        }
    }

    /// 32 threads, 60 GiB, 24 GiB dGPU + 2 GiB iGPU.
    pub fn desktop() -> Self {
        Machine::new(32, 61_820, two_gpu_machine(24_560, 2_048))
    }

    /// 8 threads, 16 GiB, one 6 GiB dGPU and an iGPU, on battery.
    pub fn laptop() -> Self {
        let mut m = Machine::new(8, 15_800, two_gpu_machine(6_144, 512));
        m.snap.power.battery_pct = Some(80);
        m.snap.power.on_ac = false;
        m.snap.power.battery_discharging = true;
        m
    }

    /// One card, nowhere to hide.
    pub fn single_gpu() -> Self {
        Machine::new(16, 32_000, one_gpu_machine(8_192))
    }

    pub fn at(mut self, ms: Millis) -> Self {
        self.snap.at = ms;
        self
    }
    pub fn advance(mut self, ms: Millis) -> Self {
        self.snap.at += ms;
        self
    }
    pub fn gpu_busy(mut self, pct: u8) -> Self {
        if let Some(g) = self.snap.gpus.first_mut() {
            g.busy_pct = pct;
        }
        self
    }
    pub fn vram_used(mut self, mib: u64) -> Self {
        if let Some(g) = self.snap.gpus.first_mut() {
            g.vram_used_mib = mib;
        }
        self
    }
    pub fn load(mut self, load1: f32) -> Self {
        self.snap.cpu.load1 = load1;
        self
    }
    pub fn cpu_psi(mut self, psi: f32) -> Self {
        self.snap.cpu.psi_some_avg10 = psi;
        self
    }
    pub fn mem_psi(mut self, psi: f32) -> Self {
        self.snap.mem.psi_some_avg10 = psi;
        self
    }
    pub fn temp(mut self, c: i16) -> Self {
        self.snap.power.gpu_temp_c = Some(c);
        self
    }
    pub fn battery(mut self, pct: u8, discharging: bool) -> Self {
        self.snap.power.battery_pct = Some(pct);
        self.snap.power.battery_discharging = discharging;
        self.snap.power.on_ac = !discharging;
        self
    }
    pub fn idle_ms(mut self, ms: Millis) -> Self {
        self.snap.operator.idle_ms = ms;
        self
    }
    pub fn locked(mut self, locked: bool) -> Self {
        self.snap.operator.locked = locked;
        self
    }
    /// WiVRn is up and streaming to a headset.
    pub fn wivrn_streaming(self) -> Self {
        self.vr(VrRuntimeKind::WiVRn, "wivrn-server", true)
    }
    /// WiVRn is up but no headset has connected yet.
    pub fn wivrn_idle(self) -> Self {
        self.vr(VrRuntimeKind::WiVRn, "wivrn-server", false)
    }
    pub fn vr(mut self, kind: VrRuntimeKind, proc_name: &str, streaming: bool) -> Self {
        self.snap.procs.vr = Some(VrRuntime {
            kind,
            proc_name: proc_name.to_string(),
            pid: 4242,
            streaming,
        });
        self
    }
    pub fn no_vr(mut self) -> Self {
        self.snap.procs.vr = None;
        self
    }
    pub fn game(mut self, name: &str) -> Self {
        self.snap.procs.game = Some(HeavyProc {
            name: name.to_string(),
            pid: 1234,
            cpu_pct: 600,
            source: HeavySource::SteamLibrary,
        });
        self
    }
    pub fn no_game(mut self) -> Self {
        self.snap.procs.game = None;
        self
    }
    pub fn top_cpu(mut self, name: &str, pct: u16) -> Self {
        self.snap.procs.top_cpu = Some(HeavyProc {
            name: name.to_string(),
            pid: 99,
            cpu_pct: pct,
            source: HeavySource::CpuHog,
        });
        self
    }
    pub fn fullscreen(mut self, app_id: &str) -> Self {
        self.snap.surface.fullscreen = Some(FullscreenSurface {
            app_id: app_id.to_string(),
            title: app_id.to_string(),
            output: "DP-1".to_string(),
            since: self.snap.at,
            active: true,
        });
        self
    }
    pub fn windowed(mut self) -> Self {
        self.snap.surface.fullscreen = None;
        self
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snap.clone()
    }
    pub fn build(self) -> Snapshot {
        self.snap
    }
}

/// Hands back a scripted list of snapshots, then repeats the last one forever.
/// Implements [`SnapshotSource`], so it can drive a whole [`crate::Governor`].
#[derive(Debug, Clone)]
pub struct Replay {
    frames: Vec<Snapshot>,
    at: usize,
}

impl Replay {
    pub fn new(frames: Vec<Snapshot>) -> Self {
        assert!(!frames.is_empty(), "a Replay needs at least one frame");
        Replay { frames, at: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.frames.len().saturating_sub(self.at)
    }
}

impl SnapshotSource for Replay {
    fn snapshot(&mut self) -> Snapshot {
        let i = self.at.min(self.frames.len() - 1);
        self.at += 1;
        self.frames[i].clone()
    }
}

/// A [`wisp_proto::Governed`] that just records what it was told, so tests can
/// assert that a downgrade really did reach every subsystem before `step`
/// returned.
#[derive(Debug, Default)]
pub struct Spy {
    pub calls: Vec<(wisp_proto::Tier, wisp_proto::TierReason)>,
}

impl Spy {
    pub fn new() -> Self {
        Spy::default()
    }
    pub fn tiers(&self) -> Vec<wisp_proto::Tier> {
        self.calls.iter().map(|(t, _)| *t).collect()
    }
}

fn default_cost(tier: wisp_proto::Tier) -> wisp_proto::Cost {
    use wisp_proto::{Cost, Tier};
    match tier {
        Tier::Feral => Cost { ram_mib: 400, vram_mib: 18_000, cpu_centi_pct: 500 },
        Tier::Full => Cost { ram_mib: 120, vram_mib: 3_000, cpu_centi_pct: 200 },
        Tier::Reduced => Cost { ram_mib: 90, vram_mib: 2_000, cpu_centi_pct: 100 },
        Tier::Lobotomised => Cost { ram_mib: 55, vram_mib: 0, cpu_centi_pct: 50 },
        Tier::Dormant => Cost::FREE,
    }
}

impl wisp_proto::Governed for Spy {
    fn set_tier(&mut self, tier: wisp_proto::Tier, reason: &wisp_proto::TierReason) {
        self.calls.push((tier, reason.clone()));
    }
    fn cost_at(tier: wisp_proto::Tier) -> wisp_proto::Cost {
        default_cost(tier)
    }
}
