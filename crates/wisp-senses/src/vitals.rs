//! F27 — machine vitals. The one sense that polls, so it is the one that has to
//! be cheap.
//!
//! Everything comes from `procfs`/`sysfs` reads of files that are a few dozen
//! bytes each. No `lm_sensors`, no `nvidia-smi`, no subprocess: a poll is a
//! handful of `read(2)`s on tmpfs-backed pseudo-files, which is why it can run
//! at T1 without registering.
//!
//! Every path is resolved once at start-up and every parse is a pure function
//! over the file's text, so the whole thing is tested against a captured sysfs
//! tree with no hardware involved.

use std::path::{Path, PathBuf};
use std::time::Duration;

use wisp_proto::{Observation, SenseId};

use crate::budget;
use crate::consent::{Sense, SenseCtx, SenseHandle, SensePlugin};

/// `Observation::Vitals` and `Observation::Files` both report `SenseId::Vitals`
/// in `wisp-proto`, so the file watcher rides on this row of the consent panel.
pub struct VitalsSense {
    cfg: VitalsConfig,
}

impl Sense for VitalsSense {
    const ID: SenseId = SenseId::Vitals;
    const LABEL: &'static str = crate::consent::label_of(SenseId::Vitals);
    const DESCRIPTION: &'static str = crate::consent::description_of(SenseId::Vitals);
}

#[derive(Debug, Clone)]
pub struct VitalsConfig {
    /// Sampling interval at T0/T1. The governor widens it below that; see
    /// [`crate::budget::vitals_interval`].
    pub interval: Duration,
    /// Root to read from. Overridden in tests to point at a captured tree.
    pub sysfs_root: PathBuf,
    pub proc_root: PathBuf,
    /// Republish even when nothing moved, so a subscriber that just started has
    /// a number. Beyond that, only changes are published.
    pub min_delta_pct: u8,
}

impl Default for VitalsConfig {
    fn default() -> Self {
        VitalsConfig {
            interval: Duration::from_secs(5),
            sysfs_root: PathBuf::from("/sys"),
            proc_root: PathBuf::from("/proc"),
            min_delta_pct: 3,
        }
    }
}

impl VitalsSense {
    pub fn new(cfg: VitalsConfig) -> Self {
        VitalsSense { cfg }
    }
}

// ---------------------------------------------------------------------------
// Pure parsing
// ---------------------------------------------------------------------------

/// One line of `/proc/stat`'s aggregate `cpu` row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

/// `cpu  user nice system idle iowait irq softirq steal guest guest_nice`
pub fn parse_proc_stat(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> =
        line.split_whitespace().skip(1).filter_map(|f| f.parse::<u64>().ok()).collect();
    if fields.len() < 5 {
        return None;
    }
    // idle + iowait. guest and guest_nice are already counted inside user/nice,
    // so summing every field would double count them.
    let idle = fields[3] + fields[4];
    let counted = fields.len().min(8);
    let total: u64 = fields[..counted].iter().sum();
    Some(CpuTimes { total, idle })
}

/// Busy percentage between two `/proc/stat` samples.
pub fn cpu_busy_pct(prev: CpuTimes, now: CpuTimes) -> u8 {
    let dt = now.total.saturating_sub(prev.total);
    if dt == 0 {
        return 0;
    }
    let di = now.idle.saturating_sub(prev.idle);
    let busy = dt.saturating_sub(di);
    ((busy * 100 + dt / 2) / dt).min(100) as u8
}

/// sysfs integers come with a trailing newline and occasionally nothing at all.
pub fn parse_u64(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()
}

/// hwmon reports millidegrees.
pub fn parse_temp_millic(text: &str) -> Option<u8> {
    let m = text.trim().parse::<i64>().ok()?;
    Some((m / 1000).clamp(0, 255) as u8)
}

pub fn bytes_to_mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

/// A vitals sample, before it becomes an `Observation`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sample {
    pub cpu_pct: u8,
    pub gpu_pct: u8,
    pub vram_used_mib: u64,
    pub temp_c: u8,
    pub on_battery: bool,
}

impl From<Sample> for Observation {
    fn from(s: Sample) -> Observation {
        Observation::Vitals {
            cpu_pct: s.cpu_pct,
            gpu_pct: s.gpu_pct,
            vram_used_mib: s.vram_used_mib,
            temp_c: s.temp_c,
            on_battery: s.on_battery,
        }
    }
}

