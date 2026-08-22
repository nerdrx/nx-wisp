//! F30 — the consent layer, and the only path by which an `Observation` may
//! reach the bus.
//!
//! SPEC §3.7: `Ambient` senses may run unprompted, `Explicit` require the
//! operator to have enabled them, `Invasive` additionally require the visible
//! tell of §0.3 for the whole time they are live.
//!
//! Everything in this module lives in one file on purpose. `SenseHandle` has no
//! public constructor and no `pub(crate)` back door: the *only* way to obtain
//! one is [`ConsentLedger::grant`], which refuses if consent is not satisfied.
//! That makes "a sense that runs without consent" unrepresentable, and
//! [`SenseHandle::publish`] rejects any `Observation` whose `SenseId` is not the
//! one the handle was granted for.

use std::collections::{BTreeMap, HashMap};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use wisp_proto::{Consent, Event, EventKind, Observation, SenseId};

use crate::clock::{utc_day, Clock};

/// Capacity of the one broadcast channel of SPEC §3.2. Senses are chatty during
/// a window drag; a slow subscriber lags rather than blocking a sense.
pub const BUS_CAPACITY: usize = 1024;

// ---------------------------------------------------------------------------
// The trait every sense implements
// ---------------------------------------------------------------------------

/// A sense declares, at the type level, exactly one `SenseId` — and therefore
/// exactly one `Consent` level, since `SenseId::consent()` is total.
///
/// The label and description are what the consent panel (F30) shows the
/// operator. "Plain-English description of what it can see" is a requirement of
/// the plan, not decoration.
pub trait Sense: Send + 'static {
    /// The one sense this type is allowed to speak for.
    const ID: SenseId;
    /// Short name for the consent panel row.
    const LABEL: &'static str;
    /// What this sense can actually see, in plain English.
    const DESCRIPTION: &'static str;

    /// The consent level, derived — never declared. A sense cannot talk its way
    /// into a cheaper permission than its `SenseId` carries.
    fn consent() -> Consent {
        Self::ID.consent()
    }
}

/// A sense that can be started by [`crate::Senses::start`].
pub trait SensePlugin: Sense + Sized {
    /// Run until the handle is dropped or `ctx.shutdown` fires. Errors are
    /// logged by the runner; a sense that dies must not take the app with it.
    fn spawn(self, handle: SenseHandle<Self>, ctx: SenseCtx) -> tokio::task::JoinHandle<()>;
}

/// Everything a running sense is given besides its handle: when to stop, and
/// how much of the machine it is currently allowed to cost.
#[derive(Debug, Clone)]
pub struct SenseCtx {
    pub shutdown: Shutdown,
    tier: tokio::sync::watch::Receiver<wisp_proto::Tier>,
}

impl SenseCtx {
    pub fn new(
        shutdown: Shutdown,
        tier: tokio::sync::watch::Receiver<wisp_proto::Tier>,
    ) -> Self {
        SenseCtx { shutdown, tier }
    }

    /// The tier right now. Cheap enough to read on every tick.
    pub fn tier(&self) -> wisp_proto::Tier {
        *self.tier.borrow()
    }

