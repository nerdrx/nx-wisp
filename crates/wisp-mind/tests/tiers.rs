//! **F12 + F13 + SPEC §3.1** — the model manager under the governor.
//!
//! No GPU, no model, no Vulkan loader. The `MockBackend` distinguishes a warm
//! eviction from a cold one and reports what each costs, so the claim F12
//! actually makes — *"the way back is ~1 s, not 30"* — is asserted here rather
//! than asserted in a comment.

mod common;

use common::{gov, Fixture};
use wisp_gov::fakes::Machine;
use wisp_mind::backend::{Residency, Role};
use wisp_mind::error::MindError;
use wisp_mind::manager::ModelManager;
use wisp_proto::{Governed, Tier, TierReason};

fn quiet() -> TierReason {
    TierReason::Idle
}

fn game() -> TierReason {
    TierReason::HeavyProcess {
        name: "cyberpunk2077.exe".into(),
    }
}

fn headset() -> TierReason {
    TierReason::VrSession
}

#[test]
fn a_model_that_is_not_on_disk_is_a_sentence_not_a_crash() {
    let f = Fixture::new();
    let mut m = f.manager(f.backend());
    let err = m.ensure(Role::Reflex).unwrap_err();
    match err {
        MindError::ModelMissing { name, path } => {
            assert!(!name.is_empty());
            assert!(path.starts_with(&f.models_dir));
        }
        other => panic!("expected ModelMissing, got {other}"),
    }
    // And it is a plain "not here yet", not a tier refusal — the caller should
    // offer to fetch it, not queue the thought.
    assert!(!m.ensure(Role::Reflex).unwrap_err().is_tier_refusal());
}

#[test]
fn at_t2_the_big_model_goes_warm_and_the_small_one_stays() {
    let f = Fixture::new();
    f.place_all();
    let mut m = f.manager(f.backend());

    let (d, b) = gov::desktop(Tier::Full, None);
    m.apply(d, b.clone());
    m.set_tier(Tier::Full, &quiet());
    m.ensure(Role::Reflex).expect("reflex loads");
    m.ensure(Role::Deliberate).expect("deliberate loads");
    assert_eq!(m.residency(Role::Deliberate), Residency::Resident);
    let held_at_t1 = m.vram_held_mib();
    assert!(held_at_t1 > 18_000, "the 30B should be resident: {held_at_t1}");

    // Something substantial started.
    let (d2, b2) = gov::desktop(Tier::Reduced, Some(&b));
    m.apply(d2, b2.clone());
    m.set_tier(Tier::Reduced, &game());

    assert_eq!(
        m.residency(Role::Deliberate),
        Residency::Warm,
        "T2 evicts the deliberate model from VRAM but keeps it mmapped"
    );
    assert!(
        m.vram_held_mib() <= b2.dgpu_mib.max(b2.igpu_mib),
        "held {} MiB against a budget of {} / {}",
        m.vram_held_mib(),
        b2.dgpu_mib,
        b2.igpu_mib
    );
    // The reflex model is still usable — that is the whole point of T2.
    m.ensure(Role::Reflex).expect("reflex survives T2");
}

#[test]
fn coming_back_from_warm_is_a_second_and_coming_back_from_cold_is_not() {
    let f = Fixture::new();
    f.place_all();
    let mut m = f.manager(f.backend());
    let (d, b) = gov::desktop(Tier::Full, None);
    m.apply(d.clone(), b.clone());
    m.set_tier(Tier::Full, &quiet());
    m.ensure(Role::Deliberate).expect("loads");

    // Warm path.
    m.warm_evict(Role::Deliberate);
    assert_eq!(m.residency(Role::Deliberate), Residency::Warm);
    m.ensure(Role::Deliberate).expect("rewarms");
    let warm_ms = m
        .states()
        .into_iter()
        .find(|s| s.role == Role::Deliberate)
        .expect("slot")
        .last_load_ms;

    // Cold path.
    m.cold_unload(Role::Deliberate);
    assert_eq!(m.residency(Role::Deliberate), Residency::Cold);
    m.ensure(Role::Deliberate).expect("reloads");
    let cold_ms = m
        .states()
        .into_iter()
        .find(|s| s.role == Role::Deliberate)
        .expect("slot")
        .last_load_ms;

    assert!(
        warm_ms * 5 < cold_ms,
        "warm eviction has to be worth having: warm {warm_ms} ms vs cold {cold_ms} ms"
    );
    assert!(warm_ms < 2_000, "the way back should be about a second, was {warm_ms} ms");
}

