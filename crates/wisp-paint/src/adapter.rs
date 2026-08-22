//! Which GPU she draws on.
//!
//! `wisp-gov` will later move rendering between the dGPU and the iGPU as the
//! tier changes — at T2/T3 the discrete card belongs to whatever game is
//! running (SPEC §3.1: "Zero dGPU use"). So adapter choice is a *parameter*,
//! not a one-time `Default::default()`, and the choosing is a pure function
//! over a list of summaries so it can be tested without a GPU present.

use crate::error::{PaintError, Result};

/// What the caller wants, in the caller's language. `wisp-gov` speaks in
/// tiers; the translation lives in [`AdapterPreference::for_tier`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AdapterPreference {
    /// The discrete card, if there is one. T0/T1.
    #[default]
    HighPerformance,
    /// The integrated GPU, if there is one. T2 and below: the dGPU belongs to
    /// the game.
    LowPower,
    /// A substring match on the adapter name, case-insensitive. For the
    /// operator's config file and for `wisp gpu --use`.
    Named(String),
    /// Exact index into the enumerated list. For tests and for the `--adapter`
    /// flag that prints the list first.
    Index(usize),
}

impl AdapterPreference {
    /// SPEC §3.1: `Tier::may_use_dgpu` is the rule; this is the consequence.
    pub fn for_tier(tier: wisp_proto::Tier) -> AdapterPreference {
        if tier.may_use_dgpu() {
            AdapterPreference::HighPerformance
        } else {
            AdapterPreference::LowPower
        }
    }
}

impl std::fmt::Display for AdapterPreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdapterPreference::HighPerformance => write!(f, "high-performance"),
            AdapterPreference::LowPower => write!(f, "low-power"),
            AdapterPreference::Named(n) => write!(f, "named {n:?}"),
            AdapterPreference::Index(i) => write!(f, "index {i}"),
        }
    }
}

/// The parts of `wgpu::AdapterInfo` the choice actually depends on. Copied out
/// so the selection logic can be unit-tested with no instance, no Vulkan
/// loader and no GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterSummary {
    pub name: String,
    pub device_type: DeviceKind,
    pub vendor: u32,
    pub device: u32,
    pub driver: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Discrete,
    Integrated,
    Virtual,
    Cpu,
    Other,
}

impl From<wgpu::DeviceType> for DeviceKind {
    fn from(t: wgpu::DeviceType) -> DeviceKind {
        match t {
            wgpu::DeviceType::DiscreteGpu => DeviceKind::Discrete,
            wgpu::DeviceType::IntegratedGpu => DeviceKind::Integrated,
            wgpu::DeviceType::VirtualGpu => DeviceKind::Virtual,
            wgpu::DeviceType::Cpu => DeviceKind::Cpu,
            wgpu::DeviceType::Other => DeviceKind::Other,
        }
    }
}

impl From<&wgpu::AdapterInfo> for AdapterSummary {
    fn from(i: &wgpu::AdapterInfo) -> AdapterSummary {
        AdapterSummary {
            name: i.name.clone(),
            device_type: i.device_type.into(),
            vendor: i.vendor,
            device: i.device,
            driver: i.driver.clone(),
        }
    }
}

/// Rank a device kind for a preference. Higher is better; `None` means the
/// adapter is unusable for this preference at all.
fn score(kind: DeviceKind, pref_low_power: bool) -> Option<u32> {
    // A CPU adapter (lavapipe) would technically satisfy "low power" and would
    // also make her cost the machine everything she was supposed to save, so
    // it is never chosen implicitly — only by `Named` or `Index`.
    match (kind, pref_low_power) {
        (DeviceKind::Cpu, _) => None,
        (DeviceKind::Discrete, false) => Some(3),
        (DeviceKind::Integrated, false) => Some(2),
        (DeviceKind::Integrated, true) => Some(3),
        (DeviceKind::Discrete, true) => Some(1),
        (DeviceKind::Virtual, _) => Some(1),
        (DeviceKind::Other, _) => Some(0),
    }
}