    /// Resolves the next time the governor moves her. `None` means the governor
    /// is gone, in which case a sense should keep running at its current tier
    /// rather than stopping.
    pub async fn tier_changed(&mut self) -> Option<wisp_proto::Tier> {
        self.tier.changed().await.ok()?;
        Some(*self.tier.borrow_and_update())
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConsentError {
    #[error("sense {0:?} requires the operator to enable it (consent: {1:?})")]
    NotEnabled(SenseId, Consent),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PublishError {
    /// The one that matters: a sense tried to speak for a `SenseId` it does not
    /// own. This cannot happen through honest code and is a bug if it does.
    #[error("sense {held:?} tried to publish an observation belonging to {attempted:?}")]
    WrongSense { held: SenseId, attempted: SenseId },
    /// The operator revoked consent while the sense was running.
    #[error("consent for {0:?} was revoked")]
    Revoked(SenseId),
}

// ---------------------------------------------------------------------------
// Persisted state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Persisted {
    /// Only senses the operator has explicitly decided about are stored.
    /// Anything absent falls back to the SPEC §3.7 default for its consent
    /// level, so a new sense never arrives silently switched on.
    #[serde(default)]
    enabled: BTreeMap<String, bool>,
    #[serde(default)]
    counters: BTreeMap<String, DayCounter>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
struct DayCounter {
    day: i64,
    count: u64,
}

impl DayCounter {
    fn bump(&mut self, today: i64) -> u64 {
        if self.day != today {
            self.day = today;
            self.count = 0;
        }
        self.count += 1;
        self.count
    }

    fn today(&self, today: i64) -> u64 {
        if self.day == today {
            self.count
        } else {
            0
        }
    }
}

/// `SenseId` has no `Display`, and its `Debug` is what we key the on-disk file
/// with. Keeping the mapping explicit means renaming a variant upstream is a
/// visible break rather than a silent reset of the operator's choices.
fn key(id: SenseId) -> &'static str {
    match id {
        SenseId::Idle => "idle",
        SenseId::ActiveWindow => "active_window",
        SenseId::WindowGeometry => "window_geometry",
        SenseId::Media => "media",
        SenseId::Audio => "audio",
        SenseId::Notifications => "notifications",
        SenseId::Vitals => "vitals",
        SenseId::Workspace => "workspace",
        SenseId::Clipboard => "clipboard",
        SenseId::Microphone => "microphone",
        SenseId::Screen => "screen",
        SenseId::Fleet => "fleet",
    }
}

/// Every sense the consent panel knows about, in display order.
pub const ALL_SENSES: [SenseId; 12] = [
    SenseId::ActiveWindow,
    SenseId::WindowGeometry,
    SenseId::Workspace,
    SenseId::Idle,
    SenseId::Media,
    SenseId::Audio,
    SenseId::Notifications,
    SenseId::Vitals,
    SenseId::Fleet,
    SenseId::Clipboard,
    SenseId::Microphone,
    SenseId::Screen,
];

/// SPEC §3.7: "ambient on, explicit off, invasive off".
pub fn ships_enabled(id: SenseId) -> bool {
    id.consent() == Consent::Ambient
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

struct LedgerInner {
    state: Persisted,
    /// How many live `SenseHandle`s exist per sense.
    ///
    /// A count rather than a flag because `wisp-proto` maps two observations
    /// onto one id: `Observation::Files` reports `SenseId::Vitals`, so the file
    /// watcher and the vitals poller legitimately hold a handle to the same
    /// sense at once. The visible tell of SPEC §0.3 goes up on 0→1 and comes
    /// down on 1→0, so it stays exact either way.
    granted: HashMap<SenseId, usize>,
    path: PathBuf,
}

impl LedgerInner {
    fn is_enabled(&self, id: SenseId) -> bool {
        self.state
            .enabled
            .get(key(id))
            .copied()
            .unwrap_or_else(|| ships_enabled(id))
    }

    fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(&self.state)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // Write-then-rename so a crash mid-write cannot leave the operator's
        // consent choices truncated (and therefore silently re-defaulted).
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &self.path)
    }
}

/// The consent panel's model and the gatekeeper for every sense.
#[derive(Clone)]
pub struct ConsentLedger {
    inner: Arc<Mutex<LedgerInner>>,
    bus: broadcast::Sender<Event>,
    clock: Clock,
}

impl std::fmt::Debug for ConsentLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.inner.lock().unwrap();
        f.debug_struct("ConsentLedger")
            .field("path", &g.path)
            .field("granted", &g.granted.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Where consent lives on disk. `NX_WISP_CONFIG_DIR` wins over XDG so tests can
/// never touch the operator's real state (SPEC §4).
pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("NX_WISP_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(d).join("nx-wisp");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("nx-wisp")
}

impl ConsentLedger {
    /// Load from `NX_WISP_CONFIG_DIR` (or XDG). A missing or corrupt file is not
    /// an error — it means "no decisions recorded", and the SPEC defaults apply.
    /// Failing open would be the wrong direction here: the defaults are the
    /// *safe* position, invasive senses off.
    pub fn load(bus: broadcast::Sender<Event>, clock: Clock) -> Self {
        Self::load_from(&config_dir(), bus, clock)
    }

    pub fn load_from(dir: &Path, bus: broadcast::Sender<Event>, clock: Clock) -> Self {
        let path = dir.join("senses.json");
        let state = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Persisted>(&b).ok())
            .unwrap_or_default();
        ConsentLedger {
            inner: Arc::new(Mutex::new(LedgerInner { state, granted: HashMap::new(), path })),
            bus,
            clock,
        }
    }

    pub fn is_enabled(&self, id: SenseId) -> bool {
        self.inner.lock().unwrap().is_enabled(id)
    }

    /// Operator flipped a row in the consent panel. Persists immediately.
    pub fn set_enabled(&self, id: SenseId, on: bool) -> std::io::Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.state.enabled.insert(key(id).to_string(), on);
        g.save()
    }

