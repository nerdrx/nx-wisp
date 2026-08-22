//! **F66 — the cost meter.**
//!
//! Two numbers, always both: what every subsystem *claims* it costs at this
//! tier, and what our process is *measured* to cost right now. Reporting only
//! the estimate would be a story she tells about herself, and SPEC §0.4 says she
//! does not do that.
//!
//! The headline is the sentence the operator reads. It is derived from the
//! measured numbers and from whether the discrete card is genuinely untouched,
//! never from the tier alone — "she is currently costing you nothing" has to be
//! a claim about the machine, not about our intentions.

use serde::{Deserialize, Serialize};
use wisp_proto::{Cost, Tier};

use crate::{device::DeviceChoice, probe::selfcost::MeasuredCost, reading::Snapshot};

/// What she costs, claimed and measured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostReport {
    pub tier: Tier,
    /// Sum of every registered subsystem's `Governed::cost_at(tier)`.
    pub estimated: Cost,
    /// Per-subsystem breakdown, in registration order.
    pub by_subsystem: Vec<(String, Cost)>,
    /// Really measured, from `/proc/self`.
    pub measured: MeasuredCost,
    /// Measured VRAM on the discrete card specifically. This is the number that
    /// has to be zero at T3.
    pub dgpu_vram_mib: u64,
    /// Nothing we are doing touches the discrete card.
    pub dgpu_untouched: bool,
    /// The sentence shown in the UI.
    pub headline: String,
    /// The governor's own explanation, e.g. `"T3 because WiVRn is streaming"`.
    pub because: String,
}

/// Thresholds under which she is honestly described as free. Deliberately
/// strict: nx-wisp-plan.md §3.5 targets <60 MiB RSS and ~0.5% of one core at T3,
/// so "nothing" means at or under that, not "not much".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreeThresholds {
    pub rss_mib: u32,
    pub cpu_centi_pct: u32,
}

impl Default for FreeThresholds {
    fn default() -> Self {
        FreeThresholds {
            rss_mib: 60,
            cpu_centi_pct: 50, // 0.5% of one core
        }
    }
}

/// Build the report. Pure: everything it needs is already measured.
pub fn report(
    tier: Tier,
    because: &str,
    by_subsystem: Vec<(String, Cost)>,
    measured: MeasuredCost,
    devices: &DeviceChoice,
    snapshot: &Snapshot,
    free: FreeThresholds,
) -> CostReport {
    let estimated = by_subsystem
        .iter()
        .fold(Cost::FREE, |acc, (_, c)| acc + *c);

    let dgpu_vram_mib = snapshot
        .discrete()
        .map(|g| measured.vram_on(&g.id.pci_slot))
        .unwrap_or(0);

    // "Untouched" is only true if both the plan and the measurement agree.
    let dgpu_untouched = devices.dgpu_untouched && dgpu_vram_mib == 0;

    let headline = headline(tier, &measured, dgpu_vram_mib, dgpu_untouched, free);

    CostReport {
        tier,
        estimated,
        by_subsystem,
        measured,
        dgpu_vram_mib,
        dgpu_untouched,
        headline,
        because: because.to_string(),
    }
}

fn headline(
    tier: Tier,
    m: &MeasuredCost,
    dgpu_vram_mib: u64,
    dgpu_untouched: bool,
    free: FreeThresholds,
) -> String {
    if tier == Tier::Dormant && m.rss_mib <= free.rss_mib && m.cpu_centi_pct <= free.cpu_centi_pct {
        return "she is dormant and costing you nothing".to_string();
    }
    let cheap = m.rss_mib <= free.rss_mib && m.cpu_centi_pct <= free.cpu_centi_pct;
    if dgpu_untouched && cheap {
        return "she is currently costing you nothing".to_string();
    }
    if dgpu_untouched {
        return format!(
            "she is costing you {} MiB of RAM and {} of a core \u{2014} nothing on your graphics card",
            m.rss_mib,
            fmt_cpu(m.cpu_centi_pct)
        );
    }
    format!(
        "she is costing you {} MiB of RAM, {} of a core and {} MiB of VRAM",
        m.rss_mib,
        fmt_cpu(m.cpu_centi_pct),
        dgpu_vram_mib.max(m.total_vram_mib())
    )
}

fn fmt_cpu(centi_pct: u32) -> String {
    if centi_pct < 100 {
        format!("{:.2}%", centi_pct as f32 / 100.0)
    } else {
        format!("{:.1}%", centi_pct as f32 / 100.0)
    }
}
