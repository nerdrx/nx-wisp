//! F44 against a **mock bus**: a real WebSocket server speaking the real
//! `docs/connector/PROTOCOL.md`, over an in-memory pipe. Nothing here needs NX
//! Hub to be installed, running, or even to exist.
//!
//! The rule under test is the one that bit this suite before: the bus drops
//! status faster than 4/s *silently*, so she must pre-throttle to ≤1/s and send
//! only what changed — or she will report stale state about other apps forever.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use tokio::time::Instant;

use wisp_fleet::connector::{self, BusEvent, ConnectorConfig, ConnectorHandle, Dialer};
use wisp_fleet::hop::{Carry, HopMessage, HopTransport, RelayTransport};
use wisp_fleet::hub::TokenSource;
use wisp_fleet::{fields, Fields};

const TOKEN: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";

/// SPEC §4: every test gets its own config dir. The dev build and the installed
/// copy otherwise share state, which has bitten this suite before.
fn isolate() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("NX_WISP_CONFIG_DIR", dir.path());
    std::env::set_var("NX_HUB_DATA_DIR", dir.path());
    dir
}

#[derive(Default)]
struct Log {
    dials: usize,
    hellos: Vec<Value>,
    /// Every `status` the bus actually accepted, with the (virtual) time it
    /// landed — which is what makes the rate assertions meaningful.
    statuses: Vec<(Instant, Value)>,
    others: Vec<Value>,
    byes: usize,
}

/// A mock NX Hub: real handshake, real framing, real message flow.
#[derive(Clone)]
struct MockHub {
    log: Arc<Mutex<Log>>,
    up: Arc<AtomicBool>,
    reject_token: Arc<AtomicBool>,
    generation: Arc<AtomicUsize>,
    /// Frames a test wants the hub to push at the client unprompted.
    pending_push: Arc<Mutex<Vec<Value>>>,
}

impl MockHub {
    fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(Log::default())),
            up: Arc::new(AtomicBool::new(true)),
            reject_token: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicUsize::new(0)),
            pending_push: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn set_up(&self, up: bool) {
        self.up.store(up, Ordering::SeqCst);
        if !up {
            self.kick();
        }
    }

    /// Drop whatever session is open, like a hub restarting.
    fn kick(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn dials(&self) -> usize {
        self.log.lock().unwrap().dials
    }
    fn hellos(&self) -> Vec<Value> {
        self.log.lock().unwrap().hellos.clone()
    }
    fn statuses(&self) -> Vec<(Instant, Value)> {
        self.log.lock().unwrap().statuses.clone()
    }
    fn merged(&self) -> Value {
        // The hub merges per key (PROTOCOL.md §4); so does this.
        let mut merged = serde_json::Map::new();
        for (_, s) in self.statuses() {
            if let Some(obj) = s.get("fields").and_then(Value::as_object) {
                for (k, v) in obj {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
        Value::Object(merged)
    }

    fn dialer(&self) -> MockDialer {
        MockDialer { hub: self.clone() }
    }
}

struct MockDialer {
    hub: MockHub,
}

impl Dialer for MockDialer {
    type Stream = DuplexStream;

    async fn dial(&self) -> std::io::Result<Self::Stream> {
        self.hub.log.lock().unwrap().dials += 1;
        if !self.hub.up.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "no hub"));
        }
        let (client, server) = tokio::io::duplex(64 * 1024);
        let hub = self.hub.clone();
        let generation = hub.generation.load(Ordering::SeqCst);
        tokio::spawn(async move {
            let _ = serve(server, hub, generation).await;
        });
        Ok(client)
    }
}