    /// "used N times today", for the consent panel.
    pub fn uses_today(&self, id: SenseId) -> u64 {
        let g = self.inner.lock().unwrap();
        g.state.counters.get(key(id)).copied().unwrap_or_default().today(utc_day())
    }

    /// One row per sense, for F30's UI.
    pub fn rows(&self) -> Vec<ConsentRow> {
        let g = self.inner.lock().unwrap();
        let today = utc_day();
        ALL_SENSES
            .iter()
            .map(|&id| ConsentRow {
                id,
                consent: id.consent(),
                label: label_of(id),
                description: description_of(id),
                enabled: g.is_enabled(id),
                live: g.granted.contains_key(&id),
                uses_today: g.state.counters.get(key(id)).copied().unwrap_or_default().today(today),
            })
            .collect()
    }

    /// Subscribe to the one bus of SPEC §3.2.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.bus.subscribe()
    }

    pub fn bus(&self) -> broadcast::Sender<Event> {
        self.bus.clone()
    }

    pub fn clock(&self) -> Clock {
        self.clock.clone()
    }

    /// The gate. Returns a handle only if `S`'s consent is satisfied.
    ///
    /// For an `Invasive` sense this also emits `EventKind::InvasiveActive
    /// { active: true }` — the visible tell of SPEC §0.3 goes up *before* the
    /// sense can see anything, and comes down in `SenseHandle::drop`.
    pub fn grant<S: Sense>(&self) -> Result<SenseHandle<S>, ConsentError> {
        let id = S::ID;
        let first = {
            let mut g = self.inner.lock().unwrap();
            if !g.is_enabled(id) {
                return Err(ConsentError::NotEnabled(id, id.consent()));
            }
            let n = g.granted.entry(id).or_insert(0);
            *n += 1;
            *n == 1
        };
        if first && id.consent() == Consent::Invasive {
            self.emit(EventKind::InvasiveActive { sense: id, active: true });
        }
        Ok(SenseHandle { ledger: self.clone(), _sense: PhantomData })
    }

    fn emit(&self, kind: EventKind) {
        // A send error only means nobody is listening yet. Losing the event is
        // correct; blocking a sense on an absent subscriber is not.
        let _ = self.bus.send(Event { at: self.clock.now(), kind });
    }

    fn release(&self, id: SenseId) {
        let last = {
            let mut g = self.inner.lock().unwrap();
            let n = g.granted.entry(id).or_insert(1);
            *n = n.saturating_sub(1);
            let last = *n == 0;
            if last {
                g.granted.remove(&id);
            }
            // Best effort: the counter is a UI nicety, not consent state.
            let _ = g.save();
            last
        };
        if last && id.consent() == Consent::Invasive {
            self.emit(EventKind::InvasiveActive { sense: id, active: false });
        }
    }

    fn publish_checked(&self, held: SenseId, obs: Observation) -> Result<(), PublishError> {
        let attempted = obs.sense();
        if attempted != held {
            return Err(PublishError::WrongSense { held, attempted });
        }
        {
            let mut g = self.inner.lock().unwrap();
            if !g.is_enabled(held) {
                return Err(PublishError::Revoked(held));
            }
            let today = utc_day();
            g.state.counters.entry(key(held).to_string()).or_default().bump(today);
        }
        self.emit(EventKind::Sensed(obs));
        Ok(())
    }
}

/// One row of the consent panel (F30).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentRow {
    pub id: SenseId,
    pub consent: Consent,
    pub label: &'static str,
    /// Plain English: what this sense can see.
    pub description: &'static str,
    pub enabled: bool,
    /// A handle is currently out — for invasive senses this is the visible tell.
    pub live: bool,
    pub uses_today: u64,
}