/// Suppresses samples that say nothing new. A vitals stream that publishes an
/// identical reading every five seconds is noise the flight recorder has to
/// store and the mind has to ignore.
#[derive(Debug, Default)]
pub struct VitalsGate {
    last: Option<Sample>,
    min_delta_pct: u8,
}

impl VitalsGate {
    pub fn new(min_delta_pct: u8) -> Self {
        VitalsGate { last: None, min_delta_pct }
    }

    pub fn apply(&mut self, s: Sample) -> Option<Observation> {
        let publish = match self.last {
            None => true,
            Some(p) => {
                p.on_battery != s.on_battery
                    || diff(p.cpu_pct, s.cpu_pct) >= self.min_delta_pct
                    || diff(p.gpu_pct, s.gpu_pct) >= self.min_delta_pct
                    || diff(p.temp_c, s.temp_c) >= self.min_delta_pct
                    || p.vram_used_mib.abs_diff(s.vram_used_mib) >= 64
            }
        };
        if !publish {
            return None;
        }
        self.last = Some(s);
        Some(s.into())
    }
}

fn diff(a: u8, b: u8) -> u8 {
    a.abs_diff(b)
}

// ---------------------------------------------------------------------------
// Where the numbers live
// ---------------------------------------------------------------------------

/// Resolved once. Discovery walks sysfs; sampling never does.
#[derive(Debug, Clone, Default)]
pub struct VitalsSources {
    pub proc_stat: PathBuf,
    /// amdgpu's `gpu_busy_percent`.
    pub gpu_busy: Option<PathBuf>,
    pub vram_used: Option<PathBuf>,
    pub vram_total: Option<PathBuf>,
    /// The hottest thing worth reporting: GPU junction if present, else CPU.
    pub temp: Option<PathBuf>,
    /// `/sys/class/power_supply/<mains>/online`, absent on a desktop with no
    /// battery at all.
    pub mains_online: Option<PathBuf>,
    pub battery_present: bool,
}

impl VitalsSources {
    pub fn discover(sysfs: &Path, proc: &Path) -> Self {
        let mut s = VitalsSources { proc_stat: proc.join("stat"), ..Default::default() };

        // The render card with a VRAM report is the discrete GPU. card0 on this
        // machine is the iGPU and reports a tiny total.
        let mut best: Option<(u64, PathBuf)> = None;
        if let Ok(cards) = std::fs::read_dir(sysfs.join("class/drm")) {
            for e in cards.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if !(name.starts_with("card") && name[4..].chars().all(|c| c.is_ascii_digit())) {
                    continue;
                }
                let dev = e.path().join("device");
                let total = std::fs::read_to_string(dev.join("mem_info_vram_total"))
                    .ok()
                    .and_then(|t| parse_u64(&t));
                if let Some(total) = total {
                    if best.as_ref().map(|(b, _)| total > *b).unwrap_or(true) {
                        best = Some((total, dev));
                    }
                }
            }
        }
        if let Some((_, dev)) = best {
            s.gpu_busy = exists(dev.join("gpu_busy_percent"));
            s.vram_used = exists(dev.join("mem_info_vram_used"));
            s.vram_total = exists(dev.join("mem_info_vram_total"));
            s.temp = find_hwmon_temp(&dev.join("hwmon"));
        }
        if s.temp.is_none() {
            s.temp = find_named_hwmon_temp(&sysfs.join("class/hwmon"), "k10temp");
        }

