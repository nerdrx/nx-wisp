//! RAM probe: `/proc/meminfo` and `/proc/pressure/memory`.
//!
//! `MemAvailable` is the only field in `meminfo` that means anything to a
//! governor — `MemFree` on a machine with 60 GiB of page cache is meaningless,
//! and F62's warm eviction depends on that page cache staying resident.

use std::path::PathBuf;

use crate::{
    probe::{parse_psi_some_avg10, MemProbe},
    reading::MemReading,
};

#[derive(Debug, Clone)]
pub struct ProcMemProbe {
    proc_root: PathBuf,
}

impl Default for ProcMemProbe {
    fn default() -> Self {
        ProcMemProbe::with_root("/proc")
    }
}

impl ProcMemProbe {
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        ProcMemProbe {
            proc_root: root.into(),
        }
    }
}

impl MemProbe for ProcMemProbe {
    fn read(&mut self) -> MemReading {
        let meminfo = std::fs::read_to_string(self.proc_root.join("meminfo")).unwrap_or_default();
        let (total_mib, available_mib) = parse_meminfo(&meminfo);
        let psi_some_avg10 = std::fs::read_to_string(self.proc_root.join("pressure/memory"))
            .map(|s| parse_psi_some_avg10(&s))
            .unwrap_or(0.0);
        MemReading {
            total_mib,
            available_mib,
            psi_some_avg10,
        }
    }
}

/// `(total_mib, available_mib)`.
pub(crate) fn parse_meminfo(s: &str) -> (u64, u64) {
    let field = |key: &str| -> u64 {
        s.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kb| kb / 1024)
            .unwrap_or(0)
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_parses_the_operators_desktop() {
        let s = "MemTotal:       63304324 kB\nMemFree:        27893016 kB\nMemAvailable:   34275528 kB\n";
        let (total, avail) = parse_meminfo(s);
        assert_eq!(total, 61820);
        assert_eq!(avail, 33472);
    }

    #[test]
    fn missing_fields_are_zero_not_a_panic() {
        assert_eq!(parse_meminfo(""), (0, 0));
    }
}