/// **The whole adapter policy, as a pure function.** Given what Vulkan
/// enumerated and what the caller asked for, which one do we use?
pub fn select(adapters: &[AdapterSummary], pref: &AdapterPreference) -> Result<usize> {
    if adapters.is_empty() {
        return Err(PaintError::NoAdapter(pref.to_string()));
    }
    let picked = match pref {
        AdapterPreference::Index(i) => {
            if *i < adapters.len() {
                Some(*i)
            } else {
                None
            }
        }
        AdapterPreference::Named(want) => {
            let want = want.to_lowercase();
            adapters.iter().position(|a| a.name.to_lowercase().contains(&want))
        }
        AdapterPreference::HighPerformance | AdapterPreference::LowPower => {
            let low = matches!(pref, AdapterPreference::LowPower);
            adapters
                .iter()
                .enumerate()
                .filter_map(|(i, a)| score(a.device_type, low).map(|s| (s, i)))
                // Ties break on the earlier enumeration index, which is stable
                // across runs — she must not switch GPUs between launches for
                // no reason.
                .max_by_key(|(s, i)| (*s, std::cmp::Reverse(*i)))
                .map(|(_, i)| i)
        }
    };
    picked.ok_or_else(|| PaintError::NoAdapter(pref.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(name: &str, kind: DeviceKind) -> AdapterSummary {
        AdapterSummary {
            name: name.into(),
            device_type: kind,
            vendor: 0x1002,
            device: 0x744c,
            driver: "radv".into(),
        }
    }

    fn machine() -> Vec<AdapterSummary> {
        // The real one: RX 7900 XTX plus the Raphael iGPU.
        vec![
            a("AMD Radeon RX 7900 XTX (RADV NAVI31)", DeviceKind::Discrete),
            a("AMD Radeon Graphics (RADV RAPHAEL_MENDOCINO)", DeviceKind::Integrated),
            a("llvmpipe (LLVM 20.1.0, 256 bits)", DeviceKind::Cpu),
        ]
    }

    #[test]
    fn high_performance_takes_the_discrete_card() {
        assert_eq!(select(&machine(), &AdapterPreference::HighPerformance).unwrap(), 0);
    }

    #[test]
    fn low_power_takes_the_igpu() {
        assert_eq!(select(&machine(), &AdapterPreference::LowPower).unwrap(), 1);
    }

    #[test]
    fn a_software_adapter_is_never_chosen_implicitly() {
        let only_cpu = vec![a("llvmpipe", DeviceKind::Cpu)];
        assert!(select(&only_cpu, &AdapterPreference::LowPower).is_err());
        assert!(select(&only_cpu, &AdapterPreference::HighPerformance).is_err());
        // …but the operator may still ask for it by name.
        assert_eq!(select(&only_cpu, &AdapterPreference::Named("llvm".into())).unwrap(), 0);
    }

    #[test]
    fn low_power_falls_back_to_the_dgpu_when_there_is_no_igpu() {
        let only_dgpu = vec![a("RX 7900 XTX", DeviceKind::Discrete)];
        assert_eq!(select(&only_dgpu, &AdapterPreference::LowPower).unwrap(), 0);
    }

    #[test]
    fn naming_is_a_case_insensitive_substring() {
        assert_eq!(select(&machine(), &AdapterPreference::Named("NAVI31".into())).unwrap(), 0);
        assert_eq!(select(&machine(), &AdapterPreference::Named("raphael".into())).unwrap(), 1);
        assert!(select(&machine(), &AdapterPreference::Named("nvidia".into())).is_err());
    }

    #[test]
    fn an_out_of_range_index_is_an_error_not_a_silent_fallback() {
        assert!(select(&machine(), &AdapterPreference::Index(9)).is_err());
        assert_eq!(select(&machine(), &AdapterPreference::Index(1)).unwrap(), 1);
    }

    #[test]
    fn an_empty_enumeration_is_an_error() {
        assert!(select(&[], &AdapterPreference::HighPerformance).is_err());
    }

    #[test]
    fn the_choice_is_stable_across_ties() {
        let two = vec![a("first igpu", DeviceKind::Integrated), a("second igpu", DeviceKind::Integrated)];
        assert_eq!(select(&two, &AdapterPreference::LowPower).unwrap(), 0);
        assert_eq!(select(&two, &AdapterPreference::LowPower).unwrap(), 0);
    }

    #[test]
    fn the_tier_ladder_moves_her_off_the_discrete_card() {
        use wisp_proto::Tier;
        for t in [Tier::Feral, Tier::Full, Tier::Reduced] {
            assert_eq!(AdapterPreference::for_tier(t), AdapterPreference::HighPerformance);
        }
        for t in [Tier::Lobotomised, Tier::Dormant] {
            assert_eq!(AdapterPreference::for_tier(t), AdapterPreference::LowPower);
        }
        assert_eq!(select(&machine(), &AdapterPreference::for_tier(Tier::Lobotomised)).unwrap(), 1);
    }
}
