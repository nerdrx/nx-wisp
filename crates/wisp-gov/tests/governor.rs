//! The whole loop: registry delivery, F66 accounting, and the event contract.

mod common;

use std::sync::{Arc, Mutex};

use wisp_gov::{
    accounting::{report, FreeThresholds},
    config::GovConfig,
    device,
    fakes::{Machine, Replay, Spy},
    probe::selfcost::{MeasuredCost, SelfCostProbe},
    registry::{Registry, Shared},
    Governor,
};
use wisp_proto::{Cost, EventKind, Governed, Tier, TierReason};

fn spy(reg: &mut Registry, name: &str) -> Arc<Mutex<Spy>> {
    let s = Shared::new(Spy::new());
    let handle = s.handle();
    reg.register(name, s);
    handle
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[test]
fn a_downgrade_reaches_every_subsystem_before_step_returns() {
    let _g = common::isolate("gov");
    let base = Machine::desktop();
    let frames = vec![
        base.clone().at(0).build(),
        base.clone().at(1_000).wivrn_streaming().build(),
    ];
    let mut gov = Governor::with_source(GovConfig::default(), Box::new(Replay::new(frames)));

    let mind = spy(gov.registry(), "wisp-mind");
    let paint = spy(gov.registry(), "wisp-paint");
    let voice = spy(gov.registry(), "wisp-voice");

    gov.step();
    assert!(mind.lock().unwrap().calls.is_empty(), "no change, no calls");

    let step = gov.step();
    assert_eq!(step.tier, Tier::Lobotomised);
    // Synchronously: by the time `step` returned, everyone had been told.
    for (name, s) in [("mind", &mind), ("paint", &paint), ("voice", &voice)] {
        let s = s.lock().unwrap();
        assert_eq!(s.tiers(), vec![Tier::Lobotomised], "{name} was not told");
        assert_eq!(s.calls[0].1, TierReason::VrSession);
    }
}

#[test]
fn subsystems_are_told_in_registration_order() {
    let _g = common::isolate("gov");
    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    struct Recorder(&'static str, Arc<Mutex<Vec<&'static str>>>);
    impl Governed for Recorder {
        fn set_tier(&mut self, _t: Tier, _r: &TierReason) {
            self.1.lock().unwrap().push(self.0);
        }
        fn cost_at(_t: Tier) -> Cost {
            Cost::FREE
        }
    }

    let mut reg = Registry::new();
    reg.register("first", Recorder("first", Arc::clone(&order)));
    reg.register("second", Recorder("second", Arc::clone(&order)));
    reg.register("third", Recorder("third", Arc::clone(&order)));
    reg.downgrade(Tier::Lobotomised, &TierReason::VrSession);

    assert_eq!(*order.lock().unwrap(), vec!["first", "second", "third"]);
    assert_eq!(reg.names(), vec!["first", "second", "third"]);
}

#[test]
fn the_registry_sums_declared_costs_per_tier() {
    let _g = common::isolate("gov");
    let mut reg = Registry::new();
    spy(&mut reg, "a");
    spy(&mut reg, "b");

    let t1 = reg.estimate_total(Tier::Full);
    let t3 = reg.estimate_total(Tier::Lobotomised);
    let t4 = reg.estimate_total(Tier::Dormant);

    assert_eq!(t1.vram_mib, 6_000);
    assert_eq!(t3.vram_mib, 0, "T3 declares no VRAM at all");
    assert!(t3.ram_mib < t1.ram_mib);
    assert_eq!(t4, Cost::FREE);
    assert_eq!(reg.estimate(Tier::Full).len(), 2);
}

#[test]
fn a_subsystem_that_panics_does_not_wedge_the_governor() {
    let _g = common::isolate("gov");
    struct Bomb;
    impl Governed for Bomb {
        fn set_tier(&mut self, _t: Tier, _r: &TierReason) {
            panic!("subsystem exploded");
        }
        fn cost_at(_t: Tier) -> Cost {
            Cost::FREE
        }
    }

    let shared = Shared::new(Bomb);
    let mut reg = Registry::new();
    reg.register("bomb", shared.clone());

    // The panic escapes this call, poisoning the lock...
    let boom = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reg.downgrade(Tier::Lobotomised, &TierReason::VrSession);
    }));
    assert!(boom.is_err());

    // ...but a poisoned lock must not take the governor with it: losing the
    // governor is the one failure the charter cannot tolerate.
    let again = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reg.downgrade(Tier::Dormant, &TierReason::PowerCritical);
    }));
    assert!(again.is_err(), "it panics again, rather than deadlocking");
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[test]
fn an_event_is_emitted_when_and_only_when_the_tier_changes() {
    let _g = common::isolate("gov");
    let base = Machine::desktop();
    let mut frames = vec![base.clone().at(0).build()];
    // Ten seconds of VR: one downgrade, then nothing.
    for i in 1..=10u64 {
        frames.push(base.clone().at(i * 1_000).wivrn_streaming().build());
    }
    // And ten quiet seconds: one upgrade, then nothing.
    for i in 11..=25u64 {
        frames.push(base.clone().at(i * 1_000).build());
    }

    let mut gov = Governor::with_source(GovConfig::default(), Box::new(Replay::new(frames)));
    let mut events = Vec::new();
    for _ in 0..26 {
        if let Some(e) = gov.step().event() {
            events.push(e);
        }
    }

    assert_eq!(events.len(), 2, "got {events:?}");
    match &events[0].kind {
        EventKind::TierChanged { from, to, reason } => {
            assert_eq!((*from, *to), (Tier::Full, Tier::Lobotomised));
            assert_eq!(reason, &TierReason::VrSession);
        }
        other => panic!("wrong event: {other:?}"),
    }
    assert_eq!(events[0].at, 1_000, "the downgrade was not instant");
    match &events[1].kind {
        EventKind::TierChanged { from, to, .. } => {
            assert_eq!((*from, *to), (Tier::Lobotomised, Tier::Full));
        }
        other => panic!("wrong event: {other:?}"),
    }
    assert_eq!(events[1].at, 11_000 + GovConfig::default().dwell.to_full_ms);
}

