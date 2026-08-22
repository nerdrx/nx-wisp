//! GPU probe: enumerate DRM cards, read busy% and VRAM, and work out which card
//! is discrete and which is integrated.
//!
//! **Nothing here assumes a card index.** On the operator's desktop the
//! *integrated* Radeon is `card0` and the RX 7900 XTX is `card1`, and the render
//! nodes are crossed over — `card1` owns `renderD128`. Their laptop looks
//! different again. Topology is discovered every time.
//!
//! Real values from the operator's desktop, 2026-08-22:
//!
//! ```text
//! /sys/class/drm/card0/device  vendor=0x1002 device=0x13c0 (Raphael iGPU)
//!   mem_info_vram_total = 2147483648      (2048 MiB, carved out of system RAM)
//!   drm/renderD129 · no fan · no power1_cap · no mem_info_vram_vendor
//! /sys/class/drm/card1/device  vendor=0x1002 device=0x744c (Navi 31)
//!   mem_info_vram_total = 25753026560     (24560 MiB)
//!   drm/renderD128 · fan1_input · power1_cap · mem_info_vram_vendor · rom
//! ```

use std::path::{Path, PathBuf};

use crate::{
    probe::{parse_hex_id, read_trimmed, read_u64, GpuProbe, MIB},
    reading::{GpuId, GpuKind, GpuReading},
};

/// Reads `/sys/class/drm`. The root is a field so the classifier can be tested
/// against synthetic trees.
#[derive(Debug, Clone)]
pub struct SysfsGpuProbe {
    root: PathBuf,
}

impl Default for SysfsGpuProbe {
    fn default() -> Self {
        SysfsGpuProbe {
            root: PathBuf::from("/sys/class/drm"),
        }
    }
}

impl SysfsGpuProbe {
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        SysfsGpuProbe { root: root.into() }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl GpuProbe for SysfsGpuProbe {
    fn read(&mut self) -> Vec<GpuReading> {
        enumerate(&self.root)
    }
}

/// Every render-capable card under `drm_root`, in stable PCI order.
pub fn enumerate(drm_root: &Path) -> Vec<GpuReading> {
    let Ok(entries) = std::fs::read_dir(drm_root) else {
        return Vec::new();
    };

    let mut cards: Vec<GpuReading> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let idx = card_index(&name)?;
            read_card(&e.path(), idx)
        })
        .collect();

    // PCI order is the closest stable proxy we have for the order a Vulkan
    // loader will enumerate physical devices in.
    cards.sort_by(|a, b| a.id.pci_slot.cmp(&b.id.pci_slot));
    for (i, c) in cards.iter_mut().enumerate() {
        c.id.enumeration_index = i;
    }
    cards
}

/// `card0` -> `Some(0)`; `card0-DP-4`, `renderD128`, `version` -> `None`.
fn card_index(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("card")?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

fn read_card(card_dir: &Path, card_index: u32) -> Option<GpuReading> {
    let dev = card_dir.join("device");
    if !dev.is_dir() {
        return None;
    }

    let render_node = find_render_node(&dev);
    // A card with no render node is display-only (or something exotic); we
    // cannot render or infer on it, so it is not our business.
    render_node.as_ref()?;

    // `device/driver` is a symlink to the bound driver. Under a nested or
    // synthetic sysfs the symlink may be absent, so fall back to uevent's
    // `DRIVER=` line, which carries the same answer.
    let driver = std::fs::read_link(dev.join("driver"))
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
        .or_else(|| uevent_field(&dev, "DRIVER"))
        .unwrap_or_default();

    let pci_slot = uevent_field(&dev, "PCI_SLOT_NAME").unwrap_or_else(|| {
        std::fs::canonicalize(&dev)
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_else(|| format!("card{card_index}"))
    });

    let vendor_id = read_trimmed(&dev.join("vendor"))
        .as_deref()
        .and_then(parse_hex_id)
        .unwrap_or(0);
    let device_id = read_trimmed(&dev.join("device"))
        .as_deref()
        .and_then(parse_hex_id)
        .unwrap_or(0);

    let vram_total_mib = read_u64(&dev.join("mem_info_vram_total")).unwrap_or(0) / MIB;
    let vram_used_mib = read_u64(&dev.join("mem_info_vram_used")).unwrap_or(0) / MIB;
    let gtt_used_mib = read_u64(&dev.join("mem_info_gtt_used")).unwrap_or(0) / MIB;
    let busy_pct = read_u64(&dev.join("gpu_busy_percent")).unwrap_or(0).min(100) as u8;

    let kind = classify_kind(&dev, &driver, vram_total_mib);

    Some(GpuReading {
        id: GpuId {
            card_index,
            driver,
            pci_slot,
            render_node,
            vendor_id,
            device_id,
            kind,
            enumeration_index: 0,
        },
        busy_pct,
        vram_used_mib,
        vram_total_mib,
        gtt_used_mib,
        temp_c: hottest_temp_c(&dev),
    })
}

/// `device/drm/renderD128` -> `/dev/dri/renderD128`.
fn find_render_node(dev: &Path) -> Option<PathBuf> {
    let drm = dev.join("drm");
    let entries = std::fs::read_dir(drm).ok()?;
    let mut found: Option<String> = None;
    for e in entries.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("renderD") {
            found = Some(name);
            break;
        }
    }
    Some(PathBuf::from("/dev/dri").join(found?))
}

