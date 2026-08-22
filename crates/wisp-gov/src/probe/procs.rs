//! Process detection: is a game or a VR runtime alive?
//!
//! **WiVRn matters most here.** It is the operator's own VR streaming server,
//! and a headset dropping frames because a language model decided to think is
//! the exact failure the charter forbids. A live VR runtime is enough for T3 on
//! its own — we do not wait for it to prove it is streaming, because by then
//! the operator is already wearing the thing.
//!
//! Cost: one `readdir` of `/proc` and one `stat` read per process per poll.
//! `cmdline` is only read for candidates, because reading it for a thousand
//! processes several times a second would itself violate the charter.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Instant,
};

use crate::{
    config::ProcessSignatures,
    probe::{surface::VrSessionHint, ProcProbe},
    reading::{HeavyProc, HeavySource, ProcReading, VrRuntime, VrRuntimeKind},
};

/// CPU percentage (of one core) at which a VR runtime is assumed to be actually
/// encoding frames rather than sitting idle waiting for a headset. Only affects
/// the *wording* of the readout — either way the tier is T3.
const VR_STREAMING_CPU_PCT: u16 = 15;

#[derive(Debug, Clone, Copy)]
struct Sample {
    ticks: u64,
}

pub struct ProcfsProcProbe {
    proc_root: PathBuf,
    sigs: ProcessSignatures,
    last: HashMap<u32, Sample>,
    last_at: Option<Instant>,
    clk_tck: u64,
    /// Set by `wisp-senses` / `wisp-fleet` when it knows for a fact that a
    /// session is live (NX Connector sees WiVRn's own state — F45).
    pub vr_hint: VrSessionHint,
}

impl ProcfsProcProbe {
    pub fn new(sigs: ProcessSignatures) -> Self {
        ProcfsProcProbe::with_root("/proc", sigs)
    }

    pub fn with_root(root: impl Into<PathBuf>, sigs: ProcessSignatures) -> Self {
        ProcfsProcProbe {
            proc_root: root.into(),
            sigs,
            last: HashMap::new(),
            last_at: None,
            clk_tck: clock_ticks_per_second(),
            vr_hint: VrSessionHint::default(),
        }
    }
}

impl ProcProbe for ProcfsProcProbe {
    fn read(&mut self) -> ProcReading {
        let now = Instant::now();
        let dt = self
            .last_at
            .map(|t| now.duration_since(t).as_secs_f64())
            .filter(|d| *d > 0.05);
        self.last_at = Some(now);

        let own_pid = std::process::id();
        let mut seen: HashMap<u32, Sample> = HashMap::new();
        let mut vr: Option<VrRuntime> = None;
        let mut game: Option<HeavyProc> = None;
        let mut top: Option<HeavyProc> = None;

        let Ok(entries) = std::fs::read_dir(&self.proc_root) else {
            return ProcReading::default();
        };

        for e in entries.filter_map(|e| e.ok()) {
            let Some(pid) = e.file_name().to_string_lossy().parse::<u32>().ok() else {
                continue;
            };
            if pid == own_pid {
                continue;
            }
            let Some((comm, ticks)) = read_stat(&e.path()) else {
                continue;
            };
            seen.insert(pid, Sample { ticks });

            let cpu_pct = match (dt, self.last.get(&pid)) {
                (Some(dt), Some(prev)) => {
                    let d = ticks.saturating_sub(prev.ticks) as f64;
                    ((d / self.clk_tck as f64 / dt) * 100.0).round() as u16
                }
                _ => 0,
            };

            if self.sigs.ignore.iter().any(|i| eq_ci(&comm, i)) {
                continue;
            }

            // --- VR runtimes -----------------------------------------------
            if let Some(kind) = vr_kind(&comm, &self.sigs) {
                let streaming = self
                    .vr_hint
                    .get()
                    .unwrap_or(cpu_pct >= VR_STREAMING_CPU_PCT);
                let candidate = VrRuntime {
                    kind,
                    proc_name: comm.clone(),
                    pid,
                    streaming,
                };
                // WiVRn wins over anything else: it is the one the operator runs.
                vr = match vr.take() {
                    Some(existing) if existing.kind == VrRuntimeKind::WiVRn => Some(existing),
                    _ => Some(candidate),
                };
            }

            // --- Games ------------------------------------------------------
            if game.is_none() {
                if let Some(source) = self.game_source(&e.path(), &comm) {
                    game = Some(HeavyProc {
                        name: pretty_name(&comm),
                        pid,
                        cpu_pct,
                        source,
                    });
                }
            }

            // --- Biggest CPU consumer ---------------------------------------
            if top.as_ref().is_none_or(|t| cpu_pct > t.cpu_pct) && cpu_pct > 0 {
                top = Some(HeavyProc {
                    name: pretty_name(&comm),
                    pid,
                    cpu_pct,
                    source: HeavySource::CpuHog,
                });
            }
        }

        self.last = seen;
        ProcReading { vr, game, top_cpu: top }
    }
}

