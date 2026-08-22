//! **F61 — the two-GPU trick.**
//!
//! The operator's desktop has a 24 GiB RX 7900 XTX and a 2 GiB integrated
//! Radeon that does nothing all day. At T2/T3 both the rig and inference move
//! to the integrated card, so the discrete one is *completely untouched* while
//! they game. That is the difference between polite and genuinely invisible.
//!
//! This module answers one question — "which adapter should rendering and
//! inference use right now?" — in a form both a `wgpu` adapter filter and a
//! llama.cpp device index can consume. `wisp-gov` deliberately does not depend
//! on `wgpu`: it hands back PCI ids, a DRM render node and an enumeration
//! index, and the caller matches those against whatever it enumerated.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use wisp_proto::Tier;

use crate::{
    config::GovConfig,
    reading::{GpuKind, GpuReading, Snapshot},
};

/// One concrete card, plus the budget it may be used within.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuTarget {
    pub card_index: u32,
    pub kind: GpuKind,
    /// Matches `wgpu::AdapterInfo::vendor`.
    pub vendor_id: u32,
    /// Matches `wgpu::AdapterInfo::device`.
    pub device_id: u32,
    /// `0000:03:00.0` — the only truly stable name for a card.
    pub pci_slot: String,
    /// `/dev/dri/renderD128`. Note that render node numbering does **not**
    /// follow card numbering.
    pub render_node: Option<PathBuf>,
    pub vram_total_mib: u64,
    /// How much VRAM she may hold on this card at the current tier.
    pub vram_budget_mib: u64,
    /// PCI-ordered index among this machine's cards. Best-effort fallback for
    /// llama.cpp's `--main-gpu`; prefer [`GpuTarget::index_in`].
    pub enumeration_index: usize,
}

impl GpuTarget {
    /// For `wgpu`: `instance.enumerate_adapters(..).find(|a| target.matches_adapter(a.get_info().vendor, a.get_info().device))`.
    pub fn matches_adapter(&self, vendor: u32, device: u32) -> bool {
        self.vendor_id == vendor && self.device_id == device
    }

    /// For llama.cpp / ggml-vulkan: given the `(vendor, device)` pairs the
    /// backend enumerated, in its own order, return the index to pass as the
    /// main device. This is the exact answer and should always be preferred to
    /// [`GpuTarget::enumeration_index`].
    pub fn index_in(&self, enumerated: &[(u32, u32)]) -> Option<usize> {
        enumerated
            .iter()
            .position(|(v, d)| self.matches_adapter(*v, *d))
    }

    /// `MESA_VK_DEVICE_SELECT` value, `"1002:13c0"`. Mesa accepts this to force
    /// a specific physical device, which is how we pin an unmodified llama.cpp
    /// or wgpu process to the integrated card without patching it.
    pub fn vk_device_select(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor_id, self.device_id)
    }

    /// Environment a child process (or our own Vulkan init) should be given so
    /// it lands on this card.
    pub fn env_hints(&self) -> Vec<(String, String)> {
        let mut v = vec![
            (
                "MESA_VK_DEVICE_SELECT".to_string(),
                self.vk_device_select(),
            ),
            ("MESA_VK_DEVICE_SELECT_FORCE_DEFAULT_DEVICE".to_string(), "1".to_string()),
        ];
        if let Some(node) = &self.render_node {
            v.push((
                "WISP_DRM_RENDER_NODE".to_string(),
                node.display().to_string(),
            ));
        }
        v
    }

    fn from_reading(g: &GpuReading, budget: u64) -> Self {
        GpuTarget {
            card_index: g.id.card_index,
            kind: g.id.kind,
            vendor_id: g.id.vendor_id,
            device_id: g.id.device_id,
            pci_slot: g.id.pci_slot.clone(),
            render_node: g.id.render_node.clone(),
            vram_total_mib: g.vram_total_mib,
            vram_budget_mib: budget,
            enumeration_index: g.id.enumeration_index,
        }
    }
}

/// Where rendering and inference should run right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceChoice {
    /// The card the rig renders on. `None` at T4 Dormant: she is not drawn.
    pub render: Option<GpuTarget>,
    /// The card a model may be resident on. `None` at T3/T4, where SPEC §3.1's
    /// [`Tier::may_hold_model`] is false and the model is fully unloaded.
    pub inference: Option<GpuTarget>,
    /// True when nothing we do touches the discrete card at all. This is the
    /// claim the cost meter makes to the operator, so it is computed, not
    /// assumed.
    pub dgpu_untouched: bool,
    /// Plain English, for the readout and the flight recorder.
    pub note: String,
}

impl DeviceChoice {
    /// Does this choice use the given card for anything?
    pub fn uses(&self, pci_slot: &str) -> bool {
        [&self.render, &self.inference]
            .into_iter()
            .flatten()
            .any(|t| t.pci_slot == pci_slot)
    }
}