fn uevent_field(dev: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(dev.join("uevent")).ok()?;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            return Some(v.trim().to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Discrete vs integrated
// ---------------------------------------------------------------------------

/// One piece of evidence about what kind of card this is. Positive scores mean
/// discrete, negative mean integrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal {
    pub name: &'static str,
    pub score: i32,
}

/// Every signal that fired, in order. Exposed so the flight recorder can answer
/// "why did you think that was the integrated card?".
pub fn classify_signals(dev: &Path, driver: &str, vram_total_mib: u64) -> Vec<Signal> {
    let mut sig = Vec::new();
    let mut push = |name: &'static str, score: i32| sig.push(Signal { name, score });

    // Driver. Only conclusive for the two vendors that ship one driver per
    // class of part; `amdgpu` and `xe` serve both.
    match driver {
        "nvidia" | "nvidia-drm" | "nouveau" => push("driver is an NVIDIA driver", 6),
        "i915" => push("driver is i915", -3),
        "xe" => push("driver is xe", -2),
        "virtio-gpu" | "vmwgfx" | "qxl" => push("driver is a virtual GPU", -4),
        _ => {}
    }

    // Physical evidence of a board: a fan, a configurable power cap, a video
    // BIOS ROM, board info. An APU has none of these.
    if hwmon_has(dev, "fan1_input") {
        push("the card has a fan", 4);
    }
    if hwmon_has(dev, "power1_cap") {
        push("the card has its own power cap", 3);
    }
    if dev.join("rom").exists() {
        push("the card has a video BIOS ROM", 2);
    }
    if dev.join("board_info").exists() {
        push("the card reports board info", 1);
    }

    // Dedicated memory with a memory vendor (GDDR6 etc). An APU's "VRAM" is a
    // carve-out of system RAM and has no vendor.
    if dev.join("mem_info_vram_vendor").exists() {
        push("its VRAM has a memory vendor", 3);
    } else if dev.join("mem_info_vram_total").exists() {
        push("its VRAM has no memory vendor", -2);
    }
    if dev.join("mem_busy_percent").exists() {
        push("it reports memory-controller busy", 1);
    }
    // Intel's name for the same thing. This is what keeps an Arc dGPU (which
    // runs on i915/xe, same as every Intel iGPU) from being mistaken for one.
    if dev.join("lmem_total_bytes").exists() {
        push("it has Intel local memory", 5);
    }

    // Size, as a tiebreak only. Deliberately weak: laptop dGPUs are small and
    // modern APU carve-outs can be large.
    if vram_total_mib >= 6144 {
        push("it has a lot of VRAM", 2);
    } else if vram_total_mib > 0 && vram_total_mib <= 2048 {
        push("it has very little VRAM", -2);
    }

    sig
}

/// Weighted verdict over [`classify_signals`].
pub fn classify_kind(dev: &Path, driver: &str, vram_total_mib: u64) -> GpuKind {
    let total: i32 = classify_signals(dev, driver, vram_total_mib)
        .iter()
        .map(|s| s.score)
        .sum();
    match total {
        t if t >= 3 => GpuKind::Discrete,
        t if t <= -3 => GpuKind::Integrated,
        // Ambiguous. `Unknown` is treated as discrete everywhere it matters, so
        // we never borrow a card we are not sure about.
        _ => GpuKind::Unknown,
    }
}

fn hwmon_has(dev: &Path, file: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dev.join("hwmon")) else {
        return false;
    };
    entries
        .filter_map(|e| e.ok())
        .any(|e| e.path().join(file).exists())
}

/// Hottest `tempN_input` under the card's hwmon, in °C.
fn hottest_temp_c(dev: &Path) -> Option<i16> {
    let entries = std::fs::read_dir(dev.join("hwmon")).ok()?;
    let mut best: Option<i64> = None;
    for hw in entries.filter_map(|e| e.ok()) {
        let Ok(files) = std::fs::read_dir(hw.path()) else {
            continue;
        };
        for f in files.filter_map(|e| e.ok()) {
            let name = f.file_name().to_string_lossy().to_string();
            if name.starts_with("temp") && name.ends_with("_input") {
                if let Some(v) = read_trimmed(&f.path()).and_then(|s| s.parse::<i64>().ok()) {
                    // millidegrees
                    let c = v / 1000;
                    best = Some(best.map_or(c, |b: i64| b.max(c)));
                }
            }
        }
    }
    best.map(|c| c as i16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_index_ignores_connectors_and_render_nodes() {
        assert_eq!(card_index("card0"), Some(0));
        assert_eq!(card_index("card12"), Some(12));
        assert_eq!(card_index("card0-DP-4"), None);
        assert_eq!(card_index("card1-Writeback-1"), None);
        assert_eq!(card_index("renderD128"), None);
        assert_eq!(card_index("version"), None);
    }
}