#[test]
fn at_t3_the_card_belongs_to_whatever_the_operator_started() {
    let f = Fixture::new();
    f.place_all();
    let mut m = f.manager(f.backend());
    let (d, b) = gov::desktop(Tier::Full, None);
    m.apply(d, b.clone());
    m.set_tier(Tier::Full, &quiet());
    m.ensure(Role::Reflex).expect("loads");
    m.ensure(Role::Deliberate).expect("loads");
    assert!(m.vram_held_mib() > 0);

    let (d3, b3) = gov::desktop(Tier::Lobotomised, Some(&b));
    assert_eq!(b3.dgpu_mib, 0, "the governor says zero discrete VRAM at T3");
    assert!(b3.evict_now, "and says somebody has to free it now");
    m.apply(d3, b3);
    m.set_tier(Tier::Lobotomised, &headset());

    assert_eq!(m.vram_held_mib(), 0, "she must be holding nothing");
    for role in Role::ALL {
        assert_eq!(
            m.residency(role),
            Residency::Cold,
            "{role:?} should be fully unloaded at T3"
        );
    }
    // And she cannot quietly reload behind the governor's back.
    let err = m.ensure(Role::Reflex).unwrap_err();
    assert!(err.is_tier_refusal(), "{err}");
    assert!(matches!(
        err,
        MindError::NotAllowedAtTier {
            tier: Tier::Lobotomised
        }
    ));
}

#[test]
fn an_evict_now_budget_frees_memory_before_it_returns() {
    let f = Fixture::new();
    f.place_all();
    let mut m = f.manager(f.backend());
    let (d, b) = gov::desktop(Tier::Full, None);
    m.apply(d, b.clone());
    m.ensure(Role::Deliberate).expect("loads");
    assert!(m.vram_held_mib() > 18_000);

    // The governor noticed a headset. SPEC §3.1: this is synchronous, and the
    // memory is gone by the time `apply` returns — not on the next tick.
    let (d3, b3) = gov::desktop(Tier::Lobotomised, Some(&b));
    m.apply(d3, b3);
    assert_eq!(m.vram_held_mib(), 0);
}

#[test]
fn upgrades_are_lazy_so_alt_tabbing_out_of_a_game_and_back_costs_nothing() {
    let f = Fixture::new();
    f.place_all();
    let mut m = f.manager(f.backend());
    let (d, b) = gov::desktop(Tier::Full, None);
    m.apply(d.clone(), b.clone());
    m.set_tier(Tier::Full, &quiet());
    m.ensure(Role::Deliberate).expect("loads");

    let (d2, b2) = gov::desktop(Tier::Reduced, Some(&b));
    m.apply(d2, b2.clone());
    m.set_tier(Tier::Reduced, &game());
    assert_eq!(m.residency(Role::Deliberate), Residency::Warm);

    // Back to T1. SPEC §3.1: upgrades are lazy — nothing is loaded until
    // something asks, so a bounce costs no model load at all.
    let (d1, b1) = gov::desktop(Tier::Full, Some(&b2));
    m.apply(d1, b1);
    m.set_tier(Tier::Full, &quiet());
    assert_eq!(
        m.residency(Role::Deliberate),
        Residency::Warm,
        "an upgrade must not eagerly reload 18 GiB"
    );
    // And when something does ask, it is the cheap path.
    m.ensure(Role::Deliberate).expect("rewarms");
    assert_eq!(m.residency(Role::Deliberate), Residency::Resident);
}