#[test]
fn pinning_through_the_governor_broadcasts_immediately() {
    let _g = common::isolate("gov");
    let frames = vec![Machine::desktop().build()];
    let mut gov = Governor::with_source(GovConfig::default(), Box::new(Replay::new(frames)));
    let mind = spy(gov.registry(), "wisp-mind");

    let c = gov.pin(Tier::Dormant, 0).expect("pin changes the tier");
    assert_eq!(c.reason, TierReason::Pinned);
    assert_eq!(mind.lock().unwrap().tiers(), vec![Tier::Dormant]);
    assert_eq!(gov.pinned(), Some(Tier::Dormant));
    assert_eq!(gov.explanation(), "T4 because you pinned her there");
}

// ---------------------------------------------------------------------------
// F66 — the cost meter
// ---------------------------------------------------------------------------

/// A `/proc/self` we control, so the measured half of the meter is testable.
fn fake_proc_self(root: &common::TempDir, rss_kib: u64, vram: &[(&str, u64)]) -> SelfCostProbe {
    root.write("proc-self/status", &format!("Name:\tnx-wisp\nVmRSS:\t{rss_kib} kB\n"));
    root.write(
        "proc-self/stat",
        "1 (nx-wisp) S 1 1 1 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 8 0 900 0 0",
    );
    root.mkdir("proc-self/fdinfo");
    for (i, (pdev, kib)) in vram.iter().enumerate() {
        root.write(
            &format!("proc-self/fdinfo/{i}"),
            &format!(
                "pos:\t0\ndrm-driver:\tamdgpu\ndrm-client-id:\t{i}\ndrm-pdev:\t{pdev}\n\
                 drm-resident-vram:\t{kib} KiB\n"
            ),
        );
    }
    SelfCostProbe::with_root(root.join("proc-self"))
}

#[test]
fn at_t3_she_is_measured_to_be_costing_nothing_and_says_so() {
    let root = common::isolate("gov");
    // 55 MiB RSS, nothing on either card: the T3 target from the plan.
    let probe = fake_proc_self(&root, 55 * 1024, &[]);

    let frames = vec![
        Machine::desktop().at(0).build(),
        Machine::desktop().at(1_000).wivrn_streaming().build(),
    ];
    let mut gov = Governor::with_source(GovConfig::default(), Box::new(Replay::new(frames)))
        .with_selfcost(probe);
    spy(gov.registry(), "wisp-mind");

    gov.step();
    let step = gov.step();

    assert_eq!(step.tier, Tier::Lobotomised);
    assert_eq!(step.cost.measured.rss_mib, 55);
    assert_eq!(step.cost.dgpu_vram_mib, 0);
    assert!(step.cost.dgpu_untouched);
    assert_eq!(step.cost.headline, "she is currently costing you nothing");
    assert_eq!(step.cost.because, "T3 because WiVRn is streaming");
    // The estimate is reported alongside, never instead of.
    assert_eq!(step.cost.estimated.vram_mib, 0);
    assert_eq!(step.cost.by_subsystem.len(), 1);
}

#[test]
fn the_meter_will_not_claim_the_card_is_free_while_we_are_still_on_it() {
    let root = common::isolate("gov");
    // We say we are at T3, but the measurement says 1 GiB is still resident on
    // the discrete card. The measurement wins.
    let mut probe = fake_proc_self(&root, 55 * 1024, &[("0000:03:00.0", 1024 * 1024)]);
    let measured = probe.read();
    assert_eq!(measured.vram_on("0000:03:00.0"), 1024);

    let snap = Machine::desktop().wivrn_streaming().build();
    let devices = device::select_for(Tier::Lobotomised, &snap, &GovConfig::default());
    assert!(devices.dgpu_untouched, "the plan says we are off the card");

    let r = report(
        Tier::Lobotomised,
        "T3 because WiVRn is streaming",
        vec![],
        measured,
        &devices,
        &snap,
        FreeThresholds::default(),
    );
    assert!(!r.dgpu_untouched, "but we are measurably still on it");
    assert_eq!(r.dgpu_vram_mib, 1024);
    assert!(r.headline.contains("1024 MiB of VRAM"), "{}", r.headline);
}

