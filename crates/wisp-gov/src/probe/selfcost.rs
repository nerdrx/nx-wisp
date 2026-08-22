//! What *we* actually cost, measured rather than estimated.
//!
//! The cost meter (F66) is only worth showing if it is honest, so this reads the
//! real numbers for our own process:
//!
//! * **RSS** from `/proc/self/status` `VmRSS`.
//! * **CPU** from `/proc/self/stat` utime+stime deltas over wall time.
//! * **VRAM** from `/proc/self/fdinfo/*`, which on `amdgpu` (and `i915`, `xe`
//!   and `nouveau`) carries per-file DRM accounting. Verified on the operator's
//!   desktop, where `plasmashell`'s render-node fd reports:
//!
//!   ```text
//!   drm-driver:         amdgpu
//!   drm-pdev:           0000:03:00.0
//!   drm-resident-vram:  1036864 KiB
//!   ```
//!
//!   Grouping by `drm-pdev` is what lets the meter say "0 MiB on the 7900 XTX"
//!   rather than a single number that hides which card we are on.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

use serde::{Deserialize, Serialize};

/// Live measured cost of our own process.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasuredCost {
    pub rss_mib: u32,
    /// Hundredths of a percent of one core, matching [`wisp_proto::Cost`].
    pub cpu_centi_pct: u32,
    /// Resident VRAM per card, keyed by PCI slot (`0000:03:00.0`).
    pub vram_mib_by_pci: BTreeMap<String, u64>,
}

impl MeasuredCost {
    pub fn total_vram_mib(&self) -> u64 {
        self.vram_mib_by_pci.values().sum()
    }
    pub fn vram_on(&self, pci_slot: &str) -> u64 {
        self.vram_mib_by_pci.get(pci_slot).copied().unwrap_or(0)
    }
    /// As a [`wisp_proto::Cost`], so measured and estimated are comparable.
    pub fn as_cost(&self) -> wisp_proto::Cost {
        wisp_proto::Cost {
            ram_mib: self.rss_mib,
            vram_mib: self.total_vram_mib().min(u32::MAX as u64) as u32,
            cpu_centi_pct: self.cpu_centi_pct,
        }
    }
}

/// Samples our own process. Holds the previous CPU sample, so it must be kept
/// between polls; the first reading reports 0% because there is no delta yet.
#[derive(Debug)]
pub struct SelfCostProbe {
    proc_self: PathBuf,
    last: Option<(u64, Instant)>,
    clk_tck: u64,
}

impl Default for SelfCostProbe {
    fn default() -> Self {
        SelfCostProbe::with_root("/proc/self")
    }
}

impl SelfCostProbe {
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        SelfCostProbe {
            proc_self: root.into(),
            last: None,
            clk_tck: {
                // SAFETY: `sysconf` with a constant name has no preconditions.
                let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
                if v > 0 {
                    v as u64
                } else {
                    100
                }
            },
        }
    }

    pub fn read(&mut self) -> MeasuredCost {
        MeasuredCost {
            rss_mib: self.rss_mib(),
            cpu_centi_pct: self.cpu_centi_pct(),
            vram_mib_by_pci: self.vram_by_pci(),
        }
    }

    fn rss_mib(&self) -> u32 {
        std::fs::read_to_string(self.proc_self.join("status"))
            .ok()
            .and_then(|s| parse_vmrss_kib(&s))
            .map(|kib| (kib / 1024) as u32)
            .unwrap_or(0)
    }

    fn cpu_centi_pct(&mut self) -> u32 {
        let Some(text) = std::fs::read_to_string(self.proc_self.join("stat")).ok() else {
            return 0;
        };
        let Some((_, ticks)) = super::procs::parse_stat(&text) else {
            return 0;
        };
        let now = Instant::now();
        let out = match self.last {
            Some((prev, at)) => {
                let dt = now.duration_since(at).as_secs_f64();
                if dt <= 0.0 {
                    0
                } else {
                    let d = ticks.saturating_sub(prev) as f64 / self.clk_tck as f64;
                    // Cost::cpu_centi_pct is hundredths of one percent of a core.
                    ((d / dt) * 10_000.0).round().max(0.0) as u32
                }
            }
            None => 0,
        };
        self.last = Some((ticks, now));
        out
    }

    fn vram_by_pci(&self) -> BTreeMap<String, u64> {
        vram_by_pci(&self.proc_self.join("fdinfo"))
    }
}