#[test]
fn on_a_laptop_that_cannot_fit_the_model_she_runs_on_the_cpu_rather_than_failing() {
    let f = Fixture::new();
    f.place_all();
    let mut m = f.manager(f.backend());

    // 6 GiB card, and the 30B wants 18. `wisp-gov` offers the card; the manager
    // declines it rather than either failing or blowing the budget.
    let (d, b) = gov::laptop(Tier::Full, None);
    m.apply(d, b);
    m.ensure(Role::Deliberate).expect("still loads");
    let s = m
        .states()
        .into_iter()
        .find(|s| s.role == Role::Deliberate)
        .expect("slot");
    assert!(!s.on_gpu, "it should have gone to the CPU");
    assert_eq!(s.vram_mib, 0);
    assert_eq!(m.vram_held_mib(), 0);

    // The small one does fit.
    m.ensure(Role::Reflex).expect("loads");
    let r = m
        .states()
        .into_iter()
        .find(|s| s.role == Role::Reflex)
        .expect("slot");
    assert!(r.on_gpu, "1.6 GiB fits in a 6 GiB card");
}

#[test]
fn every_load_and_every_eviction_is_in_the_flight_recorder() {
    let f = Fixture::new();
    f.place_all();
    let mut m = f.manager(f.backend());
    let (d, b) = gov::desktop(Tier::Full, None);
    m.apply(d, b.clone());
    m.set_tier(Tier::Full, &quiet());
    m.ensure(Role::Reflex).expect("loads");
    m.ensure(Role::Deliberate).expect("loads");

    let (d3, b3) = gov::desktop(Tier::Lobotomised, Some(&b));
    m.apply(d3, b3);
    m.set_tier(Tier::Lobotomised, &headset());

    let models = f.log.models();
    assert!(
        models.iter().any(|(_, loaded, mib)| *loaded && *mib > 0),
        "a load with its VRAM cost: {models:?}"
    );
    assert!(
        models.iter().any(|(_, loaded, mib)| !*loaded && *mib == 0),
        "an eviction: {models:?}"
    );
    // SPEC §0.4: "why is she quiet?" is answerable from this alone.
    let unloads = models.iter().filter(|(_, loaded, _)| !*loaded).count();
    assert!(unloads >= 2, "both models were evicted: {models:?}");
}

#[test]
fn the_declared_cost_falls_monotonically_as_the_tier_tightens() {
    let mut last = u32::MAX;
    for tier in [
        Tier::Feral,
        Tier::Full,
        Tier::Reduced,
        Tier::Lobotomised,
        Tier::Dormant,
    ] {
        let c = ModelManager::cost_at(tier);
        assert!(
            c.vram_mib <= last,
            "{tier:?} claims {} MiB, more than the tier above it",
            c.vram_mib
        );
        last = c.vram_mib;
    }
    assert_eq!(
        ModelManager::cost_at(Tier::Lobotomised).vram_mib,
        0,
        "T3 must claim zero VRAM — it is the promise the whole governor makes"
    );
    assert_eq!(ModelManager::cost_at(Tier::Dormant), wisp_proto::Cost::FREE);
}

#[test]
fn the_governor_can_drive_the_manager_through_a_whole_gaming_session() {
    let f = Fixture::new();
    f.place_all();
    let mut m = f.manager(f.backend());
    let cfg = wisp_gov::GovConfig::default();

    // A morning: quiet, then a game, then a headset, then quiet again.
    let script = [
        (Machine::desktop().idle_ms(0).build(), Tier::Full),
        (Machine::desktop().game("cyberpunk2077").build(), Tier::Reduced),
        (
            Machine::desktop().game("cyberpunk2077").wivrn_streaming().build(),
            Tier::Lobotomised,
        ),
        (Machine::desktop().build(), Tier::Full),
    ];
    let mut previous: Option<wisp_gov::VramBudget> = None;
    for (snap, tier) in script {
        let device = wisp_gov::device::select_for(tier, &snap, &cfg);
        let budget = wisp_gov::vram::budget(tier, &snap, &cfg, m.vram_held_mib(), previous.as_ref());
        m.apply(device, budget.clone());
        m.set_tier(tier, &quiet());
        if tier.may_hold_model() {
            let _ = m.ensure(Role::Reflex);
        }
        // The invariant that matters, checked at every step of the session.
        assert!(
            m.vram_held_mib() <= budget.dgpu_mib.max(budget.igpu_mib),
            "{tier:?}: holding {} MiB against {} / {}",
            m.vram_held_mib(),
            budget.dgpu_mib,
            budget.igpu_mib
        );
        previous = Some(budget);
    }
}