async fn serve(mut stream: DuplexStream, hub: MockHub, generation: usize) -> std::io::Result<()> {
    use wisp_fleet::ws::{server_handshake, Side, WsMessage, WsReader, WsWriter};

    let leftover = match server_handshake(&mut stream).await {
        Ok(rest) => rest,
        Err(_) => return Ok(()),
    };
    let (rd, wr) = tokio::io::split(stream);
    let mut reader = WsReader::new(rd, leftover, Side::Server);
    let mut writer = WsWriter::new(wr, Side::Server);
    let mut said_hello = false;

    loop {
        // A generation bump means "the hub went away".
        if hub.generation.load(Ordering::SeqCst) != generation {
            let _ = writer.send_close(1000).await;
            return Ok(());
        }
        if said_hello {
            let queued: Vec<Value> = std::mem::take(&mut hub.pending_push.lock().unwrap());
            for frame in queued {
                let _ = writer.send_text(&frame.to_string()).await;
            }
        }
        let next = tokio::time::timeout(Duration::from_millis(250), reader.next()).await;
        let msg = match next {
            Ok(Ok(m)) => m,
            Ok(Err(_)) => return Ok(()),
            Err(_) => continue, // idle: re-check the generation
        };
        let WsMessage::Text(text) = msg else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "hello" => {
                hub.log.lock().unwrap().hellos.push(value.clone());
                let token = value.get("token").and_then(Value::as_str).unwrap_or("");
                if hub.reject_token.load(Ordering::SeqCst) || token != TOKEN {
                    let _ = writer
                        .send_text(&json!({"type":"error","message":"unauthorized"}).to_string())
                        .await;
                    let _ = writer.send_close(1008).await;
                    return Ok(());
                }
                said_hello = true;
                let _ = writer.send_text(&json!({"type":"welcome","hub":"0.10.0"}).to_string()).await;
            }
            "status" if said_hello => {
                hub.log.lock().unwrap().statuses.push((Instant::now(), value));
            }
            "pong" => {}
            "bye" => {
                hub.log.lock().unwrap().byes += 1;
                return Ok(());
            }
            other => {
                hub.log.lock().unwrap().others.push(value.clone());
                // Exactly what the real hub does with a verb it does not know:
                // complain, but keep the presence slot (PROTOCOL.md §6).
                let _ = writer
                    .send_text(
                        &json!({"type":"error","message":format!("unknown type: {other}")})
                            .to_string(),
                    )
                    .await;
            }
        }
    }
}

fn config(token: TokenSource) -> ConnectorConfig {
    ConnectorConfig {
        app: "nx-wisp".into(),
        version: Some("0.1.0".into()),
        token,
        ..ConnectorConfig::default()
    }
}

async fn start(hub: &MockHub) -> (ConnectorHandle, mpsc::UnboundedReceiver<BusEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = connector::spawn(config(TokenSource::Fixed(TOKEN.into())), hub.dialer(), tx);
    (handle, rx)
}

/// Wait for a specific event, or give up after some virtual time.
async fn expect(rx: &mut mpsc::UnboundedReceiver<BusEvent>, want: fn(&BusEvent) -> bool) -> BusEvent {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(e)) if want(&e) => return e,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("the connector task died"),
            Err(_) => panic!("timed out waiting for an event"),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn she_says_hello_with_the_token_and_lands_on_the_bus() {
    let _dir = isolate();
    let hub = MockHub::new();
    let (handle, mut rx) = start(&hub).await;

    let event = expect(&mut rx, |e| matches!(e, BusEvent::Connected { .. })).await;
    assert_eq!(event, BusEvent::Connected { hub: "0.10.0".into() });
    assert!(handle.connected());

    let hellos = hub.hellos();
    assert_eq!(hellos.len(), 1);
    assert_eq!(hellos[0]["app"], "nx-wisp");
    assert_eq!(hellos[0]["token"], TOKEN);
    assert!(hellos[0]["caps"].as_array().unwrap().iter().any(|c| c == "status"));
    assert!(hellos[0]["pid"].is_number());
}