/// Sum `drm-resident-vram` across every DRM fd in `fdinfo_dir`, grouped by
/// `drm-pdev` and de-duplicated by `drm-client-id` — two fds onto the same DRM
/// client report the same memory, and counting it twice would make the meter
/// lie in the direction that flatters us.
pub fn vram_by_pci(fdinfo_dir: &Path) -> BTreeMap<String, u64> {
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    let mut seen: std::collections::HashSet<(String, String)> = Default::default();
    let Ok(entries) = std::fs::read_dir(fdinfo_dir) else {
        return out;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Some(f) = parse_fdinfo(&text) else {
            continue;
        };
        if !seen.insert((f.pdev.clone(), f.client_id.clone())) {
            continue;
        }
        *out.entry(f.pdev).or_insert(0) += f.resident_vram_kib / 1024;
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DrmFdInfo {
    pub pdev: String,
    pub client_id: String,
    pub resident_vram_kib: u64,
}

/// Parse one `/proc/<pid>/fdinfo/<fd>` file. `None` unless it is a DRM fd.
pub(crate) fn parse_fdinfo(text: &str) -> Option<DrmFdInfo> {
    let mut pdev = None;
    let mut client_id = None;
    let mut resident = 0u64;
    let mut is_drm = false;

    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "drm-driver" => is_drm = true,
            "drm-pdev" => pdev = Some(v.to_string()),
            "drm-client-id" => client_id = Some(v.to_string()),
            // `drm-resident-vram` is the honest one: `total` counts memory that
            // has been evicted to system RAM, which is exactly what F62's warm
            // eviction leaves behind and is no longer costing the card anything.
            "drm-resident-vram" => resident = parse_size_kib(v),
            _ => {}
        }
    }
    if !is_drm {
        return None;
    }
    Some(DrmFdInfo {
        pdev: pdev.unwrap_or_else(|| "unknown".to_string()),
        client_id: client_id.unwrap_or_default(),
        resident_vram_kib: resident,
    })
}

/// `"1036864 KiB"` -> 1036864. A bare number is bytes, per the DRM fdinfo spec.
fn parse_size_kib(v: &str) -> u64 {
    let mut it = v.split_whitespace();
    let Some(n) = it.next().and_then(|n| n.parse::<u64>().ok()) else {
        return 0;
    };
    match it.next() {
        Some("KiB") => n,
        Some("MiB") => n * 1024,
        Some("GiB") => n * 1024 * 1024,
        _ => n / 1024,
    }
}

pub(crate) fn parse_vmrss_kib(status: &str) -> Option<u64> {
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_FDINFO: &str = "\
pos:\t0
flags:\t02100002
mnt_id:\t38
ino:\t686
drm-driver:\tamdgpu
drm-client-id:\t29107
drm-pdev:\t0000:03:00.0
drm-total-vram:\t1036864 KiB
drm-shared-vram:\t218784 KiB
drm-resident-vram:\t1036864 KiB
drm-purgeable-vram:\t8448 KiB
";

    #[test]
    fn parses_a_real_amdgpu_fdinfo() {
        let f = parse_fdinfo(REAL_FDINFO).unwrap();
        assert_eq!(f.pdev, "0000:03:00.0");
        assert_eq!(f.client_id, "29107");
        assert_eq!(f.resident_vram_kib, 1_036_864);
    }

    #[test]
    fn ignores_non_drm_fds() {
        assert!(parse_fdinfo("pos:\t0\nflags:\t02\nmnt_id:\t24\n").is_none());
    }

    #[test]
    fn vmrss_parses() {
        assert_eq!(parse_vmrss_kib("Name:\tx\nVmRSS:\t   58240 kB\n"), Some(58240));
    }
}