impl ProcfsProcProbe {
    fn game_source(&self, proc_dir: &Path, comm: &str) -> Option<HeavySource> {
        if self.sigs.games.iter().any(|g| contains_ci(comm, g)) {
            return Some(HeavySource::KnownName);
        }
        if self.sigs.game_launchers.iter().any(|g| eq_ci(comm, g)) {
            return Some(HeavySource::GameLauncher);
        }
        // Only now is it worth the second read.
        let looks_windows = comm.to_ascii_lowercase().contains(".exe");
        if !looks_windows && !comm.to_ascii_lowercase().contains("wine") {
            return None;
        }
        let cmdline = read_cmdline(proc_dir)?;
        let lower = cmdline.to_ascii_lowercase();
        if self
            .sigs
            .game_paths
            .iter()
            .any(|p| lower.contains(&p.to_ascii_lowercase()))
        {
            return Some(if lower.contains("proton") || lower.contains("wine") {
                HeavySource::Proton
            } else {
                HeavySource::SteamLibrary
            });
        }
        // A bare Windows executable running on Linux is a game often enough,
        // and being wrong here only costs her some cleverness for a while.
        looks_windows.then_some(HeavySource::Proton)
    }
}

fn vr_kind(comm: &str, s: &ProcessSignatures) -> Option<VrRuntimeKind> {
    let m = |list: &[String]| list.iter().any(|n| contains_ci(comm, n));
    if m(&s.vr_wivrn) {
        return Some(VrRuntimeKind::WiVRn);
    }
    if m(&s.vr_steamvr) {
        return Some(VrRuntimeKind::SteamVr);
    }
    if m(&s.vr_alvr) {
        return Some(VrRuntimeKind::Alvr);
    }
    if m(&s.vr_monado) {
        return Some(VrRuntimeKind::Monado);
    }
    if m(&s.vr_other) {
        return Some(VrRuntimeKind::Other);
    }
    None
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}
fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_ascii_lowercase().contains(&needle.to_ascii_lowercase())
}

/// `/proc/<pid>/comm` truncates at 15 bytes, so `Cyberpunk2077.exe` arrives as
/// `Cyberpunk2077.e`. Nothing we can do about that from `stat`; the cmdline
/// path is what the readout uses when it has one.
fn pretty_name(comm: &str) -> String {
    comm.to_string()
}

/// `(comm, utime + stime)` from `/proc/<pid>/stat`.
pub(crate) fn read_stat(proc_dir: &Path) -> Option<(String, u64)> {
    let text = std::fs::read_to_string(proc_dir.join("stat")).ok()?;
    parse_stat(&text)
}

/// Split on the **last** `)`, because a process is allowed to have parentheses
/// and spaces in its name and several do.
pub(crate) fn parse_stat(text: &str) -> Option<(String, u64)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close < open {
        return None;
    }
    let comm = text[open + 1..close].to_string();
    let rest: Vec<&str> = text[close + 1..].split_whitespace().collect();
    // rest[0] is field 3 (state), so utime (14) is rest[11] and stime (15) is rest[12].
    let utime: u64 = rest.get(11)?.parse().ok()?;
    let stime: u64 = rest.get(12)?.parse().ok()?;
    Some((comm, utime + stime))
}

fn read_cmdline(proc_dir: &Path) -> Option<String> {
    let raw = std::fs::read(proc_dir.join("cmdline")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn clock_ticks_per_second() -> u64 {
    // SAFETY: sysconf with a constant name has no preconditions and no side
    // effects; a negative return simply means "unknown".
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 {
        v as u64
    } else {
        100
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_handles_parenthesised_names() {
        let text = "42 (weird (name) here) S 1 42 42 0 -1 4194304 100 0 0 0 \
                    111 222 0 0 20 0 1 0 900 0 0";
        let (comm, ticks) = parse_stat(text).unwrap();
        assert_eq!(comm, "weird (name) here");
        assert_eq!(ticks, 333);
    }

    #[test]
    fn wivrn_is_recognised_and_beats_other_runtimes() {
        let s = ProcessSignatures::default();
        assert_eq!(vr_kind("wivrn-server", &s), Some(VrRuntimeKind::WiVRn));
        assert_eq!(vr_kind("monado-service", &s), Some(VrRuntimeKind::Monado));
        assert_eq!(vr_kind("vrserver", &s), Some(VrRuntimeKind::SteamVr));
        assert_eq!(vr_kind("kwin_wayland", &s), None);
    }
}
