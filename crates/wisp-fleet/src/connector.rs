//! F44 — the NX Connector client.
//!
//! She appears on the bus like any other NX app: `hello` with the token from
//! the hub's data dir, then a small live status, answering the hub's pings
//! until one of us goes away.
//!
//! Three rules from `docs/connector/PROTOCOL.md` are load-bearing and are
//! implemented here rather than left to the caller:
//!
//! 1. **≤ 1 status per second, change-only.** The bus takes four and drops the
//!    rest *silently*; a lost terminal update is how an app ends up reported as
//!    connected forever. See [`crate::status`].
//! 2. **Re-read the token on every attempt.** She may legitimately start before
//!    NX Hub has ever run.
//! 3. **Be silent about it.** No hub is the normal case. Everything here logs
//!    at `debug`/`trace`, never at `warn`, and nothing reaches the operator.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::hub::{TokenSource, CONNECTOR_HOST, CONNECTOR_PORT};
use crate::status::{self, Decision, Fields, Throttle};
use crate::ws::{self, Side, WsMessage, WsReader, WsWriter};

/// `caps` we announce in `hello`. `status` is the bus's own; `relay` is the
/// fleet-hop capability this crate needs from the hub (see [`crate::hop`]) and
/// is harmless to announce to a hub that has never heard of it.
pub const CAP_STATUS: &str = "status";
pub const CAP_RELAY: &str = "relay";

#[derive(Debug, Clone)]
pub struct ConnectorConfig {
    /// Her app id on the bus. The hub lowercases it.
    pub app: String,
    pub version: Option<String>,
    pub caps: Vec<String>,
    pub token: TokenSource,
    pub host: String,
    pub port: u16,
    pub resource: String,
    pub min_status_interval_ms: u64,
    pub min_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl Default for ConnectorConfig {
    fn default() -> Self {
        Self {
            app: "nx-wisp".into(),
            version: option_env!("CARGO_PKG_VERSION").map(str::to_string),
            caps: vec![CAP_STATUS.into(), CAP_RELAY.into()],
            token: TokenSource::default(),
            host: CONNECTOR_HOST.into(),
            port: CONNECTOR_PORT,
            resource: "/".into(),
            min_status_interval_ms: status::MIN_STATUS_INTERVAL_MS,
            min_backoff_ms: 1000,
            max_backoff_ms: 30_000,
        }
    }
}

/// What the bus told us. These are facts about the past — the binary turns them
/// into `wisp_proto::Event`s; this crate never speaks to the operator directly.
#[derive(Debug, Clone, PartialEq)]
pub enum BusEvent {
    Connected { hub: String },
    Disconnected,
    /// The hub is stopping a stack we are part of: exit cleanly (PROTOCOL §7).
    ShutdownRequest,
    /// The hub complained about something we sent.
    HubError(String),
    /// An `app-relay` from a peer machine arrived (see [`crate::hop`]).
    Relay { peer: String, app: String, body: Value },
    /// This hub does not implement the relay verb — the fleet hop is
    /// unavailable until NX Hub grows it. Emitted at most once per connection.
    RelayUnsupported,
}

#[derive(Debug)]
pub(crate) enum Cmd {
    Publish(Fields),
    Relay { peer: String, app: String, body: Value },
    Close,
}

/// Handle to the running client. Cloneable, and every method is non-blocking so
/// it is safe to call from `Governed::set_tier`.
#[derive(Debug, Clone)]
pub struct ConnectorHandle {
    tx: mpsc::UnboundedSender<Cmd>,
    connected: Arc<AtomicBool>,
    relay_ok: Arc<AtomicBool>,
}

impl ConnectorHandle {
    /// Set the status the hub should hold for us. Coalesced and throttled: the
    /// caller may call this as often as it likes, including several times in
    /// one millisecond, and exactly one message per second reaches the bus
    /// carrying the newest values.
    pub fn publish(&self, fields: Fields) {
        let _ = self.tx.send(Cmd::Publish(fields));
    }