        if let Ok(supplies) = std::fs::read_dir(sysfs.join("class/power_supply")) {
            for e in supplies.flatten() {
                let ty = std::fs::read_to_string(e.path().join("type")).unwrap_or_default();
                match ty.trim() {
                    "Mains" => s.mains_online = exists(e.path().join("online")),
                    // A wireless mouse is a "Battery" but it is not this
                    // machine's power source. Only scope-less supplies count.
                    "Battery" => {
                        let scope = std::fs::read_to_string(e.path().join("scope"))
                            .unwrap_or_default();
                        if scope.trim().is_empty() || scope.trim() == "System" {
                            s.battery_present = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        s
    }

    pub fn sample(&self, prev_cpu: &mut CpuTimes) -> Sample {
        let cpu_pct = match read(&self.proc_stat).as_deref().and_then(parse_proc_stat) {
            Some(now) => {
                let pct = cpu_busy_pct(*prev_cpu, now);
                *prev_cpu = now;
                pct
            }
            None => 0,
        };
        let gpu_pct = self
            .gpu_busy
            .as_ref()
            .and_then(|p| read(p))
            .and_then(|t| parse_u64(&t))
            .unwrap_or(0)
            .min(100) as u8;
        let vram_used_mib = self
            .vram_used
            .as_ref()
            .and_then(|p| read(p))
            .and_then(|t| parse_u64(&t))
            .map(bytes_to_mib)
            .unwrap_or(0);
        let temp_c = self
            .temp
            .as_ref()
            .and_then(|p| read(p))
            .and_then(|t| parse_temp_millic(&t))
            .unwrap_or(0);
        let on_battery = self.on_battery();
        Sample { cpu_pct, gpu_pct, vram_used_mib, temp_c, on_battery }
    }

    /// A machine with no system battery is never on battery, whatever the mains
    /// file says.
    pub fn on_battery(&self) -> bool {
        if !self.battery_present {
            return false;
        }
        match self.mains_online.as_ref().and_then(|p| read(p)).and_then(|t| parse_u64(&t)) {
            Some(online) => online == 0,
            None => false,
        }
    }
}

fn exists(p: PathBuf) -> Option<PathBuf> {
    p.exists().then_some(p)
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

/// `<device>/hwmon/hwmonN/temp1_input`, preferring the junction sensor.
fn find_hwmon_temp(hwmon_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(hwmon_dir).ok()?;
    for e in entries.flatten() {
        let dir = e.path();
        for n in 1..=4 {
            let label = read(&dir.join(format!("temp{n}_label"))).unwrap_or_default();
            if label.trim().eq_ignore_ascii_case("junction") {
                if let Some(p) = exists(dir.join(format!("temp{n}_input"))) {
                    return Some(p);
                }
            }
        }
        if let Some(p) = exists(dir.join("temp1_input")) {
            return Some(p);
        }
    }
    None
}

fn find_named_hwmon_temp(class_hwmon: &Path, want: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(class_hwmon).ok()?;
    for e in entries.flatten() {
        let name = read(&e.path().join("name")).unwrap_or_default();
        if name.trim() == want {
            return exists(e.path().join("temp1_input"));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

impl SensePlugin for VitalsSense {
    fn spawn(self, handle: SenseHandle<Self>, mut ctx: SenseCtx) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let sources = VitalsSources::discover(&self.cfg.sysfs_root, &self.cfg.proc_root);
            tracing::info!(?sources, "vitals sources resolved");
            let mut prev = read(&sources.proc_stat)
                .as_deref()
                .and_then(parse_proc_stat)
                .unwrap_or_default();
            let mut gate = VitalsGate::new(self.cfg.min_delta_pct);
            let mut every = budget::vitals_interval(ctx.tier());
            let mut ticker = tokio::time::interval(every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut shutdown = ctx.shutdown.clone();

            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.wait() => break,
                    // A downgrade is honoured on the spot: the sample we would
                    // have taken is shed, never queued (SPEC §3.1).
                    Some(tier) = ctx.tier_changed() => {
                        every = budget::vitals_interval(tier);
                        tracing::info!(?tier, ?every, "vitals interval");
                        ticker = tokio::time::interval_at(
                            tokio::time::Instant::now() + every, every,
                        );
                        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    }
                    _ = ticker.tick() => {
                        let sample = sources.sample(&mut prev);
                        if let Some(obs) = gate.apply(sample) {
                            handle.emit(obs);
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sysfs"))
    }

    #[test]
    fn parses_the_captured_proc_stat() {
        let text = std::fs::read_to_string(tree().join("proc/stat")).unwrap();
        let t = parse_proc_stat(&text).unwrap();
        // idle 1772196973 + iowait 2189477
        assert_eq!(t.idle, 1_774_386_450);
        // first eight fields only; guest and guest_nice are already inside user/nice
        assert_eq!(
            t.total,
            147936451 + 5621258 + 173591003 + 1772196973 + 2189477 + 4314147 + 2215401
        );
    }

    #[test]
    fn cpu_percentage_between_two_samples() {
        let a = CpuTimes { total: 1000, idle: 900 };
        let b = CpuTimes { total: 1100, idle: 950 };
        assert_eq!(cpu_busy_pct(a, b), 50);
        // A stalled clock is 0, not a divide by zero.
        assert_eq!(cpu_busy_pct(a, a), 0);
        // Counters that went backwards (suspend, cpu hotplug) must not wrap.
        assert_eq!(cpu_busy_pct(b, a), 0);
        // Fully busy.
        assert_eq!(cpu_busy_pct(a, CpuTimes { total: 1100, idle: 900 }), 100);
    }

    #[test]
    fn garbage_proc_stat_is_none() {
        assert!(parse_proc_stat("").is_none());
        assert!(parse_proc_stat("cpu0 1 2 3 4 5").is_none(), "the aggregate row only");
        assert!(parse_proc_stat("cpu  1 2 3").is_none(), "too few fields");
    }

    #[test]
    fn sysfs_scalars() {
        assert_eq!(parse_u64("8756576256\n"), Some(8_756_576_256));
        assert_eq!(parse_u64("  17 \n"), Some(17));
        assert_eq!(parse_u64(""), None);
        assert_eq!(parse_u64("N/A"), None);
        assert_eq!(parse_temp_millic("61000\n"), Some(61));
        assert_eq!(parse_temp_millic("-5000"), Some(0), "clamped, not wrapped");
        assert_eq!(bytes_to_mib(8_756_576_256), 8350);
    }

    #[test]
    fn discovery_picks_the_discrete_gpu_not_the_igpu() {
        let s = VitalsSources::discover(&tree().join("sys"), &tree().join("proc"));
        let vram_total = read(s.vram_total.as_ref().unwrap()).unwrap();
        assert_eq!(parse_u64(&vram_total), Some(25_753_026_560), "discovery must pick the dGPU, not card0's iGPU");
        assert!(s.gpu_busy.is_some());
        assert!(s.temp.as_ref().unwrap().to_string_lossy().contains("hwmon"));
    }

    #[test]
    fn discovery_prefers_the_junction_sensor() {
        let s = VitalsSources::discover(&tree().join("sys"), &tree().join("proc"));
        let p = s.temp.unwrap();
        // temp2 is labelled junction in the capture; temp1 is edge.
        assert!(p.ends_with("temp2_input"), "picked {}", p.display());
    }

    #[test]
    fn a_desktop_with_only_a_mouse_battery_is_never_on_battery() {
        let s = VitalsSources::discover(&tree().join("sys"), &tree().join("proc"));
        assert!(!s.battery_present, "hidpp_battery_0 is scope=Device, not this machine");
        assert!(!s.on_battery());
    }

    #[test]
    fn a_full_sample_from_the_captured_tree() {
        let s = VitalsSources::discover(&tree().join("sys"), &tree().join("proc"));
        let mut prev = CpuTimes::default();
        let sample = s.sample(&mut prev);
        assert_eq!(sample.gpu_pct, 17);
        assert_eq!(sample.vram_used_mib, 8350);
        assert_eq!(sample.temp_c, 68);
        assert!(!sample.on_battery);
        let obs: Observation = sample.into();
        assert_eq!(obs.sense(), SenseId::Vitals);
    }

    #[test]
    fn missing_files_degrade_to_zero_rather_than_failing() {
        let empty = tempfile::tempdir().unwrap();
        let s = VitalsSources::discover(empty.path(), empty.path());
        let mut prev = CpuTimes::default();
        let sample = s.sample(&mut prev);
        assert_eq!(sample, Sample::default());
    }

    #[test]
    fn the_gate_publishes_the_first_sample_then_only_changes() {
        let mut g = VitalsGate::new(3);
        let base = Sample { cpu_pct: 10, gpu_pct: 5, vram_used_mib: 8000, temp_c: 60, on_battery: false };
        assert!(g.apply(base).is_some());
        assert!(g.apply(base).is_none());
        // Under the threshold in every dimension.
        let jitter = Sample { cpu_pct: 12, gpu_pct: 6, vram_used_mib: 8010, temp_c: 61, ..base };
        assert!(g.apply(jitter).is_none());
        // A real move.
        let busy = Sample { cpu_pct: 74, ..base };
        assert!(g.apply(busy).is_some());
    }

    #[test]
    fn losing_mains_is_always_news_however_small() {
        let mut g = VitalsGate::new(50);
        let base = Sample { cpu_pct: 10, gpu_pct: 5, vram_used_mib: 8000, temp_c: 60, on_battery: false };
        g.apply(base);
        assert!(g.apply(Sample { on_battery: true, ..base }).is_some());
    }

    #[test]
    fn a_model_loading_into_vram_is_news() {
        let mut g = VitalsGate::new(3);
        let base = Sample { cpu_pct: 10, gpu_pct: 5, vram_used_mib: 8000, temp_c: 60, on_battery: false };
        g.apply(base);
        assert!(g.apply(Sample { vram_used_mib: 8032, ..base }).is_none(), "32 MiB is noise");
        assert!(g.apply(Sample { vram_used_mib: 16000, ..base }).is_some());
    }
}
