//! F61 — the two-GPU trick, and F13's VRAM budget.

mod common;

use wisp_gov::{
    config::GovConfig,
    device::select,
    fakes::{one_gpu_machine, two_gpu_machine, Machine},
    reading::GpuKind,
    vram,
};
use wisp_proto::Tier;

const DGPU: &str = "0000:03:00.0";
const IGPU: &str = "0000:7b:00.0";

fn gpus() -> Vec<wisp_gov::reading::GpuReading> {
    two_gpu_machine(24_560, 2_048)
}

#[test]
fn at_t0_and_t1_she_uses_the_discrete_card_for_everything() {
    let _g = common::isolate("device");
    let cfg = GovConfig::default();
    for tier in [Tier::Feral, Tier::Full] {
        let c = select(tier, &gpus(), &cfg);
        assert_eq!(c.render.as_ref().unwrap().pci_slot, DGPU, "{tier:?}");
        assert_eq!(c.inference.as_ref().unwrap().pci_slot, DGPU, "{tier:?}");
        assert!(!c.dgpu_untouched);
        // Nearly the whole card, less the headroom we always leave.
        assert_eq!(c.render.unwrap().vram_budget_mib, 24_560 - 1_536);
    }
}

#[test]
fn at_t2_and_t3_both_move_to_the_integrated_card() {
    let _g = common::isolate("device");
    let cfg = GovConfig::default();

    let t2 = select(Tier::Reduced, &gpus(), &cfg);
    assert_eq!(t2.render.as_ref().unwrap().pci_slot, IGPU);
    assert_eq!(t2.inference.as_ref().unwrap().pci_slot, IGPU);
    assert!(t2.dgpu_untouched, "T2 must not touch the 7900 XTX: {}", t2.note);

    let t3 = select(Tier::Lobotomised, &gpus(), &cfg);
    assert_eq!(t3.render.as_ref().unwrap().pci_slot, IGPU);
    // SPEC §3.1: at T3 the model is fully unloaded, so there is no inference
    // device at all — not even on the integrated card.
    assert!(t3.inference.is_none());
    assert!(t3.dgpu_untouched);
    assert!(!t3.uses(DGPU));
}

#[test]
fn at_t4_nothing_is_drawn_and_nothing_is_resident() {
    let _g = common::isolate("device");
    let c = select(Tier::Dormant, &gpus(), &GovConfig::default());
    assert!(c.render.is_none());
    assert!(c.inference.is_none());
    assert!(c.dgpu_untouched);
}

#[test]
fn device_selection_follows_the_tier_all_the_way_down_and_back() {
    let _g = common::isolate("device");
    let cfg = GovConfig::default();
    let trail: Vec<(Tier, bool)> = [
        Tier::Feral,
        Tier::Full,
        Tier::Reduced,
        Tier::Lobotomised,
        Tier::Dormant,
        Tier::Full,
    ]
    .into_iter()
    .map(|t| (t, select(t, &gpus(), &cfg).dgpu_untouched))
    .collect();
    assert_eq!(
        trail,
        vec![
            (Tier::Feral, false),
            (Tier::Full, false),
            (Tier::Reduced, true),
            (Tier::Lobotomised, true),
            (Tier::Dormant, true),
            (Tier::Full, false),
        ]
    );
}

#[test]
fn a_tiny_integrated_card_is_not_worth_running_a_model_on() {
    let _g = common::isolate("device");
    let cfg = GovConfig::default();
    // The laptop's 512 MiB carve-out is below `igpu_inference_min_mib`, so the
    // reflex model stays on the discrete card at T2 — but hard-capped.
    let gpus = two_gpu_machine(6_144, 512);
    let c = select(Tier::Reduced, &gpus, &cfg);
    assert_eq!(c.render.as_ref().unwrap().pci_slot, IGPU, "the rig still moves");
    let inf = c.inference.as_ref().unwrap();
    assert_eq!(inf.pci_slot, DGPU);
    assert_eq!(inf.vram_budget_mib, cfg.reduced_vram_budget_mib);
    assert!(!c.dgpu_untouched, "and we say so, instead of pretending");
}

#[test]
fn with_one_card_there_is_nowhere_to_hide_and_she_admits_it() {
    let _g = common::isolate("device");
    let cfg = GovConfig::default();
    let c = select(Tier::Lobotomised, &one_gpu_machine(8_192), &cfg);
    let r = c.render.as_ref().unwrap();
    assert_eq!(r.pci_slot, DGPU);
    assert_eq!(r.vram_budget_mib, cfg.lobotomised_vram_budget_mib);
    assert!(!c.dgpu_untouched);
    assert!(c.note.contains("only one GPU"), "note was {:?}", c.note);
}