    /// Are we on the bus right now?
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Does this hub know how to relay app messages to a paired machine?
    /// False until proven otherwise, and false forever on an older hub.
    pub fn relay_supported(&self) -> bool {
        self.relay_ok.load(Ordering::Relaxed)
    }

    pub(crate) fn send_relay(&self, peer: String, app: String, body: Value) -> bool {
        self.tx.send(Cmd::Relay { peer, app, body }).is_ok()
    }

    /// Say `bye` and stop reconnecting.
    pub fn close(&self) {
        let _ = self.tx.send(Cmd::Close);
    }
}

/// How to reach the bus. Production dials TCP; the tests hand back one end of
/// an in-memory pipe, which is what makes the suite independent of NX Hub.
pub trait Dialer: Send + Sync + 'static {
    type Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static;
    fn dial(&self) -> impl std::future::Future<Output = std::io::Result<Self::Stream>> + Send;
}

pub struct TcpDialer {
    pub host: String,
    pub port: u16,
}

impl Dialer for TcpDialer {
    type Stream = tokio::net::TcpStream;
    async fn dial(&self) -> std::io::Result<Self::Stream> {
        let stream = tokio::net::TcpStream::connect((self.host.as_str(), self.port)).await?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }
}

/// Monotonic milliseconds since this client started. Never wall clock: a
/// suspend/resume must not reorder the throttle.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clock {
    start: Instant,
}

impl Clock {
    pub(crate) fn new() -> Self {
        Self { start: Instant::now() }
    }
    pub(crate) fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

/// Spawn the client. Returns immediately; the task retries forever, quietly.
pub fn spawn<D: Dialer>(
    cfg: ConnectorConfig,
    dialer: D,
    events: mpsc::UnboundedSender<BusEvent>,
) -> ConnectorHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let connected = Arc::new(AtomicBool::new(false));
    let relay_ok = Arc::new(AtomicBool::new(false));
    let handle =
        ConnectorHandle { tx, connected: Arc::clone(&connected), relay_ok: Arc::clone(&relay_ok) };
    tokio::spawn(run(cfg, dialer, rx, events, connected, relay_ok));
    handle
}

async fn run<D: Dialer>(
    cfg: ConnectorConfig,
    dialer: D,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    events: mpsc::UnboundedSender<BusEvent>,
    connected: Arc<AtomicBool>,
    relay_ok: Arc<AtomicBool>,
) {
    let clock = Clock::new();
    let mut throttle = Throttle::new(cfg.min_status_interval_ms);
    let mut pending = Fields::new();
    let mut backoff = cfg.min_backoff_ms;

    loop {
        // Drain any commands that queued while we were down. Status is a live
        // gauge, never a backlog: only the newest value survives (PROTOCOL §8).
        while let Ok(cmd) = cmds.try_recv() {
            match cmd {
                Cmd::Publish(f) => merge(&mut pending, f),
                Cmd::Relay { .. } => { /* dropped: no session to relay over */ }
                Cmd::Close => return,
            }
        }

        let Some(token) = cfg.token.read() else {
            // No token == no hub. Perfectly normal; say nothing.
            tracing::trace!(path = %cfg.token.describe(), "no connector token yet");
            if !sleep_or_close(&mut cmds, &mut pending, backoff).await {
                return;
            }
            backoff = (backoff * 2).min(cfg.max_backoff_ms);
            continue;
        };

        match dialer.dial().await {
            Ok(stream) => {
                throttle.reset();
                let live = session(
                    &cfg,
                    &token,
                    stream,
                    &mut cmds,
                    &events,
                    &mut throttle,
                    &mut pending,
                    &clock,
                    &connected,
                    &relay_ok,
                )
                .await;
                connected.store(false, Ordering::Relaxed);
                relay_ok.store(false, Ordering::Relaxed);
                match live {
                    SessionEnd::Closed => return,
                    SessionEnd::Reached => {
                        let _ = events.send(BusEvent::Disconnected);
                        // A connection that reached `welcome` resets the ladder.
                        backoff = cfg.min_backoff_ms;
                    }
                    SessionEnd::Failed => {}
                }
            }
            Err(e) => tracing::trace!(error = %e, "connector dial failed"),
        }

        if !sleep_or_close(&mut cmds, &mut pending, backoff).await {
            return;
        }
        backoff = (backoff * 2).min(cfg.max_backoff_ms);
    }
}