#[test]
fn vram_from_two_fds_onto_one_drm_client_is_not_counted_twice() {
    let root = common::isolate("gov");
    let line = |client: &str| {
        format!(
            "drm-driver:\tamdgpu\ndrm-client-id:\t{client}\ndrm-pdev:\t0000:03:00.0\n\
             drm-resident-vram:\t1048576 KiB\n"
        )
    };
    root.mkdir("proc-self/fdinfo");
    root.write("proc-self/fdinfo/3", &line("29107"));
    root.write("proc-self/fdinfo/4", &line("29107")); // same client, second fd
    root.write("proc-self/fdinfo/5", &line("29108")); // a genuinely second client
    root.write("proc-self/status", "VmRSS:\t1024 kB\n");
    root.write(
        "proc-self/stat",
        "1 (nx-wisp) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0",
    );

    let mut probe = SelfCostProbe::with_root(root.join("proc-self"));
    let m = probe.read();
    assert_eq!(m.vram_on("0000:03:00.0"), 2048, "1 GiB per client, two clients");
}

#[test]
fn a_busy_wisp_reports_real_numbers_rather_than_a_nice_sentence() {
    let _g = common::isolate("gov");
    let snap = Machine::desktop().build();
    let devices = device::select_for(Tier::Full, &snap, &GovConfig::default());
    let measured = MeasuredCost {
        rss_mib: 480,
        cpu_centi_pct: 1_250, // 12.5% of a core
        vram_mib_by_pci: [("0000:03:00.0".to_string(), 18_000u64)]
            .into_iter()
            .collect(),
    };
    let r = report(
        Tier::Full,
        "T1 because you are at the desk and nothing heavy is running",
        vec![("wisp-mind".into(), Cost { ram_mib: 120, vram_mib: 18_000, cpu_centi_pct: 200 })],
        measured,
        &devices,
        &snap,
        FreeThresholds::default(),
    );
    assert!(!r.dgpu_untouched);
    assert_eq!(
        r.headline,
        "she is costing you 480 MiB of RAM, 12.5% of a core and 18000 MiB of VRAM"
    );
    assert_eq!(r.estimated.vram_mib, 18_000);
}

#[test]
fn the_first_cpu_sample_reports_zero_rather_than_a_fabricated_rate() {
    let root = common::isolate("gov");
    let mut probe = fake_proc_self(&root, 1024, &[]);
    assert_eq!(probe.read().cpu_centi_pct, 0);
}

#[test]
fn the_governor_polls_itself_less_often_where_being_late_is_harmless() {
    let _g = common::isolate("gov");
    let base = Machine::desktop();
    let frames = vec![
        base.clone().at(0).build(),
        base.clone().at(1_000).wivrn_streaming().build(),
    ];
    let cfg = GovConfig::default();
    let mut gov = Governor::with_source(cfg.clone(), Box::new(Replay::new(frames)));

    gov.step();
    assert_eq!(gov.tier(), Tier::Full);
    assert_eq!(gov.poll_interval_ms(), cfg.cadence.t1_ms);

    gov.step();
    assert_eq!(gov.tier(), Tier::Lobotomised);
    assert_eq!(gov.poll_interval_ms(), cfg.cadence.t3_ms);
    assert!(
        cfg.cadence.t3_ms > cfg.cadence.t1_ms,
        "perception costs ~4ms a poll; at T3 that has to be rationed"
    );

    gov.pin(Tier::Dormant, 2_000);
    assert_eq!(gov.poll_interval_ms(), cfg.cadence.t4_ms);
}

// ---------------------------------------------------------------------------
// Device selection through the whole loop
// ---------------------------------------------------------------------------

#[test]
fn the_devices_move_with_the_tier_across_a_whole_session() {
    let _g = common::isolate("gov");
    let base = Machine::desktop();
    let mut frames = vec![base.clone().at(0).build()];
    for i in 1..=5u64 {
        frames.push(base.clone().at(i * 1_000).wivrn_streaming().build());
    }
    for i in 6..=25u64 {
        frames.push(base.clone().at(i * 1_000).build());
    }

    let mut gov = Governor::with_source(GovConfig::default(), Box::new(Replay::new(frames)));
    let mut trail = Vec::new();
    for _ in 0..26 {
        let s = gov.step();
        trail.push((s.tier, s.devices.dgpu_untouched, s.vram.dgpu_mib));
    }

    assert_eq!(trail[0], (Tier::Full, false, 23_024));
    // The very first VR sample: off the card, budget zero, in one step.
    assert_eq!(trail[1], (Tier::Lobotomised, true, 0));
    assert!(trail[1..6].iter().all(|(t, u, v)| *t == Tier::Lobotomised && *u && *v == 0));
    // And back afterwards.
    assert_eq!(trail.last().unwrap().0, Tier::Full);
    assert!(!trail.last().unwrap().1);
}
