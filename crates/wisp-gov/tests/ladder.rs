//! F60 — the tier ladder. Injected probe sequences, expected trajectories.
//!
//! No GPU, no compositor, no clock: every timestamp is a number in a snapshot.

mod common;

use wisp_gov::{
    config::{DwellConfig, GovConfig},
    fakes::Machine,
    ladder::{classify, Ladder},
    reading::VrRuntimeKind,
};
use wisp_proto::{Tier, TierReason};

fn cfg() -> GovConfig {
    GovConfig::default()
}

/// Settle the ladder into a known tier without asserting on the way.
fn settle(l: &mut Ladder, m: Machine, until_ms: u64, step_ms: u64) {
    let mut t = m.snapshot().at;
    while t <= until_ms {
        l.step(&m.clone().at(t).build());
        t += step_ms;
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

#[test]
fn a_quiet_desktop_with_the_operator_present_is_t1() {
    let _g = common::isolate("ladder");
    let v = classify(&Machine::desktop().build(), &cfg(), Tier::Full);
    assert_eq!(v.tier, Tier::Full);
    assert_eq!(v.reason, TierReason::Idle);
    assert_eq!(
        v.explanation,
        "T1 because you are at the desk and nothing heavy is running"
    );
}

#[test]
fn an_idle_machine_with_the_operator_away_is_t0() {
    let _g = common::isolate("ladder");
    let m = Machine::desktop().idle_ms(600_000).build();
    let v = classify(&m, &cfg(), Tier::Full);
    assert_eq!(v.tier, Tier::Feral);
    assert!(v.explanation.starts_with("T0 because you have been away 10m"));
}

#[test]
fn the_readout_names_wivrn() {
    let _g = common::isolate("ladder");
    let v = classify(&Machine::desktop().wivrn_streaming().build(), &cfg(), Tier::Full);
    assert_eq!(v.tier, Tier::Lobotomised);
    assert_eq!(v.reason, TierReason::VrSession);
    // This exact sentence is what the operator sees. It is part of the contract.
    assert_eq!(v.explanation, "T3 because WiVRn is streaming");
}

#[test]
fn a_vr_runtime_that_has_not_connected_a_headset_is_still_t3() {
    let _g = common::isolate("ladder");
    // The charter says she costs nothing *when it matters*, and a headset about
    // to go on is exactly when it matters. We do not wait for frames to move.
    let v = classify(&Machine::desktop().wivrn_idle().build(), &cfg(), Tier::Full);
    assert_eq!(v.tier, Tier::Lobotomised);
    assert_eq!(v.explanation, "T3 because WiVRn is running");
}

#[test]
fn other_vr_runtimes_are_named_too() {
    let _g = common::isolate("ladder");
    for (kind, proc_name, word) in [
        (VrRuntimeKind::Monado, "monado-service", "Monado"),
        (VrRuntimeKind::SteamVr, "vrserver", "SteamVR"),
        (VrRuntimeKind::Alvr, "alvr_server", "ALVR"),
    ] {
        let m = Machine::desktop().vr(kind, proc_name, true).build();
        let v = classify(&m, &cfg(), Tier::Full);
        assert_eq!(v.tier, Tier::Lobotomised);
        assert_eq!(v.explanation, format!("T3 because {word} is streaming"));
    }
}

#[test]
fn a_fullscreen_surface_is_t3_and_carries_the_app_id() {
    let _g = common::isolate("ladder");
    let m = Machine::desktop().fullscreen("steam_app_1091500").build();
    let v = classify(&m, &cfg(), Tier::Full);
    assert_eq!(v.tier, Tier::Lobotomised);
    assert_eq!(
        v.reason,
        TierReason::Fullscreen {
            app_id: "steam_app_1091500".into()
        }
    );
}

#[test]
fn gpu_pressure_is_t2_and_vram_pressure_beats_it() {
    let _g = common::isolate("ladder");
    let busy = classify(&Machine::desktop().gpu_busy(60).build(), &cfg(), Tier::Full);
    assert_eq!(busy.tier, Tier::Reduced);
    assert_eq!(busy.reason, TierReason::GpuPressure { busy_pct: 60 });

    // Nearly out of VRAM: that is the failure that actually stutters a game.
    let vram = classify(
        &Machine::desktop().gpu_busy(60).vram_used(23_800).build(),
        &cfg(),
        Tier::Full,
    );
    assert_eq!(
        vram.reason,
        TierReason::VramPressure {
            used_mib: 23_800,
            total_mib: 24_560
        }
    );
}

#[test]
fn vram_headroom_is_absolute_so_a_small_card_is_protected_too() {
    let _g = common::isolate("ladder");
    // 6 GiB laptop dGPU at 5 GiB used: only 1 GiB free, under the 1536 MiB
    // headroom, even though the *fraction* used is under the 85% band.
    let m = Machine::laptop().vram_used(5_120).build();
    let v = classify(&m, &cfg(), Tier::Full);
    assert_eq!(v.tier, Tier::Reduced);
    assert!(matches!(v.reason, TierReason::VramPressure { .. }));
}

#[test]
fn cpu_pressure_names_the_process_responsible() {
    let _g = common::isolate("ladder");
    // 32 cores, load 30 => 0.94 per core, over the 0.70 band.
    let m = Machine::desktop().load(30.0).top_cpu("cargo", 2900).build();
    let v = classify(&m, &cfg(), Tier::Full);
    assert_eq!(v.tier, Tier::Reduced);
    assert_eq!(v.explanation, "T2 because cargo is eating the CPU");
}

#[test]
fn thermal_emergency_is_t4() {
    let _g = common::isolate("ladder");
    let v = classify(&Machine::desktop().temp(97).build(), &cfg(), Tier::Full);
    assert_eq!(v.tier, Tier::Dormant);
    assert_eq!(v.reason, TierReason::PowerCritical);
}

#[test]
fn a_dying_battery_is_t4_but_a_low_one_is_only_t2() {
    let _g = common::isolate("ladder");
    let dying = classify(&Machine::laptop().battery(7, true).build(), &cfg(), Tier::Full);
    assert_eq!(dying.tier, Tier::Dormant);

    let low = classify(&Machine::laptop().battery(25, true).build(), &cfg(), Tier::Full);
    assert_eq!(low.tier, Tier::Reduced);
    assert!(low.explanation.contains("frugal"));
}

#[test]
fn a_peripheral_battery_can_never_matter() {
    let _g = common::isolate("ladder");
    // The operator's desktop reports exactly one power supply: their mouse.
    // `PowerReading` has already filtered `scope=Device`, so the snapshot has
    // no battery at all and the desktop stays at T1.
    let m = Machine::desktop().build();
    assert_eq!(m.power.battery_pct, None);
    assert_eq!(classify(&m, &cfg(), Tier::Full).tier, Tier::Full);
}

// ---------------------------------------------------------------------------
// Hysteresis
// ---------------------------------------------------------------------------

#[test]
fn downgrades_are_instant_and_upgrades_are_not() {
    let _g = common::isolate("ladder");
    let mut l = Ladder::new(cfg());
    let quiet = Machine::desktop();

    assert!(l.step(&quiet.clone().at(0).build()).is_none());
    assert_eq!(l.tier(), Tier::Full);

    // A game appears. One sample is enough.
    let change = l
        .step(&quiet.clone().at(1_000).fullscreen("gamescope").build())
        .expect("downgrade must be instant");
    assert_eq!((change.from, change.to), (Tier::Full, Tier::Lobotomised));
    assert!(change.is_downgrade());

    // It exits. One sample is *not* enough.
    assert!(l.step(&quiet.clone().at(2_000).build()).is_none());
    assert_eq!(l.tier(), Tier::Lobotomised);
    assert_eq!(l.pending(), Some((Tier::Full, 2_000)));

    // Still not enough, one millisecond short of the dwell.
    assert!(l
        .step(&quiet.clone().at(2_000 + cfg().dwell.to_full_ms - 1).build())
        .is_none());

    // Now.
    let up = l
        .step(&quiet.clone().at(2_000 + cfg().dwell.to_full_ms).build())
        .expect("upgrade after dwell");
    assert_eq!((up.from, up.to), (Tier::Lobotomised, Tier::Full));
    assert!(!up.is_downgrade());
}

#[test]
fn alt_tabbing_in_and_out_of_a_game_never_flaps_the_tier() {
    let _g = common::isolate("ladder");
    let mut l = Ladder::new(cfg());
    let base = Machine::desktop();

    // Get to T3.
    let first = l.step(&base.clone().at(0).fullscreen("gamescope").build());
    assert_eq!(first.map(|c| c.to), Some(Tier::Lobotomised));

    // Now flap the fullscreen state every second for two minutes. The dwell is
    // 8s, so an upgrade would need eight consecutive quiet samples and never
    // gets them.
    let mut changes = Vec::new();
    for i in 1..=120u64 {
        let t = i * 1_000;
        let m = if i % 2 == 0 {
            base.clone().at(t).fullscreen("gamescope")
        } else {
            base.clone().at(t).windowed()
        };
        if let Some(c) = l.step(&m.build()) {
            changes.push(c);
        }
    }
    assert!(
        changes.is_empty(),
        "the tier flapped {} times: {:?}",
        changes.len(),
        changes.iter().map(|c| (c.from, c.to)).collect::<Vec<_>>()
    );
    assert_eq!(l.tier(), Tier::Lobotomised);

    // Quiet for real: she comes back once, and only once.
    let mut ups = Vec::new();
    for i in 121..=140u64 {
        if let Some(c) = l.step(&base.clone().at(i * 1_000).build()) {
            ups.push(c);
        }
    }
    assert_eq!(ups.len(), 1, "expected exactly one upgrade, got {ups:?}");
    assert_eq!(ups[0].to, Tier::Full);
}

#[test]
fn a_metric_hovering_on_the_threshold_does_not_flap_either() {
    let _g = common::isolate("ladder");
    // Band is enter 45%, release 25%. Oscillating between 40 and 50 trips the
    // downgrade once and then never clears, because 40 is still above 25.
    let mut l = Ladder::new(cfg());
    let base = Machine::desktop();
    let mut changes = Vec::new();
    for i in 0..200u64 {
        let busy = if i % 2 == 0 { 50 } else { 40 };
        if let Some(c) = l.step(&base.clone().at(i * 500).gpu_busy(busy).build()) {
            changes.push((c.from, c.to));
        }
    }
    assert_eq!(changes, vec![(Tier::Full, Tier::Reduced)]);
}

#[test]
fn a_climb_settles_for_the_worst_candidate_in_its_window() {
    let _g = common::isolate("ladder");
    // From T3, the machine goes quiet (T1 is justified) but partway through the
    // dwell the GPU gets busy again (only T2 is justified). She must land on T2
    // at the end of the *original* window, not restart the clock and not
    // optimistically take T1.
    let mut l = Ladder::new(cfg());
    let base = Machine::desktop();
    l.step(&base.clone().at(0).fullscreen("gamescope").build());
    assert_eq!(l.tier(), Tier::Lobotomised);

    l.step(&base.clone().at(1_000).build()); // T1 candidate, window opens
    l.step(&base.clone().at(2_000).gpu_busy(60).build()); // T2 candidate, settles down
    let c = l
        .step(&base.clone().at(1_000 + cfg().dwell.to_full_ms).build())
        .expect("lands at the end of the original window");
    assert_eq!(c.to, Tier::Reduced);
}

#[test]
fn dwell_times_are_configurable() {
    let _g = common::isolate("ladder");
    let mut c = cfg();
    c.dwell = DwellConfig {
        to_full_ms: 500,
        ..DwellConfig::default()
    };
    let mut l = Ladder::new(c);
    let base = Machine::desktop();
    l.step(&base.clone().at(0).fullscreen("x").build());
    l.step(&base.clone().at(100).build());
    let up = l.step(&base.clone().at(600).build()).expect("short dwell honoured");
    assert_eq!(up.to, Tier::Full);
}

#[test]
fn climbing_to_feral_is_the_slowest_move_of_all() {
    let _g = common::isolate("ladder");
    let mut l = Ladder::new(cfg());
    let away = Machine::desktop().idle_ms(300_000);
    l.step(&away.clone().at(0).build());
    assert_eq!(l.tier(), Tier::Full, "T0 is not free");

    // Halfway through the minute: still not.
    l.step(&away.clone().at(30_000).build());
    assert_eq!(l.tier(), Tier::Full);

    let c = l.step(&away.clone().at(60_000).build()).expect("feral eventually");
    assert_eq!(c.to, Tier::Feral);
}

#[test]
fn a_vr_session_forces_t3_and_gives_it_back_afterwards() {
    let _g = common::isolate("ladder");
    let mut l = Ladder::new(cfg());
    let base = Machine::desktop();

    // Sitting at the desk.
    settle(&mut l, base.clone(), 0, 1_000);
    assert_eq!(l.tier(), Tier::Full);

    // WiVRn starts. Instant, in one sample, with the right words.
    let down = l
        .step(&base.clone().at(5_000).wivrn_streaming().build())
        .expect("VR must land immediately");
    assert_eq!(down.to, Tier::Lobotomised);
    assert_eq!(down.reason, TierReason::VrSession);
    assert_eq!(l.explanation(), "T3 because WiVRn is streaming");

    // A whole session goes by. Nothing changes; no events are emitted.
    for i in 6..=600u64 {
        assert!(l.step(&base.clone().at(i * 1_000).wivrn_streaming().build()).is_none());
    }
    assert_eq!(l.tier(), Tier::Lobotomised);

    // Headset off, server exits.
    for i in 601..=620u64 {
        l.step(&base.clone().at(i * 1_000).no_vr().build());
    }
    assert_eq!(l.tier(), Tier::Full);
    assert_eq!(
        l.explanation(),
        "T1 because you are at the desk and nothing heavy is running"
    );
}

#[test]
fn a_reason_change_at_the_same_tier_updates_the_readout_but_emits_nothing() {
    let _g = common::isolate("ladder");
    let mut l = Ladder::new(cfg());
    let base = Machine::desktop();
    l.step(&base.clone().at(0).fullscreen("gamescope").build());
    assert_eq!(l.tier(), Tier::Lobotomised);

    // Fullscreen game becomes a VR session. Same tier, different truth.
    assert!(l
        .step(&base.clone().at(1_000).windowed().wivrn_streaming().build())
        .is_none());
    assert_eq!(l.tier(), Tier::Lobotomised);
    assert_eq!(l.explanation(), "T3 because WiVRn is streaming");
    assert_eq!(l.reason(), &TierReason::VrSession);
}

// ---------------------------------------------------------------------------
// Pinning
// ---------------------------------------------------------------------------

#[test]
fn pinning_beats_every_probe_including_a_vr_session() {
    let _g = common::isolate("ladder");
    let mut l = Ladder::new(cfg());
    let base = Machine::desktop();

    let c = l.pin(Tier::Feral, 0).expect("pin applies immediately");
    assert_eq!(c.to, Tier::Feral);
    assert_eq!(c.reason, TierReason::Pinned);
    assert_eq!(l.explanation(), "T0 because you pinned her there");

    // WiVRn, a game, a fullscreen window and a hot GPU: the pin wins.
    for i in 1..=20u64 {
        let m = base
            .clone()
            .at(i * 1_000)
            .wivrn_streaming()
            .game("Cyberpunk2077.exe")
            .fullscreen("gamescope")
            .gpu_busy(99);
        assert!(l.step(&m.build()).is_none());
    }
    assert_eq!(l.tier(), Tier::Feral);
    assert_eq!(l.pinned(), Some(Tier::Feral));
}

#[test]
fn unpinning_hands_control_straight_back_to_the_machine() {
    let _g = common::isolate("ladder");
    let mut l = Ladder::new(cfg());
    let base = Machine::desktop();
    l.pin(Tier::Feral, 0);
    l.unpin();
    assert_eq!(l.pinned(), None);

    // The machine has a headset on it, so the correction is a downgrade and is
    // therefore instant.
    let c = l
        .step(&base.clone().at(1_000).wivrn_streaming().build())
        .expect("instant");
    assert_eq!((c.from, c.to), (Tier::Feral, Tier::Lobotomised));
}

#[test]
fn pinning_downwards_is_instant_too() {
    let _g = common::isolate("ladder");
    let mut l = Ladder::new(cfg());
    let c = l.pin(Tier::Dormant, 0).expect("silence her now");
    assert_eq!(c.to, Tier::Dormant);
    assert_eq!(l.explanation(), "T4 because you pinned her there");
}

// ---------------------------------------------------------------------------
// A whole afternoon
// ---------------------------------------------------------------------------

#[test]
fn a_realistic_trajectory() {
    let _g = common::isolate("ladder");
    let mut l = Ladder::new(cfg());
    let base = Machine::desktop();
    let mut seen: Vec<(u64, Tier)> = Vec::new();
    let push = |l: &Ladder, c: Option<wisp_gov::ladder::TierChange>, seen: &mut Vec<_>| {
        let _ = l;
        if let Some(c) = c {
            seen.push((c.at, c.to));
        }
    };

    // 0–20s: at the desk.
    for i in 0..20u64 {
        let c = l.step(&base.clone().at(i * 1_000).build());
        push(&l, c, &mut seen);
    }
    // 20–60s: a big compile.
    for i in 20..60u64 {
        let c = l.step(&base.clone().at(i * 1_000).load(28.0).top_cpu("cc1plus", 3100).build());
        push(&l, c, &mut seen);
    }
    // 60–90s: compile done, back to quiet.
    for i in 60..90u64 {
        let c = l.step(&base.clone().at(i * 1_000).build());
        push(&l, c, &mut seen);
    }
    // 90–200s: VR.
    for i in 90..200u64 {
        let c = l.step(&base.clone().at(i * 1_000).wivrn_streaming().build());
        push(&l, c, &mut seen);
    }
    // 200–260s: headset off.
    for i in 200..260u64 {
        let c = l.step(&base.clone().at(i * 1_000).build());
        push(&l, c, &mut seen);
    }
    // 260–400s: walked away.
    for i in 260..400u64 {
        let c = l.step(&base.clone().at(i * 1_000).idle_ms(300_000).build());
        push(&l, c, &mut seen);
    }

    let tiers: Vec<Tier> = seen.iter().map(|(_, t)| *t).collect();
    assert_eq!(
        tiers,
        vec![
            Tier::Reduced,     // compile started, instantly
            Tier::Full,        // compile done, after the dwell
            Tier::Lobotomised, // WiVRn, instantly
            Tier::Full,        // headset off, after the dwell
            Tier::Feral,       // away for a minute
        ],
        "trajectory was {seen:?}"
    );

    // The instant ones must really be instant.
    assert_eq!(seen[0].0, 20_000, "the compile downgrade was late");
    assert_eq!(seen[2].0, 90_000, "the VR downgrade was late");
    // The lazy ones must really be lazy.
    assert_eq!(seen[1].0, 60_000 + cfg().dwell.to_full_ms);
    assert_eq!(seen[3].0, 200_000 + cfg().dwell.to_full_ms);
}