#[test]
fn an_unidentified_card_is_never_borrowed_and_never_claimed_as_untouched() {
    let _g = common::isolate("device");
    let mut gpus = gpus();
    gpus[1].id.kind = GpuKind::Unknown;
    let c = select(Tier::Lobotomised, &gpus, &GovConfig::default());
    // No integrated card we trust, so the rig has to stay where it was...
    assert_eq!(c.render.as_ref().unwrap().pci_slot, DGPU);
    // ...and we must not tell the operator their card is free.
    assert!(!c.dgpu_untouched);
}

#[test]
fn a_target_is_consumable_by_wgpu_and_by_llama_cpp() {
    let _g = common::isolate("device");
    let c = select(Tier::Lobotomised, &gpus(), &GovConfig::default());
    let t = c.render.unwrap();

    // wgpu: match on the adapter info it enumerated.
    assert!(t.matches_adapter(0x1002, 0x13c0));
    assert!(!t.matches_adapter(0x1002, 0x744c));

    // llama.cpp / ggml-vulkan: an index into whatever the backend enumerated,
    // in the backend's own order.
    let enumerated = [(0x1002, 0x744c), (0x1002, 0x13c0)];
    assert_eq!(t.index_in(&enumerated), Some(1));
    assert_eq!(t.index_in(&[(0x8086, 0x1234)]), None);

    // And the environment that forces an unmodified child onto the same card.
    assert_eq!(t.vk_device_select(), "1002:13c0");
    let env = t.env_hints();
    assert!(env
        .iter()
        .any(|(k, v)| k == "MESA_VK_DEVICE_SELECT" && v == "1002:13c0"));
    assert!(env
        .iter()
        .any(|(k, v)| k == "WISP_DRM_RENDER_NODE" && v == "/dev/dri/renderD129"));
}

#[test]
fn the_enumeration_index_is_pci_ordered_not_card_ordered() {
    let _g = common::isolate("device");
    // On the operator's desktop the discrete card is `card1` but comes first in
    // PCI order, and owns renderD128. Nothing may assume otherwise.
    let g = gpus();
    assert_eq!(g[0].id.card_index, 1);
    assert_eq!(g[0].id.enumeration_index, 0);
    assert_eq!(
        g[0].id.render_node.as_deref().unwrap().to_str().unwrap(),
        "/dev/dri/renderD128"
    );
    assert_eq!(g[1].id.card_index, 0);
    assert_eq!(g[1].id.enumeration_index, 1);
}

// ---------------------------------------------------------------------------
// F13
// ---------------------------------------------------------------------------

#[test]
fn the_vram_budget_goes_to_zero_the_moment_a_headset_appears() {
    let _g = common::isolate("device");
    let cfg = GovConfig::default();
    let quiet = Machine::desktop().build();

    let full = vram::budget(Tier::Full, &quiet, &cfg, 0, None);
    assert!(full.dgpu_mib > 20_000, "she may be greedy at T1: {full:?}");
    assert!(!full.evict_now);

    let vr = Machine::desktop().wivrn_streaming().build();
    let t3 = vram::budget(Tier::Lobotomised, &vr, &cfg, 0, Some(&full));
    assert_eq!(t3.dgpu_mib, 0);
    assert!(t3.evict_now, "somebody must free memory before we return");
    assert!(t3.note.contains("zero discrete VRAM"));
}

#[test]
fn she_does_not_evict_herself_in_a_loop_as_her_own_model_fills_the_card() {
    let _g = common::isolate("device");
    let cfg = GovConfig::default();
    // 18 GiB of the 24 GiB card is in use, and all of it is ours.
    let m = Machine::desktop().vram_used(18_432).build();
    let b = vram::budget(Tier::Full, &m, &cfg, 18_432, None);
    assert_eq!(b.dgpu_mib, 24_560 - 1_536, "our own memory is not somebody else's");

    // Same card, same usage, but it belongs to a game: the allowance collapses.
    let b2 = vram::budget(Tier::Full, &m, &cfg, 0, Some(&b));
    assert_eq!(b2.dgpu_mib, 24_560 - 18_432);
    assert!(b2.evict_now, "and we have to give it back");
}

#[test]
fn t2_holds_only_the_reflex_model() {
    let _g = common::isolate("device");
    let cfg = GovConfig::default();
    let m = Machine::desktop().build();
    let b = vram::budget(Tier::Reduced, &m, &cfg, 0, None);
    assert_eq!(b.dgpu_mib, cfg.reduced_vram_budget_mib);
    assert!(b.note.contains("mmapped in RAM"), "F62 is the promise here");
}

#[test]
fn the_budget_answers_per_card_kind() {
    let _g = common::isolate("device");
    let m = Machine::desktop().build();
    let b = vram::budget(Tier::Lobotomised, &m, &GovConfig::default(), 0, None);
    assert_eq!(b.for_kind(GpuKind::Discrete), 0);
    assert_eq!(b.for_kind(GpuKind::Unknown), 0, "unknown is treated as discrete");
    assert!(b.for_kind(GpuKind::Integrated) > 0);
}