enum SessionEnd {
    /// We never reached `welcome`.
    Failed,
    /// We were on the bus and then were not.
    Reached,
    /// `close()` was called; stop retrying.
    Closed,
}

/// Wait out the backoff, while still accepting commands. Returns false if we
/// were told to stop.
async fn sleep_or_close(
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    pending: &mut Fields,
    ms: u64,
) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return true,
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Publish(f)) => merge(pending, f),
                Some(Cmd::Relay { .. }) => {}
                Some(Cmd::Close) | None => return false,
            },
        }
    }
}

fn merge(into: &mut Fields, from: Fields) {
    for (k, v) in from {
        into.insert(k, v);
    }
}

#[allow(clippy::too_many_arguments)]
async fn session<S>(
    cfg: &ConnectorConfig,
    token: &str,
    mut stream: S,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    events: &mpsc::UnboundedSender<BusEvent>,
    throttle: &mut Throttle,
    pending: &mut Fields,
    clock: &Clock,
    connected: &AtomicBool,
    relay_ok: &AtomicBool,
) -> SessionEnd
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let leftover = match ws::client_handshake(&mut stream, &cfg.host, cfg.port, &cfg.resource).await
    {
        Ok(rest) => rest,
        Err(e) => {
            tracing::debug!(error = %e, "connector handshake failed");
            return SessionEnd::Failed;
        }
    };
    let (rd, wr) = tokio::io::split(stream);
    let mut reader = WsReader::new(rd, leftover, Side::Client);
    let mut writer = WsWriter::new(wr, Side::Client);

    let hello = json!({
        "type": "hello",
        "app": cfg.app,
        "version": cfg.version,
        "pid": std::process::id(),
        "token": token,
        "caps": cfg.caps,
    });
    if writer.send_text(&hello.to_string()).await.is_err() {
        return SessionEnd::Failed;
    }

    let mut live = false;
    let mut relay_probe_pending = false;

    loop {
        // The flush deadline for a status that is waiting out the second.
        let flush_at = match throttle.decide(pending, clock.now_ms()) {
            Decision::Wait(ms) => Some(Instant::now() + Duration::from_millis(ms)),
            _ => None,
        };
        let deadline = flush_at.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

        tokio::select! {
            biased;

            incoming = reader.next() => match incoming {
                Ok(WsMessage::Text(text)) => {
                    match on_text(&text, events, &mut live, relay_ok, &mut relay_probe_pending) {
                        Reply::None => {}
                        Reply::Pong => {
                            if writer.send_text("{\"type\":\"pong\"}").await.is_err() {
                                return end(live);
                            }
                        }
                        Reply::Welcome => {
                            connected.store(true, Ordering::Relaxed);
                            // A fresh slot holds nothing, so restate everything.
                            throttle.reset();
                            if !flush(&mut writer, throttle, pending, clock).await {
                                return end(live);
                            }
                        }
                    }
                }
                Ok(WsMessage::Ping(p)) => {
                    if writer.send_pong(&p).await.is_err() {
                        return end(live);
                    }
                }
                Ok(WsMessage::Close(_)) | Err(_) => return end(live),
                Ok(_) => {}
            },

            cmd = cmds.recv() => match cmd {
                Some(Cmd::Publish(f)) => {
                    merge(pending, f);
                    if live && !flush(&mut writer, throttle, pending, clock).await {
                        return end(live);
                    }
                }
                Some(Cmd::Relay { peer, app, body }) => {
                    if live {
                        let msg = json!({"type": "relay", "peer": peer, "app": app, "body": body});
                        relay_probe_pending = true;
                        if writer.send_text(&msg.to_string()).await.is_err() {
                            return end(live);
                        }
                    }
                }
                Some(Cmd::Close) | None => {
                    if live {
                        let _ = writer.send_text("{\"type\":\"bye\"}").await;
                        let _ = writer.send_close(1000).await;
                    }
                    return SessionEnd::Closed;
                }
            },

            _ = tokio::time::sleep_until(deadline), if flush_at.is_some() => {
                if live && !flush(&mut writer, throttle, pending, clock).await {
                    return end(live);
                }
            }
        }
    }
}