pub const fn label_of(id: SenseId) -> &'static str {
    match id {
        SenseId::Idle => "Idle",
        SenseId::ActiveWindow => "Active window",
        SenseId::WindowGeometry => "Window geometry",
        SenseId::Media => "Media",
        SenseId::Audio => "Audio",
        SenseId::Notifications => "Notifications",
        SenseId::Vitals => "Machine vitals",
        SenseId::Workspace => "Virtual desktops",
        SenseId::Clipboard => "Clipboard",
        SenseId::Microphone => "Microphone",
        SenseId::Screen => "Screen",
        SenseId::Fleet => "NX fleet",
    }
}

pub const fn description_of(id: SenseId) -> &'static str {
    match id {
        SenseId::Idle => "Whether you have stopped touching the mouse and keyboard, and for how long. Not what you typed.",
        SenseId::ActiveWindow => "The application and window title of whatever you are focused on.",
        SenseId::WindowGeometry => "Where your windows are on screen. She stands on them; she cannot see inside them.",
        SenseId::Media => "What your media players report they are playing: track, artist, and whether it is paused.",
        SenseId::Audio => "How loud your output is and whether a microphone is currently open. Never the sound itself.",
        SenseId::Notifications => "The desktop notifications that pop up: which app, the summary and the body text.",
        SenseId::Vitals => "CPU, GPU, VRAM, temperature and battery, plus changes in the project folders you listed.",
        SenseId::Workspace => "Which virtual desktop you switched to, and its name.",
        SenseId::Clipboard => "That you copied something, its size and its type. The contents are never stored or sent anywhere.",
        SenseId::Microphone => "Your microphone, transcribed locally. Her eyes stay open the whole time it is listening.",
        SenseId::Screen => "A region of your screen, looked at locally when you ask. Her eyes stay open the whole time.",
        SenseId::Fleet => "What your other NX apps announce on the local Connector bus.",
    }
}

// ---------------------------------------------------------------------------
// The handle
// ---------------------------------------------------------------------------

/// Proof that consent for `S` was satisfied, and the only way to publish.
///
/// There is no constructor. `ConsentLedger::grant` is the sole source, so a
/// sense cannot exist in a running state without its consent, and `publish`
/// refuses observations belonging to any other `SenseId`.
///
/// Deliberately **not** `Clone`: one live handle per sense means the invasive
/// tell in `Drop` is exact rather than approximately right.
pub struct SenseHandle<S: Sense> {
    ledger: ConsentLedger,
    _sense: PhantomData<fn() -> S>,
}

impl<S: Sense> SenseHandle<S> {
    pub fn id(&self) -> SenseId {
        S::ID
    }

    pub fn consent(&self) -> Consent {
        S::ID.consent()
    }

    /// The guarded publish path. Everything a sense sees goes through here.
    pub fn publish(&self, obs: Observation) -> Result<(), PublishError> {
        self.ledger.publish_checked(S::ID, obs)
    }

    /// Publish and log the refusal rather than propagating it. Most senses are
    /// in an event loop where there is nothing useful to do with the error.
    pub fn emit(&self, obs: Observation) {
        if let Err(e) = self.publish(obs) {
            tracing::warn!(sense = ?S::ID, error = %e, "observation refused");
        }
    }

    pub fn ledger(&self) -> &ConsentLedger {
        &self.ledger
    }

    pub fn clock(&self) -> Clock {
        self.ledger.clock()
    }

    /// Has the operator revoked us since we started?
    pub fn still_permitted(&self) -> bool {
        self.ledger.is_enabled(S::ID)
    }
}

impl<S: Sense> std::fmt::Debug for SenseHandle<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SenseHandle").field("id", &S::ID).finish()
    }
}

impl<S: Sense> Drop for SenseHandle<S> {
    fn drop(&mut self) {
        self.ledger.release(S::ID);
    }
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

/// Cooperative shutdown for the sense tasks. Cheap, clonable, and awaitable.
#[derive(Debug, Clone)]
pub struct Shutdown {
    rx: tokio::sync::watch::Receiver<bool>,
}

#[derive(Debug)]
pub struct ShutdownSignal {
    tx: tokio::sync::watch::Sender<bool>,
}

impl ShutdownSignal {
    pub fn new() -> (ShutdownSignal, Shutdown) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        (ShutdownSignal { tx }, Shutdown { rx })
    }
    pub fn fire(&self) {
        let _ = self.tx.send(true);
    }
}

