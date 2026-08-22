//! CPU probe: `/proc/loadavg`, `/proc/stat` deltas and `/proc/pressure/cpu`.
//!
//! Load average alone is a bad signal (it counts uninterruptible I/O waits, so
//! a big `git clone` looks like a compile). PSI is the honest one and is present
//! on every kernel this project targets, so both are read and the ladder trips
//! on either.

use std::path::{Path, PathBuf};

use crate::{
    probe::{parse_psi_some_avg10, CpuProbe},
    reading::CpuReading,
};

#[derive(Debug, Clone)]
pub struct ProcCpuProbe {
    proc_root: PathBuf,
    /// `(idle_ticks, total_ticks)` from the previous read, for the busy delta.
    last: Option<(u64, u64)>,
}

impl Default for ProcCpuProbe {
    fn default() -> Self {
        ProcCpuProbe::with_root("/proc")
    }
}

impl ProcCpuProbe {
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        ProcCpuProbe {
            proc_root: root.into(),
            last: None,
        }
    }
}

impl CpuProbe for ProcCpuProbe {
    fn read(&mut self) -> CpuReading {
        let root = &self.proc_root;
        let load1 = std::fs::read_to_string(root.join("loadavg"))
            .ok()
            .and_then(|s| parse_loadavg(&s))
            .unwrap_or(0.0);

        let cores = count_cores(root);

        let busy_pct = std::fs::read_to_string(root.join("stat"))
            .ok()
            .and_then(|s| parse_stat_cpu_line(&s))
            .and_then(|(idle, total)| {
                let out = self.last.and_then(|(pi, pt)| {
                    let d_total = total.saturating_sub(pt);
                    let d_idle = idle.saturating_sub(pi);
                    let d_busy = d_total.saturating_sub(d_idle);
                    (d_busy * 100).checked_div(d_total).map(|p| p as u8)
                });
                self.last = Some((idle, total));
                out
            });

        let psi_some_avg10 = std::fs::read_to_string(root.join("pressure/cpu"))
            .map(|s| parse_psi_some_avg10(&s))
            .unwrap_or(0.0);

        CpuReading {
            cores,
            load1,
            busy_pct,
            psi_some_avg10,
        }
    }
}

fn count_cores(proc_root: &Path) -> u32 {
    // /proc/cpuinfo is the portable answer and matches what /proc/loadavg is
    // normalised against.
    let n = std::fs::read_to_string(proc_root.join("cpuinfo"))
        .map(|s| s.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(0);
    if n > 0 {
        return n as u32;
    }
    std::thread::available_parallelism()
        .map(|p| p.get() as u32)
        .unwrap_or(1)
}

pub(crate) fn parse_loadavg(s: &str) -> Option<f32> {
    s.split_whitespace().next()?.parse().ok()
}

/// The aggregate `cpu ` line: returns `(idle_ticks, total_ticks)`.
pub(crate) fn parse_stat_cpu_line(s: &str) -> Option<(u64, u64)> {
    let line = s.lines().find(|l| l.starts_with("cpu "))?;
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if vals.len() < 5 {
        return None;
    }
    // user nice system idle iowait irq softirq steal ...
    let idle = vals[3] + vals[4];
    let total: u64 = vals.iter().sum();
    Some((idle, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loadavg_parses() {
        assert_eq!(parse_loadavg("16.85 17.03 16.87 14/4958 1025243"), Some(16.85));
    }

    #[test]
    fn stat_cpu_line_parses() {
        let s = "cpu  100 0 50 800 20 0 0 0 0 0\ncpu0 1 2 3 4 5\n";
        let (idle, total) = parse_stat_cpu_line(s).unwrap();
        assert_eq!(idle, 820);
        assert_eq!(total, 970);
    }
}