/// Pick devices for a tier. Pure: no Vulkan, no wgpu, no probing.
///
/// * **T0/T1** — discrete card for both. She is allowed to be expensive.
/// * **T2** — rig moves to the integrated card; the reflex model follows if the
///   integrated card has enough VRAM to be worth it, otherwise it stays on the
///   discrete card inside [`GovConfig::reduced_vram_budget_mib`].
/// * **T3** — rig on the integrated card, **zero** discrete use, no model.
/// * **T4** — nothing.
///
/// On a machine with only one card there is nowhere to hide. Rather than lie,
/// T3 keeps rendering on the only card at 15 fps with the sprite-atlas rig, and
/// [`DeviceChoice::dgpu_untouched`] reports `false` so the cost meter tells the
/// truth.
pub fn select(tier: Tier, gpus: &[GpuReading], cfg: &GovConfig) -> DeviceChoice {
    let discrete = gpus.iter().find(|g| g.id.kind == GpuKind::Discrete);
    let integrated = gpus.iter().find(|g| g.id.kind == GpuKind::Integrated);
    // On a single-GPU box `primary` is whatever exists; on the operator's
    // desktop it is the 7900 XTX. An unidentified card is preferred over an
    // integrated one here for the same reason it is never borrowed: we assume
    // the worst about what it is.
    let primary = discrete
        .or_else(|| gpus.iter().find(|g| g.id.kind == GpuKind::Unknown))
        .or_else(|| gpus.first());

    let hide_on_igpu = matches!(tier, Tier::Reduced | Tier::Lobotomised);
    let sole = gpus.len() <= 1;
    let target = |g: &GpuReading| GpuTarget::from_reading(g, vram_budget(tier, g, cfg, sole));

    let (render, note) = match tier {
        Tier::Dormant => (None, "she is dormant and nothing is drawn".to_string()),
        _ if !hide_on_igpu => match primary {
            Some(g) => (
                Some(target(g)),
                format!(
                    "rig on the {} card ({})",
                    kind_word(g.id.kind),
                    g.id.pci_slot
                ),
            ),
            None => (None, "no render-capable card was found".to_string()),
        },
        _ => match integrated.or(primary) {
            Some(g) if g.id.kind == GpuKind::Integrated => (
                Some(target(g)),
                format!(
                    "rig moved to the integrated card ({}) so the discrete one is untouched",
                    g.id.pci_slot
                ),
            ),
            Some(g) => (
                Some(target(g)),
                "this machine has only one GPU, so the rig stays on it at the T3 frame budget"
                    .to_string(),
            ),
            None => (None, "no render-capable card was found".to_string()),
        },
    };

    let inference = if !tier.may_hold_model() {
        None
    } else if hide_on_igpu {
        match integrated {
            Some(g) if g.vram_total_mib >= cfg.igpu_inference_min_mib => Some(target(g)),
            // Integrated card too small (or absent): the reflex model stays on
            // the discrete card but capped hard, which is what T2 means.
            _ => primary.map(target),
        }
    } else {
        primary.map(target)
    };

    // An `Unknown` card counts as one we must not touch: if we could not tell
    // what it was, we do not get to claim the operator's card is untouched.
    let dgpu_untouched = !gpus
        .iter()
        .filter(|g| g.id.kind != GpuKind::Integrated)
        .any(|g| {
            [&render, &inference]
                .into_iter()
                .flatten()
                .any(|t| t.pci_slot == g.id.pci_slot)
        });

    let note = match (&inference, tier) {
        (_, Tier::Dormant) => note,
        (None, _) => format!("{note}; no model resident"),
        (Some(i), _) => format!(
            "{note}; model on the {} card, budget {} MiB",
            kind_word(i.kind),
            i.vram_budget_mib
        ),
    };

    DeviceChoice {
        render,
        inference,
        dgpu_untouched,
        note,
    }
}

/// Convenience over a whole snapshot.
pub fn select_for(tier: Tier, s: &Snapshot, cfg: &GovConfig) -> DeviceChoice {
    select(tier, &s.gpus, cfg)
}

fn kind_word(k: GpuKind) -> &'static str {
    match k {
        GpuKind::Discrete => "discrete",
        GpuKind::Integrated => "integrated",
        GpuKind::Unknown => "unidentified",
    }
}

/// How much VRAM she may hold on this card at this tier.
///
/// `sole_card` is true when this is the only render-capable card on the
/// machine, in which case T3 has nowhere to hide and must still be allowed the
/// sprite-atlas rig's few hundred MiB.
fn vram_budget(tier: Tier, g: &GpuReading, cfg: &GovConfig, sole_card: bool) -> u64 {
    let cap = |mib: u64| mib.min(g.vram_total_mib);
    match tier {
        Tier::Dormant => 0,
        Tier::Lobotomised => {
            if g.id.kind == GpuKind::Discrete && !sole_card {
                // The whole point of T3: zero discrete VRAM.
                0
            } else {
                cap(cfg.lobotomised_vram_budget_mib)
            }
        }
        Tier::Reduced => cap(
            cfg.reduced_vram_budget_mib
                .min(g.vram_total_mib.saturating_sub(256).max(1)),
        ),
        Tier::Full | Tier::Feral => g.vram_total_mib.saturating_sub(cfg.vram_headroom_mib),
    }
}
