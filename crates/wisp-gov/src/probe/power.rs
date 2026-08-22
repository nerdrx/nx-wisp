//! Thermals and battery/AC.
//!
//! Two traps this avoids:
//!
//! 1. **Peripheral batteries.** The operator's desktop has exactly one entry in
//!    `/sys/class/power_supply`: `hidpp_battery_0`, their mouse. Its `type` is
//!    `Battery`. A naive probe would put the wisp into T4 Dormant when the mouse
//!    ran low. Anything with `scope=Device` is skipped.
//! 2. **Missing thermal zones.** This AMD desktop has no `/sys/class/thermal`
//!    at all; CPU temperature comes from the `k10temp` hwmon instead. Both
//!    sources are read and the hottest wins.

use std::path::{Path, PathBuf};

use crate::{
    probe::{read_trimmed, read_u64, PowerProbe},
    reading::PowerReading,
};

#[derive(Debug, Clone)]
pub struct SysfsPowerProbe {
    sys_root: PathBuf,
}

impl Default for SysfsPowerProbe {
    fn default() -> Self {
        SysfsPowerProbe::with_root("/sys")
    }
}

impl SysfsPowerProbe {
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        SysfsPowerProbe {
            sys_root: root.into(),
        }
    }
}

impl PowerProbe for SysfsPowerProbe {
    fn read(&mut self) -> PowerReading {
        let sys = &self.sys_root;
        let cpu_temp_c = cpu_temp(sys);
        let gpu_temp_c = hwmon_temp_by_name(&sys.join("class/hwmon"), &["amdgpu", "nvidia"]);
        let (on_ac, battery_pct, battery_discharging) = battery(&sys.join("class/power_supply"));
        PowerReading {
            cpu_temp_c,
            gpu_temp_c,
            on_ac,
            battery_pct,
            battery_discharging,
        }
    }
}

fn cpu_temp(sys: &Path) -> Option<i16> {
    let zones = thermal_zone_temp(&sys.join("class/thermal"));
    let hwmon = hwmon_temp_by_name(
        &sys.join("class/hwmon"),
        &["k10temp", "coretemp", "zenpower", "cpu_thermal"],
    );
    match (zones, hwmon) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

fn thermal_zone_temp(thermal: &Path) -> Option<i16> {
    let entries = std::fs::read_dir(thermal).ok()?;
    let mut best: Option<i64> = None;
    for e in entries.filter_map(|e| e.ok()) {
        if !e
            .file_name()
            .to_string_lossy()
            .starts_with("thermal_zone")
        {
            continue;
        }
        // Skip zones that are not about the processor package; a battery or
        // wifi zone reporting 100 °C is not our emergency.
        let ty = read_trimmed(&e.path().join("type")).unwrap_or_default();
        if !(ty.contains("cpu") || ty.contains("pkg") || ty.contains("x86") || ty.contains("soc")) {
            continue;
        }
        if let Some(v) = read_u64(&e.path().join("temp")) {
            let c = (v / 1000) as i64;
            best = Some(best.map_or(c, |b: i64| b.max(c)));
        }
    }
    best.map(|c| c as i16)
}

/// Hottest `tempN_input` under any hwmon whose `name` is in `names`.
fn hwmon_temp_by_name(hwmon_root: &Path, names: &[&str]) -> Option<i16> {
    let entries = std::fs::read_dir(hwmon_root).ok()?;
    let mut best: Option<i64> = None;
    for e in entries.filter_map(|e| e.ok()) {
        let name = read_trimmed(&e.path().join("name")).unwrap_or_default();
        if !names.contains(&name.as_str()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(e.path()) else {
            continue;
        };
        for f in files.filter_map(|f| f.ok()) {
            let fname = f.file_name().to_string_lossy().to_string();
            if fname.starts_with("temp") && fname.ends_with("_input") {
                if let Some(v) = read_trimmed(&f.path()).and_then(|s| s.parse::<i64>().ok()) {
                    let c = v / 1000;
                    best = Some(best.map_or(c, |b: i64| b.max(c)));
                }
            }
        }
    }
    best.map(|c| c as i16)
}

/// `(on_ac, battery_pct, discharging)`.
///
/// A machine with no system battery is a desktop, and a desktop is always on
/// mains. Peripheral batteries (`scope=Device`) are ignored entirely.
pub(crate) fn battery(power_supply: &Path) -> (bool, Option<u8>, bool) {
    let Ok(entries) = std::fs::read_dir(power_supply) else {
        return (true, None, false);
    };

    let mut mains_online: Option<bool> = None;
    let mut pct: Option<u8> = None;
    let mut discharging = false;

    for e in entries.filter_map(|e| e.ok()) {
        let p = e.path();
        let ty = read_trimmed(&p.join("type")).unwrap_or_default();
        let scope = read_trimmed(&p.join("scope")).unwrap_or_default();
        if scope.eq_ignore_ascii_case("Device") {
            // The operator's mouse. Not our problem.
            continue;
        }
        match ty.as_str() {
            "Mains" | "USB" | "USB_PD" | "USB_PD_DRP" => {
                if let Some(v) = read_u64(&p.join("online")) {
                    mains_online = Some(mains_online.unwrap_or(false) || v == 1);
                }
            }
            "Battery" => {
                if let Some(v) = read_u64(&p.join("capacity")) {
                    pct = Some(pct.map_or(v.min(100) as u8, |c: u8| c.min(v.min(100) as u8)));
                }
                let status = read_trimmed(&p.join("status")).unwrap_or_default();
                if status.eq_ignore_ascii_case("Discharging") {
                    discharging = true;
                }
            }
            _ => {}
        }
    }

    // No system battery at all => desktop => on mains.
    let on_ac = match (mains_online, pct) {
        (Some(v), _) => v,
        (None, None) => true,
        // A battery but no mains adapter node: trust the battery's own status.
        (None, Some(_)) => !discharging,
    };
    (on_ac, pct, discharging)
}
