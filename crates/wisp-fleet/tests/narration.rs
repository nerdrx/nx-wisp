//! F45 end to end: a snapshot file changes on disk, and she says the right
//! thing with the right urgency — or, at the wrong tier, says nothing at all.
//!
//! This drives the whole assembled subsystem ([`Fleet`]) rather than the
//! narrator alone, so it also covers the roster diff, the wiring, and the
//! governor's social half. No hub, no bus, no `nx`.

use std::time::Duration;

use wisp_fleet::{Fleet, FleetConfig, FleetEvent};
use wisp_proto::{Governed, Observation, Tier, TierReason, Urgency};

/// SPEC §4: her config dir and the hub's data dir both go somewhere temporary.
fn isolate() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("NX_WISP_CONFIG_DIR", dir.path());
    std::env::set_var("NX_HUB_DATA_DIR", dir.path());
    dir
}

fn config(dir: &std::path::Path) -> FleetConfig {
    FleetConfig {
        connector: wisp_fleet::ConnectorConfig {
            // No token file exists, so the client never dials anything. This
            // test must not touch a real hub on port 9021 if one is running.
            token: wisp_fleet::hub::TokenSource::File(dir.join("no-such-token")),
            ..Default::default()
        },
        snapshot: dir.join("connector-clients.json"),
        roster_poll: Duration::from_millis(200),
        rules_path: None,
        nx_binary: dir.join("no-such-nx"),
    }
}

/// The hub's snapshot format. `ts` is omitted deliberately: an unstamped
/// snapshot is treated as fresh, and the staleness rule has its own unit test.
fn write_snapshot(path: &std::path::Path, clients: serde_json::Value) {
    std::fs::write(path, serde_json::json!({ "clients": clients }).to_string()).unwrap();
}

async fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<FleetEvent>) -> Vec<FleetEvent> {
    // Let a couple of poll intervals go by in virtual time, then take
    // everything that landed.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

fn said(events: &[FleetEvent]) -> Vec<(String, Urgency)> {
    events
        .iter()
        .filter_map(|e| match e {
            FleetEvent::Says(u) => Some((u.text.clone(), u.urgency)),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_four_fleet_events_the_plan_names_all_land_with_the_right_urgency() {
    let dir = isolate();
    let cfg = config(dir.path());
    let snapshot = cfg.snapshot.clone();
    let (mut fleet, mut rx) = Fleet::spawn(cfg);

    // Nothing is running yet.
    write_snapshot(&snapshot, serde_json::json!([]));
    drain(&mut rx).await;

    // NX Sentry trips. This is the one thing in the whole rule set that is
    // allowed to break flow.
    write_snapshot(
        &snapshot,
        serde_json::json!([{"app":"nx-sentry","fields":{"armed":true,"tripped":false}}]),
    );
    drain(&mut rx).await;
    write_snapshot(
        &snapshot,
        serde_json::json!([{"app":"nx-sentry","fields":{"armed":true,"tripped":true}}]),
    );
    let events = drain(&mut rx).await;
    let lines = said(&events);
    assert!(
        lines.iter().any(|(text, u)| *u == Urgency::Alarm && text.contains("moved")),
        "expected an alarm about the watched region, got {lines:?}"
    );
    // …and the raw fact is published too, for the flight recorder.
    assert!(events.iter().any(|e| matches!(
        e,
        FleetEvent::Observed(Observation::Fleet { app, field, value })
            if app == "nx-sentry" && field == "tripped" && value == "true"
    )));

    // NX Hub has updates. Notable, and deferred a little — it is not urgent.
    // The hub is the bus's *server*, so this one never arrives as a client
    // status: somebody who knows hands it in (see `Fleet::observe`).
    fleet.observe(Observation::Fleet {
        app: "nx-hub".into(),
        field: "updates".into(),
        value: "3".into(),
    });
    let events = drain(&mut rx).await;
    let (text, urgency) = said(&events).into_iter().next().expect("she mentions updates");
    assert_eq!(urgency, Urgency::Notable);
    assert!(text.contains('3'), "{text}");

    // PulseNX: a resting rate says nothing, a spike is Notable — ambient
    // vitals, never an emergency.
    for hr in [61, 62, 63] {
        write_snapshot(
            &snapshot,
            serde_json::json!([{"app":"pulsenx","fields":{"hr":hr,"connected":true}}]),
        );
        let events = drain(&mut rx).await;
        assert!(said(&events).iter().all(|(_, u)| *u != Urgency::Alarm));
    }
    write_snapshot(
        &snapshot,
        serde_json::json!([{"app":"pulsenx","fields":{"hr":104,"connected":true}}]),
    );
    let events = drain(&mut rx).await;
    let lines = said(&events);
    assert!(
        lines.iter().any(|(t, u)| *u == Urgency::Notable && t.contains("heart rate")),
        "expected one remark about the spike, got {lines:?}"
    );

    // WiVRn starts: she waves goodbye. (The governor drops her to T3 on its
    // own; this is only the social half.)
    write_snapshot(
        &snapshot,
        serde_json::json!([{"app":"wivrn-nx","fields":{"session":true}}]),
    );
    let events = drain(&mut rx).await;
    let lines = said(&events);
    let goodbye = lines.iter().find(|(t, _)| t.contains("out of your way"));
    assert!(goodbye.is_some(), "expected a goodbye, got {lines:?}");
    assert_eq!(goodbye.unwrap().1, Urgency::Notable);
    assert!(events.iter().any(|e| matches!(
        e,
        FleetEvent::Says(u) if u.expression.as_deref() == Some("wave")
    )));

    fleet.set_tier(Tier::Lobotomised, &TierReason::VrSession);
    fleet.close();
}

#[tokio::test(start_paused = true)]
async fn at_t3_only_an_alarm_gets_through_and_at_t4_she_stops_looking() {
    let dir = isolate();
    let cfg = config(dir.path());
    let snapshot = cfg.snapshot.clone();
    let (mut fleet, mut rx) = Fleet::spawn(cfg);
    write_snapshot(&snapshot, serde_json::json!([]));
    drain(&mut rx).await;

    // A headset is up: she is out of the way.
    fleet.set_tier(Tier::Lobotomised, &TierReason::VrSession);
    write_snapshot(
        &snapshot,
        serde_json::json!([{"app":"pulsenx","fields":{"hr":60}},
                           {"app":"nx-hub","fields":{"updates":2}}]),
    );
    drain(&mut rx).await;
    write_snapshot(
        &snapshot,
        serde_json::json!([{"app":"pulsenx","fields":{"hr":130}},
                           {"app":"nx-hub","fields":{"updates":4}}]),
    );
    let events = drain(&mut rx).await;
    assert!(said(&events).is_empty(), "T3 says nothing short of an alarm");
    assert!(
        events.iter().any(|e| matches!(e, FleetEvent::Observed(_))),
        "…but she is still watching, so the recorder still gets the facts"
    );

    // Something moves on the watched screen. That still reaches her.
    write_snapshot(
        &snapshot,
        serde_json::json!([{"app":"nx-sentry","fields":{"tripped":true}}]),
    );
    let events = drain(&mut rx).await;
    assert_eq!(said(&events).len(), 1);
    assert_eq!(said(&events)[0].1, Urgency::Alarm);

    // T4 is silence: she stops reading the world at all.
    fleet.set_tier(Tier::Dormant, &TierReason::PowerCritical);
    write_snapshot(
        &snapshot,
        serde_json::json!([{"app":"nx-sentry","fields":{"tripped":false}},
                           {"app":"pulsenx","fields":{"hr":180}}]),
    );
    let events = drain(&mut rx).await;
    assert!(events.is_empty(), "dormant means dormant, got {events:?}");

    fleet.close();
}

#[tokio::test(start_paused = true)]
async fn with_no_hub_installed_she_simply_has_nothing_to_say() {
    let dir = isolate();
    let (fleet, mut rx) = Fleet::spawn(config(dir.path()));
    // No snapshot file, no token, no `nx`: the ordinary state of a machine
    // that does not run NX Hub.
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert!(rx.try_recv().is_err());
    assert!(!fleet.connector().connected());
    assert!(!fleet.tools().available());
    fleet.close();
}