impl Shutdown {
    pub fn is_down(&self) -> bool {
        *self.rx.borrow()
    }
    /// Resolves when shutdown is requested. Also resolves if the signal was
    /// dropped, so a lost owner cannot strand a sense task forever.
    pub async fn wait(&mut self) {
        loop {
            if *self.rx.borrow_and_update() {
                return;
            }
            if self.rx.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempConfig;

    struct FakeIdle;
    impl Sense for FakeIdle {
        const ID: SenseId = SenseId::Idle;
        const LABEL: &'static str = "Idle";
        const DESCRIPTION: &'static str = "test";
    }

    struct FakeClipboard;
    impl Sense for FakeClipboard {
        const ID: SenseId = SenseId::Clipboard;
        const LABEL: &'static str = "Clipboard";
        const DESCRIPTION: &'static str = "test";
    }

    fn ledger(dir: &Path) -> (ConsentLedger, broadcast::Receiver<Event>) {
        let (tx, rx) = broadcast::channel(64);
        (ConsentLedger::load_from(dir, tx, Clock::new()), rx)
    }

    #[test]
    fn defaults_are_ambient_on_invasive_off() {
        let tmp = TempConfig::new();
        let (l, _rx) = ledger(tmp.path());
        for id in ALL_SENSES {
            let want = id.consent() == Consent::Ambient;
            assert_eq!(l.is_enabled(id), want, "{id:?} default wrong");
        }
        assert!(!l.is_enabled(SenseId::Clipboard));
        assert!(!l.is_enabled(SenseId::Microphone));
        assert!(!l.is_enabled(SenseId::Screen));
    }

    #[test]
    fn invasive_sense_cannot_be_granted_until_enabled() {
        let tmp = TempConfig::new();
        let (l, _rx) = ledger(tmp.path());
        let err = l.grant::<FakeClipboard>().unwrap_err();
        assert_eq!(err, ConsentError::NotEnabled(SenseId::Clipboard, Consent::Invasive));
        l.set_enabled(SenseId::Clipboard, true).unwrap();
        assert!(l.grant::<FakeClipboard>().is_ok());
    }

    #[test]
    fn invasive_grant_and_drop_bracket_the_visible_tell() {
        let tmp = TempConfig::new();
        let (l, mut rx) = ledger(tmp.path());
        l.set_enabled(SenseId::Clipboard, true).unwrap();

        let h = l.grant::<FakeClipboard>().unwrap();
        let ev = rx.try_recv().unwrap();
        assert_eq!(
            ev.kind,
            EventKind::InvasiveActive { sense: SenseId::Clipboard, active: true }
        );
        assert!(l.rows().iter().find(|r| r.id == SenseId::Clipboard).unwrap().live);

        drop(h);
        let ev = rx.try_recv().unwrap();
        assert_eq!(
            ev.kind,
            EventKind::InvasiveActive { sense: SenseId::Clipboard, active: false }
        );
        assert!(!l.rows().iter().find(|r| r.id == SenseId::Clipboard).unwrap().live);
    }

    #[test]
    fn ambient_grant_emits_no_tell() {
        let tmp = TempConfig::new();
        let (l, mut rx) = ledger(tmp.path());
        let h = l.grant::<FakeIdle>().unwrap();
        drop(h);
        // Nothing at all should have been emitted for an ambient sense.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_sense_cannot_publish_another_senses_observation() {
        let tmp = TempConfig::new();
        let (l, _rx) = ledger(tmp.path());
        let h = l.grant::<FakeIdle>().unwrap();
        let err = h
            .publish(Observation::Clipboard { len: 12, kind: "text/plain".into() })
            .unwrap_err();
        assert_eq!(
            err,
            PublishError::WrongSense { held: SenseId::Idle, attempted: SenseId::Clipboard }
        );
        // And the legitimate one goes through.
        assert!(h.publish(Observation::Idle { idle: true, for_ms: 60_000 }).is_ok());
    }

    #[test]
    fn revoking_mid_run_stops_publication() {
        let tmp = TempConfig::new();
        let (l, _rx) = ledger(tmp.path());
        let h = l.grant::<FakeIdle>().unwrap();
        assert!(h.publish(Observation::Idle { idle: false, for_ms: 0 }).is_ok());
        l.set_enabled(SenseId::Idle, false).unwrap();
        assert!(!h.still_permitted());
        assert_eq!(
            h.publish(Observation::Idle { idle: false, for_ms: 0 }).unwrap_err(),
            PublishError::Revoked(SenseId::Idle)
        );
    }

    /// `Observation::Files` reports `SenseId::Vitals`, so two senses share one
    /// id. The tell must still be exact: up once, down once.
    #[test]
    fn handles_are_counted_so_the_invasive_tell_stays_exact() {
        let tmp = TempConfig::new();
        let (l, mut rx) = ledger(tmp.path());
        l.set_enabled(SenseId::Clipboard, true).unwrap();

        let a = l.grant::<FakeClipboard>().unwrap();
        let b = l.grant::<FakeClipboard>().unwrap();
        assert_eq!(
            rx.try_recv().unwrap().kind,
            EventKind::InvasiveActive { sense: SenseId::Clipboard, active: true }
        );
        assert!(rx.try_recv().is_err(), "the second handle must not raise the tell again");

        drop(a);
        assert!(rx.try_recv().is_err(), "one handle is still live; the tell stays up");
        assert!(l.rows().iter().find(|r| r.id == SenseId::Clipboard).unwrap().live);

        drop(b);
        assert_eq!(
            rx.try_recv().unwrap().kind,
            EventKind::InvasiveActive { sense: SenseId::Clipboard, active: false }
        );
        assert!(!l.rows().iter().find(|r| r.id == SenseId::Clipboard).unwrap().live);
    }

    #[test]
    fn counter_counts_and_persists() {
        let tmp = TempConfig::new();
        {
            let (l, _rx) = ledger(tmp.path());
            let h = l.grant::<FakeIdle>().unwrap();
            for _ in 0..3 {
                h.publish(Observation::Idle { idle: true, for_ms: 1 }).unwrap();
            }
            assert_eq!(l.uses_today(SenseId::Idle), 3);
            // Refused publications must not count as uses.
            let _ = h.publish(Observation::Clipboard { len: 1, kind: "x".into() });
            assert_eq!(l.uses_today(SenseId::Idle), 3);
        } // handle drop flushes
        let (l2, _rx) = ledger(tmp.path());
        assert_eq!(l2.uses_today(SenseId::Idle), 3);
    }

    #[test]
    fn enablement_persists_across_reload() {
        let tmp = TempConfig::new();
        {
            let (l, _rx) = ledger(tmp.path());
            l.set_enabled(SenseId::Clipboard, true).unwrap();
            l.set_enabled(SenseId::Media, false).unwrap();
        }
        let (l2, _rx) = ledger(tmp.path());
        assert!(l2.is_enabled(SenseId::Clipboard));
        assert!(!l2.is_enabled(SenseId::Media));
        assert!(l2.is_enabled(SenseId::Idle), "untouched ambient sense stays on");
    }

    #[test]
    fn corrupt_state_file_falls_back_to_safe_defaults() {
        let tmp = TempConfig::new();
        std::fs::write(tmp.path().join("senses.json"), b"{ this is not json").unwrap();
        let (l, _rx) = ledger(tmp.path());
        assert!(!l.is_enabled(SenseId::Clipboard), "must fail closed, not open");
        assert!(l.is_enabled(SenseId::Idle));
    }

    #[test]
    fn day_counter_rolls_over() {
        let mut c = DayCounter { day: 100, count: 7 };
        assert_eq!(c.today(100), 7);
        assert_eq!(c.today(101), 0);
        assert_eq!(c.bump(101), 1);
        assert_eq!(c.count, 1);
    }

    #[test]
    fn every_sense_has_a_panel_row_with_real_prose() {
        let tmp = TempConfig::new();
        let (l, _rx) = ledger(tmp.path());
        let rows = l.rows();
        assert_eq!(rows.len(), ALL_SENSES.len());
        for r in rows {
            assert!(r.description.len() > 30, "{:?} description is a stub", r.id);
            assert!(!r.label.is_empty());
        }
    }

    #[tokio::test]
    async fn shutdown_wakes_waiters() {
        let (sig, mut sd) = ShutdownSignal::new();
        assert!(!sd.is_down());
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            sig.fire();
        });
        sd.wait().await;
        assert!(sd.is_down());
    }

    #[tokio::test]
    async fn dropped_signal_also_wakes_waiters() {
        let (sig, mut sd) = ShutdownSignal::new();
        drop(sig);
        sd.wait().await; // must not hang
    }
}