#[tokio::test(start_paused = true)]
async fn a_burst_never_exceeds_one_status_a_second_and_the_newest_value_wins() {
    let _dir = isolate();
    let hub = MockHub::new();
    let (handle, mut rx) = start(&hub).await;
    expect(&mut rx, |e| matches!(e, BusEvent::Connected { .. })).await;

    // Twenty updates as fast as the machine can produce them — the shape of a
    // subsystem that publishes on every frame.
    for hr in 60..80u32 {
        handle.publish(fields! { "hr" => hr, "listening" => true });
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let statuses = hub.statuses();
    assert!(!statuses.is_empty(), "the first update goes out immediately");
    assert!(
        statuses.len() <= 2,
        "a burst must coalesce, got {} messages: {:?}",
        statuses.len(),
        statuses.iter().map(|(_, s)| s.clone()).collect::<Vec<_>>()
    );
    for pair in statuses.windows(2) {
        let gap = pair[1].0.duration_since(pair[0].0);
        assert!(gap >= Duration::from_millis(1000), "two statuses only {gap:?} apart");
    }
    // Whatever survived, the hub's merged view is the newest state.
    assert_eq!(hub.merged()["hr"], json!(79));

    // Now a long, steady stream: still ≤1/s, and never a dropped final value.
    for hr in 80..140u32 {
        handle.publish(fields! { "hr" => hr });
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let statuses = hub.statuses();
    for pair in statuses.windows(2) {
        assert!(pair[1].0.duration_since(pair[0].0) >= Duration::from_millis(1000));
    }
    assert!(statuses.len() <= 9, "60 samples over 6 s became {} messages", statuses.len());
    assert_eq!(hub.merged()["hr"], json!(139), "the terminal value always lands");
}

#[tokio::test(start_paused = true)]
async fn an_unchanged_status_is_never_sent_twice() {
    let _dir = isolate();
    let hub = MockHub::new();
    let (handle, mut rx) = start(&hub).await;
    expect(&mut rx, |e| matches!(e, BusEvent::Connected { .. })).await;

    let same: Fields = fields! { "tier" => "T1", "listening" => false };
    for _ in 0..10 {
        handle.publish(same.clone());
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    assert_eq!(hub.statuses().len(), 1, "change-only means exactly one message");

    // …and a single changed key travels alone.
    handle.publish(fields! { "tier" => "T3", "listening" => false });
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let last = hub.statuses().last().unwrap().1.clone();
    assert_eq!(last["fields"], json!({"tier": "T3"}));
}

#[tokio::test(start_paused = true)]
async fn a_terminal_update_survives_a_burst_in_front_of_it() {
    let _dir = isolate();
    let hub = MockHub::new();
    let (handle, mut rx) = start(&hub).await;
    expect(&mut rx, |e| matches!(e, BusEvent::Connected { .. })).await;

    // The exact shape that lost `connected:false` in PulseNX: noise, then one
    // one-shot fact that nothing will ever restate.
    for i in 0..50u32 {
        handle.publish(fields! { "hr" => 60 + i });
    }
    handle.publish(fields! { "hr" => wisp_fleet::Field::Null, "connected" => false });
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert_eq!(hub.merged()["connected"], json!(false), "the terminal update must land");
}

#[tokio::test(start_paused = true)]
async fn a_hub_restart_is_survived_and_the_whole_status_is_restated() {
    let _dir = isolate();
    let hub = MockHub::new();
    let (handle, mut rx) = start(&hub).await;
    expect(&mut rx, |e| matches!(e, BusEvent::Connected { .. })).await;

    handle.publish(fields! { "tier" => "T1", "mood" => "curious", "listening" => false });
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let before = hub.statuses().len();
    assert!(before >= 1);

    hub.kick();
    expect(&mut rx, |e| matches!(e, BusEvent::Disconnected)).await;
    assert!(!handle.connected());

    expect(&mut rx, |e| matches!(e, BusEvent::Connected { .. })).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;

    assert_eq!(hub.hellos().len(), 2, "a second hello after the restart");
    let after: Vec<Value> = hub.statuses()[before..].iter().map(|(_, s)| s.clone()).collect();
    let restated = after
        .iter()
        .find(|s| s["fields"]["tier"] == json!("T1"))
        .expect("the full status is restated on a fresh slot");
    // PROTOCOL.md §8: the hub's slot starts empty, so merge semantics have
    // nothing to merge onto — everything has to be said again.
    assert_eq!(restated["fields"]["mood"], json!("curious"));
    assert_eq!(restated["fields"]["listening"], json!(false));
}

#[tokio::test(start_paused = true)]
async fn no_hub_at_all_is_a_quiet_exponential_retry() {
    let _dir = isolate();
    let hub = MockHub::new();
    hub.set_up(false);
    let (handle, _rx) = start(&hub).await;

    handle.publish(fields! { "tier" => "T1" });
    tokio::time::sleep(Duration::from_secs(20)).await;
    let dials = hub.dials();
    // 1s, 2s, 4s, 8s, 16s… — a handful of attempts in twenty seconds, not a
    // spin. And absolutely nothing was said to the operator.
    assert!((3..=7).contains(&dials), "expected a backoff ladder, got {dials} dials");

    // The hub arrives late. She notices, and says everything she has.
    hub.set_up(true);
    tokio::time::sleep(Duration::from_secs(45)).await;
    assert!(handle.connected(), "she found the hub once it appeared");
    assert_eq!(hub.merged()["tier"], json!("T1"), "state buffered while down is not lost");
}

#[tokio::test(start_paused = true)]
async fn backoff_resets_after_a_connection_that_reached_welcome() {
    let _dir = isolate();
    let hub = MockHub::new();
    let (_handle, mut rx) = start(&hub).await;
    expect(&mut rx, |e| matches!(e, BusEvent::Connected { .. })).await;
    let after_first = hub.dials();

    for _ in 0..3 {
        hub.kick();
        expect(&mut rx, |e| matches!(e, BusEvent::Disconnected)).await;
        expect(&mut rx, |e| matches!(e, BusEvent::Connected { .. })).await;
    }
    // Each cycle costs one minimum-backoff wait, not a doubling one.
    assert_eq!(hub.dials(), after_first + 3);
}

#[tokio::test(start_paused = true)]
async fn no_token_means_no_hub_and_she_never_even_dials() {
    let dir = isolate();
    let hub = MockHub::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let missing = TokenSource::File(dir.path().join("connector.token"));
    let handle = connector::spawn(config(missing), hub.dialer(), tx);

    handle.publish(fields! { "tier" => "T1" });
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(hub.dials(), 0, "no token means no hub; do not knock on the port");
    assert!(rx.try_recv().is_err(), "and do not say a word about it");

    // The hub is installed while she is running: the token is re-read on every
    // attempt, so she joins without a restart.
    std::fs::write(dir.path().join("connector.token"), format!("{TOKEN}\n")).unwrap();
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(handle.connected());
}

#[tokio::test(start_paused = true)]
async fn a_refused_token_is_retried_quietly_rather_than_crashing() {
    let _dir = isolate();
    let hub = MockHub::new();
    hub.reject_token.store(true, Ordering::SeqCst);
    let (handle, mut rx) = start(&hub).await;

    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(!handle.connected());
    let saw_error = std::iter::from_fn(|| rx.try_recv().ok())
        .any(|e| matches!(e, BusEvent::HubError(m) if m == "unauthorized"));
    assert!(saw_error, "the refusal is recorded…");
    assert!(hub.dials() >= 2, "…and she keeps trying, quietly");

    // The hub is fixed (a re-pair, a fresh token): she recovers on her own.
    hub.reject_token.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(handle.connected());
}

#[tokio::test(start_paused = true)]
async fn a_hub_without_the_relay_verb_disables_the_hop_instead_of_breaking()
{
    let _dir = isolate();
    let hub = MockHub::new();
    let (handle, mut rx) = start(&hub).await;
    expect(&mut rx, |e| matches!(e, BusEvent::Connected { .. })).await;

    let transport = RelayTransport::new(handle.clone(), "nx-wisp");
    let msg = HopMessage::Claim { from: "aaaa".into(), seq: 1, epoch: 1, owner: "aaaa".into() };
    assert!(transport.send(&"bbbb".to_string(), &msg), "the send itself is queued");

    expect(&mut rx, |e| matches!(e, BusEvent::RelayUnsupported)).await;
    assert!(!handle.relay_supported());
    // Crucially: still on the bus. An old hub costs her the hop, nothing else.
    assert!(handle.connected());
    handle.publish(fields! { "tier" => "T1" });
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(hub.merged()["tier"], json!("T1"));
}

#[tokio::test(start_paused = true)]
async fn an_inbound_relay_becomes_a_bus_event() {
    // The hub side of the hop does not exist yet, so this proves the *client*
    // half: a `relay` frame from the hub is decoded and handed up.
    let _dir = isolate();
    let hub = MockHub::new();
    let (handle, mut rx) = start(&hub).await;
    expect(&mut rx, |e| matches!(e, BusEvent::Connected { .. })).await;

    // Stand up a bare session and push a relay at the client. (The mock hub
    // above never sends one unprompted; this is the shape it would take.)
    let body = serde_json::to_value(HopMessage::Hop {
        hop_id: "deadbeef".into(),
        from: "bbbb".into(),
        to: "aaaa".into(),
        seq: 9,
        epoch: 2,
        attempt: 1,
        carry: Carry { mood: "restless".into(), ..Carry::default() },
    })
    .unwrap();
    let framed = json!({"type":"relay","peer":"bbbb","app":"nx-wisp","body":body});
    push_from_hub(&hub, framed).await;

    let event = expect(&mut rx, |e| matches!(e, BusEvent::Relay { .. })).await;
    let BusEvent::Relay { peer, app, body } = event else { unreachable!() };
    assert_eq!(peer, "bbbb");
    assert_eq!(app, "nx-wisp");
    let decoded: HopMessage = serde_json::from_value(body).unwrap();
    assert!(matches!(decoded, HopMessage::Hop { epoch: 2, .. }));
    assert!(handle.relay_supported(), "a hub that speaks relay is remembered as one");
}

/// Queue a frame for the mock hub to push at the client on its next tick.
async fn push_from_hub(hub: &MockHub, message: Value) {
    hub.pending_push.lock().unwrap().push(message);
    tokio::time::sleep(Duration::from_millis(400)).await;
}
