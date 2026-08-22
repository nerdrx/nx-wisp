//! Reading the real machine.
//!
//! Every probe is a trait with exactly one job, so the tier ladder can be driven
//! by injected readings and the whole of [`crate::ladder`] is testable with no
//! GPU, no compositor and no clock (SPEC §4).
//!
//! The sysfs implementations take their root as a parameter, so their parsing
//! and — importantly — their **discrete-vs-integrated classification** is tested
//! against synthetic `/sys` trees rather than against whatever card happens to
//! be in the machine running the suite.

pub mod cpu;
pub mod gpu;
pub mod mem;
pub mod power;
pub mod procs;
pub mod selfcost;
pub mod surface;

use crate::{
    reading::{
        CpuReading, GpuReading, MemReading, OperatorReading, PowerReading, ProcReading, Snapshot,
        SurfaceReading,
    },
    Millis,
};

/// Every render-capable card and its live state.
pub trait GpuProbe {
    fn read(&mut self) -> Vec<GpuReading>;
}

/// CPU load and pressure.
pub trait CpuProbe {
    fn read(&mut self) -> CpuReading;
}

/// RAM and memory pressure.
pub trait MemProbe {
    fn read(&mut self) -> MemReading;
}

/// Thermals and battery/AC.
pub trait PowerProbe {
    fn read(&mut self) -> PowerReading;
}

/// Is a game or a VR runtime alive?
pub trait ProcProbe {
    fn read(&mut self) -> ProcReading;
}

/// Is some surface fullscreen? Fed from KWin by `wisp-senses`; `wisp-gov` never
/// speaks D-Bus.
pub trait SurfaceProbe {
    fn read(&mut self) -> SurfaceReading;
}

/// Is the operator there? Fed from `ext_idle_notifier_v1` by `wisp-senses`.
pub trait OperatorProbe {
    fn read(&mut self) -> OperatorReading;
}

/// A source of monotonic milliseconds. Injected so dwell timers are tested
/// deterministically instead of with `sleep`.
pub trait Clock {
    fn now_ms(&self) -> Millis;
}

/// The real one: monotonic since first use, never wall-clock (SPEC §3.2).
pub struct MonotonicClock {
    start: std::time::Instant,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        MonotonicClock {
            start: std::time::Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now_ms(&self) -> Millis {
        self.start.elapsed().as_millis() as Millis
    }
}

/// Anything that can produce one [`Snapshot`]. The governor takes this rather
/// than [`Probes`] directly, so a test can hand it a scripted sequence of
/// readings and drive the whole ladder with no machine underneath.
pub trait SnapshotSource {
    fn snapshot(&mut self) -> Snapshot;
}

/// The bundle of probes the [`crate::Governor`] polls. Any of them may be a
/// fake; see [`crate::fakes`].
pub struct Probes {
    pub clock: Box<dyn Clock + Send>,
    pub gpu: Box<dyn GpuProbe + Send>,
    pub cpu: Box<dyn CpuProbe + Send>,
    pub mem: Box<dyn MemProbe + Send>,
    pub power: Box<dyn PowerProbe + Send>,
    pub procs: Box<dyn ProcProbe + Send>,
    pub surface: Box<dyn SurfaceProbe + Send>,
    pub operator: Box<dyn OperatorProbe + Send>,
}

impl Probes {
    /// Everything reading the real machine.
    pub fn real(cfg: &crate::config::GovConfig) -> Self {
        Probes {
            clock: Box::new(MonotonicClock::default()),
            gpu: Box::new(gpu::SysfsGpuProbe::default()),
            cpu: Box::new(cpu::ProcCpuProbe::default()),
            mem: Box::new(mem::ProcMemProbe::default()),
            power: Box::new(power::SysfsPowerProbe::default()),
            procs: Box::new(procs::ProcfsProcProbe::new(cfg.procs.clone())),
            surface: Box::new(surface::FullscreenHook::default()),
            operator: Box::new(surface::OperatorHook::default()),
        }
    }

    /// Poll everything once into one [`Snapshot`].
    pub fn poll(&mut self) -> Snapshot {
        Snapshot {
            at: self.clock.now_ms(),
            gpus: self.gpu.read(),
            cpu: self.cpu.read(),
            mem: self.mem.read(),
            power: self.power.read(),
            procs: self.procs.read(),
            surface: self.surface.read(),
            operator: self.operator.read(),
        }
    }
}

impl SnapshotSource for Probes {
    fn snapshot(&mut self) -> Snapshot {
        self.poll()
    }
}

// --- small shared helpers ---------------------------------------------------

pub(crate) fn read_trimmed(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

pub(crate) fn read_u64(path: &std::path::Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

/// Parse `0x1002` or `1002` into a PCI id.
pub(crate) fn parse_hex_id(s: &str) -> Option<u32> {
    let s = s.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    u32::from_str_radix(s, 16).ok()
}

pub(crate) const MIB: u64 = 1024 * 1024;

/// `some avg10=1.23 ...` → `1.23`.
pub(crate) fn parse_psi_some_avg10(contents: &str) -> f32 {
    for line in contents.lines() {
        let Some(rest) = line.strip_prefix("some ") else {
            continue;
        };
        for field in rest.split_whitespace() {
            if let Some(v) = field.strip_prefix("avg10=") {
                return v.parse().unwrap_or(0.0);
            }
        }
    }
    0.0
}