fn end(live: bool) -> SessionEnd {
    if live {
        SessionEnd::Reached
    } else {
        SessionEnd::Failed
    }
}

enum Reply {
    None,
    Pong,
    Welcome,
}

fn on_text(
    text: &str,
    events: &mpsc::UnboundedSender<BusEvent>,
    live: &mut bool,
    relay_ok: &AtomicBool,
    relay_probe_pending: &mut bool,
) -> Reply {
    let Ok(msg) = serde_json::from_str::<Value>(text) else {
        return Reply::None;
    };
    match msg.get("type").and_then(Value::as_str).unwrap_or("") {
        "welcome" => {
            *live = true;
            let hub = msg.get("hub").and_then(Value::as_str).unwrap_or("").to_string();
            let _ = events.send(BusEvent::Connected { hub });
            Reply::Welcome
        }
        "ping" => Reply::Pong,
        "shutdown-request" => {
            let _ = events.send(BusEvent::ShutdownRequest);
            Reply::None
        }
        // The hub's answer to an `app-relay` from a peer machine. Not in the
        // frozen v0.5 protocol — see `crate::hop` for what we need from the hub.
        "relay" => {
            relay_ok.store(true, Ordering::Relaxed);
            *relay_probe_pending = false;
            let peer = msg.get("peer").and_then(Value::as_str).unwrap_or("").to_string();
            let app = msg.get("app").and_then(Value::as_str).unwrap_or("").to_string();
            let body = msg.get("body").cloned().unwrap_or(Value::Null);
            let _ = events.send(BusEvent::Relay { peer, app, body });
            Reply::None
        }
        "relay-ack" => {
            relay_ok.store(msg.get("ok").and_then(Value::as_bool).unwrap_or(false), Ordering::Relaxed);
            *relay_probe_pending = false;
            Reply::None
        }
        "error" => {
            let message = msg.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            // "unknown type: relay" is a *version gap*, not a fault: an older
            // hub keeps our presence slot and simply cannot federate her.
            if *relay_probe_pending && message.contains("unknown type") && message.contains("relay")
            {
                *relay_probe_pending = false;
                relay_ok.store(false, Ordering::Relaxed);
                let _ = events.send(BusEvent::RelayUnsupported);
            } else {
                let _ = events.send(BusEvent::HubError(message));
            }
            Reply::None
        }
        _ => Reply::None,
    }
}

/// Send whatever the throttle allows right now. False means the socket died.
async fn flush<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut WsWriter<W>,
    throttle: &mut Throttle,
    pending: &Fields,
    clock: &Clock,
) -> bool {
    let now = clock.now_ms();
    let Decision::Send(delta) = throttle.decide(pending, now) else {
        return true;
    };
    // Never send something the bus would answer with an error and a close: the
    // merged view is what it caps, so check the merge, not just the delta.
    let mut merged = throttle.mirror().clone();
    for (k, v) in &delta {
        merged.insert(k.clone(), v.clone());
    }
    if !status::fits(&merged) {
        tracing::debug!(keys = merged.len(), "status over the bus cap; dropping the update");
        return true;
    }
    let msg = json!({"type": "status", "fields": delta});
    if writer.send_text(&msg.to_string()).await.is_err() {
        return false;
    }
    throttle.on_sent(&delta, now);
    true
}
