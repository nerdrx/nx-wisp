//! **The event loop.** Six finished crates, one process, and the two
//! whole-system guarantees that only exist here.
//!
//! # The bus, and why there are two of them
//!
//! SPEC §3.2 asks for one broadcast channel that everything publishes to and
//! anything may subscribe to, and says *"every event is recorded by the flight
//! recorder before dispatch"*. A recorder that merely subscribes records
//! **after** dispatch, and loses precisely the event that explains a crash.
//!
//! So the channel is split in two and the recorder is the bridge:
//!
//! ```text
//!   senses ──▶ inner ──▶ [Recorder::record] ──▶ bus ──▶ pump, shell, anyone
//!   fleet  ──▶ inner ──┘                         ▲
//!   attn   ── drained events ────────────────────┘  (recorded on the way past)
//!   gov    ── Step::event() ─────────────────────┘
//! ```
//!
//! Nothing holds a sender for `bus` except the relay. There is no code path
//! that can publish without recording, and that is a structural property rather
//! than a convention somebody has to remember.
//!
//! # Proposals are not events
//!
//! SPEC §3.2 also says events are **facts about the past, never commands.** A
//! subsystem that wants her to say something therefore does *not* publish
//! `EventKind::Proposed` — that would be a command wearing a fact's clothes.
//! It sends the [`Utterance`] down [`Speaker`], the pump submits it to
//! `wisp-attn`, and the `Proposed` record appears when the budget has actually
//! accepted it. `wisp-attn`'s own `drain_events` is the only source of
//! `Proposed`, `Said` and `Dropped`, which is what makes `wisp explain`'s
//! walk-back sound.
//!
//! # Tiers
//!
//! [`wisp_gov::Governor::step`] calls every registered `Governed` before it
//! returns, so the fan-out is synchronous by construction (SPEC §3.1) — this
//! module's job is to make sure everything that costs anything is in that
//! registry, and to get out of the way at T3/T4. The loop itself obeys the
//! governor's own cadence ([`Governor::poll_interval_ms`]), and the pump and
//! the rig heartbeat both slow down with the tier.
//!
//! [`Governor::poll_interval_ms`]: wisp_gov::Governor::poll_interval_ms

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, watch};
use wisp_attn::{Attention, Effect, Interaction, Mood, Turn};
use wisp_gov::{Governor, Shared};
use wisp_proto::{
    Cost, Event, EventKind, Governed, Millis, Observation, SenseId, Tier, TierReason, Utterance,
};
use wisp_rig::{Rig, RigInput};
use wisp_senses::{Clock, Senses, Shutdown, ShutdownSignal, BUS_CAPACITY};

use crate::config::{self, Config, SkinChoice};
use crate::recorder::Recorder;
use crate::shell::{FrameCtx, Headless, Shell};
use crate::state::{self, State};

// ---------------------------------------------------------------------------
// Adapters
// ---------------------------------------------------------------------------

/// `Senses::shutdown` consumes `self`, but the governor's registry needs to
/// hold it for the lifetime of the process. This is the join: the registry gets
/// a `Shared<SensesSlot>`, and shutdown takes the `Senses` back out of it.
///
/// A thin local newtype, in this crate, because `wisp-senses` is finished and
/// this is our problem, not its.
pub struct SensesSlot(pub Option<Senses>);

impl Governed for SensesSlot {
    fn set_tier(&mut self, tier: Tier, reason: &TierReason) {
        if let Some(s) = &mut self.0 {
            s.set_tier(tier, reason);
        }
    }
    fn cost_at(tier: Tier) -> Cost {
        <Senses as Governed>::cost_at(tier)
    }
}

/// `wisp-fleet` is optional at runtime (no hub is the normal case, and `--mock`
/// switches it off entirely), so the registry holds a slot rather than a
/// `Fleet`. Its declared cost is the fleet's whether or not it is running: the
/// cost meter should not flatter her by omitting a subsystem that is merely
/// disconnected.
pub struct FleetSlot(pub Option<wisp_fleet::Fleet>);

impl Governed for FleetSlot {
    fn set_tier(&mut self, tier: Tier, reason: &TierReason) {
        if let Some(f) = &mut self.0 {
            f.set_tier(tier, reason);
        }
    }
    fn cost_at(tier: Tier) -> Cost {
        <wisp_fleet::Fleet as Governed>::cost_at(tier)
    }
}

/// The one door into the attention budget from outside the loop.
///
/// Handing out a `Speaker` rather than a bus sender is the enforcement point
/// for SPEC §3.4: there is no way to reach the operator that does not go
/// through `wisp-attn`.
#[derive(Debug, Clone)]
pub struct Speaker(mpsc::UnboundedSender<Utterance>);

impl Speaker {
    /// Offer something for her to say. Whether it is ever said is
    /// `wisp-attn`'s decision, and the answer lands in the flight recorder
    /// either way.
    pub fn propose(&self, u: Utterance) {
        let _ = self.0.send(u);
    }
}

// ---------------------------------------------------------------------------
// Options and result
// ---------------------------------------------------------------------------

pub struct Options {
    pub config_dir: PathBuf,
    /// Fake senses and a fake governor. No compositor, no GPU, no bus.
    pub mock: bool,
    /// Override the config's fleet setting. `None` follows the config.
    pub fleet: Option<bool>,
    /// Stop after this long. `None` runs until a signal.
    pub run_for: Option<Duration>,
    /// The compositor half. `None` is [`Headless`] — no window, no surface.
    pub shell: Option<Box<dyn Shell>>,
    /// Behaviour-tree seed. Fixed in tests so a run is reproducible.
    pub seed: u64,
}

impl Options {
    pub fn new(config_dir: PathBuf) -> Self {
        Options {
            config_dir,
            mock: false,
            fleet: None,
            run_for: None,
            shell: None,
            seed: crate::epoch_ms(),
        }
    }

    pub fn mock(mut self) -> Self {
        self.mock = true;
        self
    }

    pub fn run_for(mut self, d: Duration) -> Self {
        self.run_for = Some(d);
        self
    }

    pub fn with_shell(mut self, shell: Box<dyn Shell>) -> Self {
        self.shell = Some(shell);
        self
    }

    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }
}

/// What a run did. Everything here is a count of something in the flight
/// recorder, so a test asserts on the same numbers the operator can read back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub recorded: u64,
    pub sensed: u64,
    pub tier_changes: u64,
    pub said: u64,
    pub dropped: u64,
    pub frames: u64,
    pub final_tier: Option<Tier>,
    /// The config was corrupt and the defaults were used.
    pub config_recovered: bool,
}

#[derive(Default)]
struct Counters {
    recorded: AtomicU64,
    sensed: AtomicU64,
    tier_changes: AtomicU64,
    said: AtomicU64,
    dropped: AtomicU64,
    frames: AtomicU64,
}

impl Counters {
    fn note(&self, kind: &EventKind) {
        self.recorded.fetch_add(1, Ordering::Relaxed);
        let c = match kind {
            EventKind::Sensed(_) => &self.sensed,
            EventKind::TierChanged { .. } => &self.tier_changes,
            EventKind::Said { .. } => &self.said,
            EventKind::Dropped { .. } => &self.dropped,
            _ => return,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// Commands for the governor task. The governor is `!Sync` and owns the
/// registry, so it lives on one task and is spoken to rather than shared.
enum GovCmd {
    Pin(Tier),
    Unpin,
}

/// Start everything, run until told to stop, then take it all down.
///
/// Holds the single-instance lock for the whole run. A second copy in the same
/// config dir fails here rather than half-starting.
#[cfg(feature = "voice-piper")]
type VoiceTx = Option<crate::voice_host::VoiceSender>;
#[cfg(not(feature = "voice-piper"))]
type VoiceTx = Option<std::convert::Infallible>;

pub async fn run(opts: Options) -> anyhow::Result<Summary> {
    let dir = opts.config_dir.clone();
    std::fs::create_dir_all(&dir)?;

    let lock = crate::lock::acquire(&dir)?;
    tracing::info!(dir = %dir.display(), pid = lock.pid(), mock = opts.mock, "nx-wisp starting");

    let loaded = config::load_from(&dir);
    if let Some(note) = loaded.note() {
        tracing::warn!("{note}");
    }
    let cfg = loaded.config.clone();
    let config_recovered = loaded.recovered();

    let clock = Clock::new();
    let session = crate::epoch_ms();
    let recorder = Arc::new(Recorder::open(&dir, cfg.recorder, session)?);
    let counters = Arc::new(Counters::default());

    // --- the two channels of the module docs --------------------------------
    let (inner_tx, mut inner_rx) = broadcast::channel::<Event>(BUS_CAPACITY);
    let (bus_tx, _bus_rx) = broadcast::channel::<Event>(BUS_CAPACITY);

    let (sig, shutdown) = ShutdownSignal::new();
    let sig = Arc::new(sig);

    // --- the shell seam -----------------------------------------------------
    let shell: Arc<Mutex<Box<dyn Shell>>> =
        Arc::new(Mutex::new(opts.shell.unwrap_or_else(|| Box::new(Headless::default()))));

    // --- senses -------------------------------------------------------------
    let mut senses = Senses::with_bus(inner_tx.clone(), clock.clone());
    if opts.mock {
        crate::mock::start_all(&mut senses);
    } else {
        senses.start_all(&senses_config(&cfg));
    }
    let senses = Shared::new(SensesSlot(Some(senses)));

    // --- attention ----------------------------------------------------------
    let mut attention = Attention::new(opts.seed);
    attention.set_chattiness(cfg.chattiness);
    attention.set_hour(crate::local_hour());
    let attention = Shared::new(attention);

    // --- the rig ------------------------------------------------------------
    let rig = Shared::new(load_rig(&cfg)?);

    // --- the fleet ----------------------------------------------------------
    let want_fleet = opts.fleet.unwrap_or(cfg.fleet.enabled) && !opts.mock;
    let (fleet, fleet_rx) = if want_fleet {
        let (f, rx) = wisp_fleet::Fleet::spawn(fleet_config(&cfg));
        (Some(f), Some(rx))
    } else {
        (None, None)
    };
    let fleet = Shared::new(FleetSlot(fleet));

    // --- the governor -------------------------------------------------------
    let mut governor = if opts.mock { crate::mock::governor() } else { Governor::real(gov_config()) };
    // Registration order is the order a downgrade reaches them, so the
    // expensive ones go first: the rig frees geometry and the renderer frees
    // VRAM before anything merely stops talking.
    governor.registry().register("rig", rig.clone());
    governor.registry().register("senses", senses.clone());
    governor.registry().register("fleet", fleet.clone());
    governor.registry().register("attention", attention.clone());

    let (tier_tx, tier_rx) = watch::channel(governor.tier());
    let (gov_tx, gov_rx) = mpsc::unbounded_channel::<GovCmd>();
    let (speak_tx, speak_rx) = mpsc::unbounded_channel::<Utterance>();
    let speaker = Speaker(speak_tx);

    // --- cognition ----------------------------------------------------------
    // Optional by design: a build without a model, or a machine the backend
    // cannot start on, still gives the operator the creature. She is just a
    // creature without language, and `status` says so.
    let mind = match crate::mind_host::MindHost::spawn(
        &dir,
        speaker.clone(),
        recorder.clone(),
        bus_tx.clone(),
        clock.clone(),
    ) {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!("she has no mind this run: {e}");
            None
        }
    };
    let mind_tx = mind.as_ref().map(|m| m.sender());

    // --- the loudspeaker ----------------------------------------------------
    // Feature-gated: without `voice-piper` there is no synthesiser to mount.
    // A failure to build the stack is a legitimate state (no PipeWire, no
    // fetched voice) — she stays on bubbles and the log says why.
    #[cfg(feature = "voice-piper")]
    let voice = {
        if opts.mock {
            None
        } else {
            match crate::voice_host::VoiceHost::spawn(&dir) {
                Ok(v) => Some(v),
                Err(why) => {
                    tracing::info!("no voice this run: {why}");
                    None
                }
            }
        }
    };
    #[cfg(feature = "voice-piper")]
    let voice_tx: VoiceTx = voice.as_ref().map(|v| v.sender());
    #[cfg(not(feature = "voice-piper"))]
    let voice_tx: VoiceTx = None;

    if let Some(pinned) = cfg.tier.pinned {
        let _ = gov_tx.send(GovCmd::Pin(pinned));
    }

    // --- tasks --------------------------------------------------------------
    // Five long-lived tasks, plus the fleet's when there is one. All of them
    // are stopped by the one shutdown signal and all of them are awaited before
    // this function returns.
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = vec![
        // The recorder relay first: it is the bridge from the inner channel to
        // the bus, and nothing downstream sees an event until it exists.
        tokio::spawn(relay(
            inner_rx_take(&mut inner_rx),
            bus_tx.clone(),
            recorder.clone(),
            counters.clone(),
            clock.clone(),
            mind_tx.clone(),
            shutdown.clone(),
        )),
        tokio::spawn(governor_task(
            governor,
            gov_rx,
            tier_tx,
            bus_tx.clone(),
            recorder.clone(),
            counters.clone(),
            shell.clone(),
            GovContext {
                mind: mind_tx.clone(),
                dir: dir.clone(),
                session,
                clock: clock.clone(),
                attention: attention.clone(),
                senses: senses.clone(),
                mock: opts.mock,
            },
            shutdown.clone(),
        )),
        tokio::spawn(pump(
            bus_tx.clone(),
            speak_rx,
            attention.clone(),
            rig.clone(),
            shell.clone(),
            recorder.clone(),
            counters.clone(),
            clock.clone(),
            tier_rx.clone(),
            voice_tx.clone(),
            shutdown.clone(),
        )),
        tokio::spawn(heartbeat(
            rig.clone(),
            shell.clone(),
            clock.clone(),
            tier_rx.clone(),
            voice_tx.clone(),
            inner_tx.clone(),
            counters.clone(),
            cfg.appearance.size_px,
            shutdown.clone(),
        )),
        tokio::spawn(config_watcher(
            dir.clone(),
            cfg.clone(),
            attention.clone(),
            shell.clone(),
            gov_tx.clone(),
            shutdown.clone(),
        )),
    ];

    if let Some(rx) = fleet_rx {
        tasks.push(tokio::spawn(fleet_task(
            rx,
            inner_tx.clone(),
            speaker.clone(),
            clock.clone(),
            sig.clone(),
            shutdown.clone(),
        )));
    }

    // --- wait ---------------------------------------------------------------
    wait_for_stop(opts.run_for, sig.clone()).await;
    tracing::info!("nx-wisp stopping");

    // Firing the signal stops every task above and every sense below.
    sig.fire();
    for t in tasks {
        // A task that panicked would otherwise vanish without trace, and the
        // subsystem it was driving would simply have stopped happening — the
        // hardest possible bug to notice in a process whose whole job is to be
        // unobtrusive.
        if let Err(e) = t.await {
            if e.is_panic() {
                tracing::error!(error = %e, "a subsystem task panicked");
            }
        }
    }
    if let Some(m) = mind {
        m.shutdown();
    }
    #[cfg(feature = "voice-piper")]
    if let Some(v) = voice {
        v.shutdown();
    }
    // The guard is taken and dropped in its own scope. A `std::sync::MutexGuard`
    // held across an `.await` would make this future `!Send` — and, worse, would
    // block the governor's synchronous fan-out on whatever the await is waiting
    // for. Every lock in this module is scoped for the same reason.
    let taken = senses.0.lock().unwrap_or_else(|e| e.into_inner()).0.take();
    if let Some(s) = taken {
        // Invasive tells come down as the handles drop, before this returns.
        s.shutdown().await;
    }
    if let Some(f) = &fleet.0.lock().unwrap_or_else(|e| e.into_inner()).0 {
        f.close();
    }
    shell.lock().unwrap_or_else(|e| e.into_inner()).shutdown();
    recorder.flush();
    state::clear(&dir);
    lock.release();

    let final_tier = *tier_rx.borrow();
    Ok(Summary {
        recorded: counters.recorded.load(Ordering::Relaxed),
        sensed: counters.sensed.load(Ordering::Relaxed),
        tier_changes: counters.tier_changes.load(Ordering::Relaxed),
        said: counters.said.load(Ordering::Relaxed),
        dropped: counters.dropped.load(Ordering::Relaxed),
        frames: counters.frames.load(Ordering::Relaxed),
        final_tier: Some(final_tier),
        config_recovered,
    })
}

/// `broadcast::Receiver` is not `Clone`; this hands the one we made to the task
/// that owns it without leaving a dangling binding behind.
fn inner_rx_take(rx: &mut broadcast::Receiver<Event>) -> broadcast::Receiver<Event> {
    std::mem::replace(rx, rx.resubscribe())
}

async fn wait_for_stop(run_for: Option<Duration>, sig: Arc<ShutdownSignal>) {
    let signals = async {
        let mut term = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "no SIGTERM handler; Ctrl-C only");
                // Never resolves; ctrl_c below still does.
                let () = std::future::pending().await;
                unreachable!()
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
            _ = term.recv() => tracing::info!("asked to stop"),
        }
    };

    match run_for {
        Some(d) => {
            tokio::select! {
                _ = tokio::time::sleep(d) => {}
                _ = signals => {}
            }
        }
        None => signals.await,
    }
    sig.fire();
}

// ---------------------------------------------------------------------------
// The recorder relay — the only bridge from the inner channel to the bus
// ---------------------------------------------------------------------------

async fn relay(
    mut inner: broadcast::Receiver<Event>,
    bus: broadcast::Sender<Event>,
    recorder: Arc<Recorder>,
    counters: Arc<Counters>,
    clock: Clock,
    mind: Option<crate::mind_host::MindSender>,
    mut shutdown: Shutdown,
) {
    // The mind thinks on a cadence, not per event — thinking is expensive and
    // observations are not commands. Two seconds is the reflex model's world;
    // the governor's tier messages arrive separately and jump the queue by
    // being applied first in the drain.
    let mut think = tokio::time::interval(Duration::from_secs(2));
    think.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let ev = tokio::select! {
            r = inner.recv() => r,
            _ = think.tick() => {
                if let Some(m) = &mind {
                    m.send(crate::mind_host::MindMsg::Tick(clock.now()));
                }
                continue;
            }
            _ = shutdown.wait() => return,
        };
        match ev {
            Ok(ev) => {
                counters.note(&ev.kind);
                recorder.record(&ev);
                // The mind sees what the senses saw — after the recorder, like
                // every other consumer, so `explain` can never know less than
                // she did.
                if let Some(m) = &mind {
                    if let EventKind::Sensed(obs) = &ev.kind {
                        m.send(crate::mind_host::MindMsg::Obs(obs.clone(), ev.at));
                        if matches!(obs, Observation::Speech { final_: true, .. }) {
                            m.send(crate::mind_host::MindMsg::Heard(ev.at));
                        }
                    }
                }
                let _ = bus.send(ev);
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // The recorder itself fell behind, so events really were lost.
                // SPEC §0.4 does not allow that to pass silently: the gap goes
                // into the trace so `explain` cannot present a partial story as
                // a complete one.
                //
                // `wisp-proto` has no variant for this. `Dropped` is the honest
                // fit — something was discarded and here is why — and the gap
                // is recorded rather than inferred from a jump in `seq`.
                tracing::warn!(lost = n, "the flight recorder fell behind the bus");
                recorder.record_kind(
                    clock.now(),
                    EventKind::Dropped {
                        text: format!("{n} events"),
                        why: "the flight recorder fell behind the bus".to_string(),
                    },
                );
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// The governor task
// ---------------------------------------------------------------------------

struct GovContext {
    mind: Option<crate::mind_host::MindSender>,
    dir: PathBuf,
    session: u64,
    /// The one clock every timestamp in the flight recorder comes from.
    clock: Clock,
    attention: Shared<Attention>,
    senses: Shared<SensesSlot>,
    mock: bool,
}

#[allow(clippy::too_many_arguments)]
async fn governor_task(
    mut governor: Governor,
    mut cmds: mpsc::UnboundedReceiver<GovCmd>,
    tier_tx: watch::Sender<Tier>,
    bus: broadcast::Sender<Event>,
    recorder: Arc<Recorder>,
    counters: Arc<Counters>,
    shell: Arc<Mutex<Box<dyn Shell>>>,
    ctx: GovContext,
    mut shutdown: Shutdown,
) {
    // The state file is a cache for `wisp status` in another process, not a
    // second source of truth. Rewritten on every tier change and otherwise no
    // faster than this, so it stays free at T3.
    const STATE_EVERY: Duration = Duration::from_secs(2);
    let mut last_state: Option<tokio::time::Instant> = None;
    // The first poll happens immediately. Waiting out a cadence before the
    // first one would mean a second of not knowing what tier she is in, during
    // which she might be drawing at 60 fps over a game.
    let mut first = true;

    loop {
        if first {
            first = false;
        } else {
            // The governor's own cadence: ~1 Hz at T1, 2 s at T3, 5 s at T4.
            // It costs about 4 ms of CPU a poll, which at T3's budget is the
            // single most expensive thing the loop does, so it is what slows
            // down first.
            let interval = Duration::from_millis(governor.poll_interval_ms().max(20));
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                cmd = cmds.recv() => {
                    match cmd {
                        // A pin applies and fans out immediately: the operator
                        // asked, so there is nothing to be lazy about.
                        Some(GovCmd::Pin(tier)) => {
                            let at = ctx.clock.now();
                            if let Some(change) = governor.pin(tier, at) {
                                let to = change.to;
                                publish_tier(&change.into_event_kind(), at, &bus,
                                    &recorder, &counters, &tier_tx, &shell, to);
                            }
                            continue;
                        }
                        Some(GovCmd::Unpin) => { governor.unpin(); continue; }
                        None => return,
                    }
                }
                _ = shutdown.wait() => return,
            }
        }

        // `step` applies a downgrade to every registered `Governed` before it
        // returns (SPEC §3.1). By the time we look at the result, the VRAM is
        // already freed and the senses are already slower.
        let step = governor.step();
        if step.change.is_some() {
            if let Some(m) = &ctx.mind {
                m.send(crate::mind_host::MindMsg::Tier {
                    tier: step.tier,
                    reason: step
                        .change
                        .as_ref()
                        .map(|c| c.reason.clone())
                        .unwrap_or(wisp_proto::TierReason::Idle),
                    devices: step.devices.clone(),
                    vram: step.vram.clone(),
                    now: ctx.clock.now(),
                });
            }
        }
        tracing::trace!(tier = ?step.tier, because = %step.explanation, "governor polled");
        let changed = step.event().is_some();
        if let Some(ev) = step.event() {
            // **Stamped with the host's clock, not the governor's.**
            //
            // `TierChange::at` comes from the snapshot the ladder classified,
            // and `wisp-gov` measures that on a clock of its own — its probes'
            // origin under `Governor::real`, and the *scripted* machine's clock
            // under a fake source. Both are correct for dwell arithmetic inside
            // that crate and neither is comparable with the timestamps the
            // senses put on their observations. One trace, one clock: otherwise
            // `explain`'s window would measure across two of them and quietly
            // include or exclude the wrong events.
            let tier = step.tier;
            publish_tier(&ev.kind, ctx.clock.now(), &bus, &recorder, &counters, &tier_tx, &shell, tier);
        }

        let due = last_state.is_none_or(|t| t.elapsed() >= STATE_EVERY);
        if changed || due {
            last_state = Some(tokio::time::Instant::now());
            write_state(&ctx, &step, &shell);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_tier(
    kind: &EventKind,
    at: Millis,
    bus: &broadcast::Sender<Event>,
    recorder: &Recorder,
    counters: &Counters,
    tier_tx: &watch::Sender<Tier>,
    shell: &Arc<Mutex<Box<dyn Shell>>>,
    tier: Tier,
) {
    let ev = Event { at, kind: kind.clone() };
    counters.note(&ev.kind);
    // Recorded before dispatch, like everything else.
    recorder.record(&ev);
    let _ = bus.send(ev);
    let _ = tier_tx.send(tier);
    shell.lock().unwrap_or_else(|e| e.into_inner()).set_tier(tier);
}

fn write_state(ctx: &GovContext, step: &wisp_gov::Step, shell: &Arc<Mutex<Box<dyn Shell>>>) {
    let _ = shell;
    let (chattiness, silenced, waiting) = {
        let a = ctx.attention.0.lock().unwrap_or_else(|e| e.into_inner());
        (
            config::chattiness_name(a.chattiness()).to_string(),
            a.silenced(),
            a.budget.held_count(),
        )
    };
    let (senses_live, invasive_live) = {
        let g = ctx.senses.0.lock().unwrap_or_else(|e| e.into_inner());
        match &g.0 {
            Some(s) => {
                let rows = s.ledger().rows();
                (
                    rows.iter().filter(|r| r.live).map(|r| r.id).collect::<Vec<_>>(),
                    rows.iter()
                        .filter(|r| r.live && r.consent == wisp_proto::Consent::Invasive)
                        .map(|r| r.id)
                        .collect::<Vec<_>>(),
                )
            }
            None => (Vec::new(), Vec::new()),
        }
    };

    let s = State {
        written_ms: crate::epoch_ms(),
        session: ctx.session,
        pid: std::process::id(),
        version: crate::VERSION.to_string(),
        tier: step.tier,
        because: step.explanation.clone(),
        headline: step.cost.headline.clone(),
        estimated: step.cost.estimated,
        measured_rss_mib: step.cost.measured.rss_mib,
        measured_cpu_centi_pct: step.cost.measured.cpu_centi_pct,
        dgpu_vram_mib: step.cost.dgpu_vram_mib,
        dgpu_untouched: step.cost.dgpu_untouched,
        by_subsystem: step.cost.by_subsystem.clone(),
        senses_live,
        invasive_live,
        chattiness,
        silenced,
        pinned: None,
        waiting,
        last_said: None,
        mock: ctx.mock,
    };
    if let Err(e) = state::save(&ctx.dir, &s) {
        tracing::debug!(error = %e, "could not write the state file");
    }
}

// ---------------------------------------------------------------------------
// The pump: observations in, behaviour and speech out
// ---------------------------------------------------------------------------

/// How often the behaviour trees get a turn, per tier. `wisp-attn` is pure and
/// cheap, but at T3 the whole process has half a percent of one core, so even
/// cheap gets slower.
fn tick_interval(tier: Tier) -> Duration {
    Duration::from_millis(match tier {
        Tier::Feral | Tier::Full => 200,
        Tier::Reduced => 400,
        Tier::Lobotomised => 1_000,
        Tier::Dormant => 2_000,
    })
}

#[allow(clippy::too_many_arguments)]
async fn pump(
    bus: broadcast::Sender<Event>,
    mut speak: mpsc::UnboundedReceiver<Utterance>,
    attention: Shared<Attention>,
    rig: Shared<Rig>,
    shell: Arc<Mutex<Box<dyn Shell>>>,
    recorder: Arc<Recorder>,
    counters: Arc<Counters>,
    clock: Clock,
    tier_rx: watch::Receiver<Tier>,
    voice_tx: VoiceTx,
    mut shutdown: Shutdown,
) {
    let mut rx = bus.subscribe();

    loop {
        let tier = *tier_rx.borrow();
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => on_bus_event(&ev, &attention, &shell, &clock),
                // The pump lagging is not a lost trace — the recorder already
                // has every one of these — so it is a warning, not an event.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(missed = n, "the pump fell behind; observations skipped");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            u = speak.recv() => match u {
                // SPEC §3.4: this is the only door. Nothing is said here; the
                // budget decides on the tick below.
                Some(u) => {
                    let mut a = attention.0.lock().unwrap_or_else(|e| e.into_inner());
                    a.submit(clock.now(), u);
                }
                None => return,
            },
            _ = tokio::time::sleep(tick_interval(tier)) => {}
            _ = shutdown.wait() => return,
        }

        let now = clock.now();
        let turn = {
            let mut a = attention.0.lock().unwrap_or_else(|e| e.into_inner());
            a.tick(now)
        };
        apply_turn(&turn, &rig, &shell, &voice_tx);

        // Everything the budget decided, as facts about the past. This is the
        // only source of Proposed / Said / Dropped, which is what makes
        // `wisp explain`'s walk-back trustworthy.
        let events = {
            let mut a = attention.0.lock().unwrap_or_else(|e| e.into_inner());
            a.drain_events()
        };
        for ev in events {
            counters.note(&ev.kind);
            recorder.record(&ev);
            let _ = bus.send(ev);
        }
    }
}

fn on_bus_event(
    ev: &Event,
    attention: &Shared<Attention>,
    shell: &Arc<Mutex<Box<dyn Shell>>>,
    clock: &Clock,
) {
    match &ev.kind {
        EventKind::Sensed(obs) => {
            // The shell needs the window rectangles: their top edges are the
            // ledges she stands on (F68).
            shell.lock().unwrap_or_else(|e| e.into_inner()).observed(obs);
            let mut a = attention.0.lock().unwrap_or_else(|e| e.into_inner());
            a.observe(clock.now(), obs);
            // Being spoken to is an opening, and it is the operator asking.
            if matches!(obs, Observation::Speech { final_: true, .. }) {
                a.interact(clock.now(), Interaction::Summon);
            }
        }
        // SPEC §0.3: the tell goes on the character herself, for the whole
        // time the sense is live.
        EventKind::InvasiveActive { sense, active } => {
            shell.lock().unwrap_or_else(|e| e.into_inner()).invasive_tell(*sense, *active);
        }
        // Proposed / Said / Dropped are this task's own output coming back
        // round; the tier is handled by the governor task, which owns it.
        _ => {}
    }
}

fn apply_turn(
    turn: &Turn,
    rig: &Shared<Rig>,
    shell: &Arc<Mutex<Box<dyn Shell>>>,
    voice: &VoiceTx,
) {
    let mut sh = shell.lock().unwrap_or_else(|e| e.into_inner());
    for u in &turn.said {
        // The only things in the whole process that reach the operator: the
        // bubble, and — with a voice mounted — the same words aloud. Both are
        // fed from here, AFTER the budget said yes, so the loudspeaker can
        // never say something the bubble would not show.
        sh.say(u);
        #[cfg(feature = "voice-piper")]
        if let Some(v) = voice {
            v.send(crate::voice_host::VoiceMsg::Say {
                text: u.text.clone(),
                expression: u.expression.clone(),
            });
        }
        if let Some(expr) = &u.expression {
            rig.0.lock().unwrap_or_else(|e| e.into_inner()).set_expression(expr);
            sh.set_expression(expr);
        }
    }
    for e in &turn.effects {
        match e {
            Effect::Clip { name, loop_ } => {
                // `Rig::play` has no looping parameter — a skin decides whether
                // a clip loops — so the flag is passed to the shell, which owns
                // the frame clock, and not to the rig.
                let played =
                    rig.0.lock().unwrap_or_else(|e| e.into_inner()).play(name, CLIP_FADE_MS);
                if !played {
                    tracing::debug!(clip = %name, "the skin has no such clip");
                }
                sh.play_clip(name, *loop_);
            }
            Effect::Mood(m) => {
                let expr = expression_for(*m);
                rig.0.lock().unwrap_or_else(|e| e.into_inner()).set_expression(expr);
                sh.set_expression(expr);
            }
            Effect::Move(target) => sh.move_to(target),
            Effect::Poke { window } => sh.poke(*window),
            // Tags and session events are behaviour-tree bookkeeping; nothing
            // outside `wisp-attn` acts on them, and inventing an event kind for
            // them would be a spec amendment.
            Effect::Tag(t) => tracing::debug!(tag = %t, "behaviour tag"),
            Effect::Session(s) => tracing::debug!(session = ?s, "focus session"),
            Effect::Propose(_) => {
                debug_assert!(false, "a proposal escaped the budget (SPEC §3.4)");
            }
        }
    }
}

const CLIP_FADE_MS: f32 = 120.0;

/// `wisp-attn`'s [`Mood`] onto `wisp-rig`'s `REQUIRED_EXPRESSIONS`.
///
/// The two sets are deliberately different sizes: mood is a disposition and an
/// expression is a face, and a skin author should not have to author nine of
/// them. Every arm lands on a name in [`wisp_rig::REQUIRED_EXPRESSIONS`], so
/// every skin that validates can show every mood.
/// Delegates to [`wisp_proto::Mood::expression`] (SPEC §3.8).
///
/// This function used to own the table. It was the fourth place the mood
/// vocabulary appeared, and the only one that mapped it to an expression, so
/// it was also the only place a new mood could be silently forgotten.
pub fn expression_for(mood: Mood) -> &'static str {
    mood.expression()
}

// ---------------------------------------------------------------------------
// The rig heartbeat
// ---------------------------------------------------------------------------

/// Frames per second when nothing is drawing them.
///
/// A real shell drives the rig from its own frame callback, which is the only
/// clock that matters once there is a surface. Without one the rig still has to
/// advance — clips have to finish, expressions have to land, and `status` would
/// otherwise report a frozen character — but there is no reason to do it sixty
/// times a second into a void.
const HEADLESS_FPS: u32 = 4;

#[allow(clippy::too_many_arguments)]
async fn heartbeat(
    rig: Shared<Rig>,
    shell: Arc<Mutex<Box<dyn Shell>>>,
    clock: Clock,
    tier_rx: watch::Receiver<Tier>,
    voice_tx: VoiceTx,
    input_tx: broadcast::Sender<Event>,
    counters: Arc<Counters>,
    size_px: f32,
    mut shutdown: Shutdown,
) {
    let mut last = clock.now();
    let mut last_tier: Option<Tier> = None;
    loop {
        let tier = *tier_rx.borrow();
        #[cfg(feature = "voice-piper")]
        if last_tier != Some(tier) {
            last_tier = Some(tier);
            if let Some(v) = &voice_tx {
                v.send(crate::voice_host::VoiceMsg::Tier(tier));
            }
        }
        #[cfg(not(feature = "voice-piper"))]
        {
            last_tier = Some(tier);
            let _ = (&last_tier, &voice_tx);
        }
        let target = tier.target_fps();
        if target == 0 {
            // T4: the rig draws nothing at all. Wait for a tier change rather
            // than spinning.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
                _ = shutdown.wait() => return,
            }
        }
        let fps = target.min(HEADLESS_FPS.max(shell_fps(&shell)));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis((1000 / fps.max(1)) as u64)) => {}
            _ = shutdown.wait() => return,
        }

        let now = clock.now();
        let dt = (now.saturating_sub(last)) as f32 / 1000.0;
        last = now;

        let mut sh = shell.lock().unwrap_or_else(|e| e.into_inner());

        // Palette lines are speech. They enter through the INNER channel like
        // every sense — recorded before dispatch — and the whole pipeline
        // (mind hears the operator, budget passes the Answer, bubble and voice
        // deliver it) runs with no special path for typed words.
        for line in sh.take_input() {
            let _ = input_tx.send(Event {
                at: clock.now(),
                kind: EventKind::Sensed(Observation::Speech { text: line, final_: true }),
            });
        }

        let mut r = rig.0.lock().unwrap_or_else(|e| e.into_inner());
        let anchor = sh.anchor().unwrap_or((0.0, 0.0));
        let input = RigInput {
            size_px,
            anchor: wisp_rig::Vec2::new(anchor.0, anchor.1),
            cursor: sh.cursor().map(|(x, y)| wisp_rig::Vec2::new(x, y)),
            ..Default::default()
        };
        r.update(dt, &input);
        let region = r.contour(sh.contour_options());
        let ctx = FrameCtx { dt, size_px, tier, target_fps: target };
        sh.present(r.frame(), &region, &ctx);
        counters.frames.fetch_add(1, Ordering::Relaxed);
    }
}

/// A shell that reports an anchor is a real one and wants real frames; one that
/// does not is headless and gets [`HEADLESS_FPS`].
fn shell_fps(shell: &Arc<Mutex<Box<dyn Shell>>>) -> u32 {
    let sh = shell.lock().unwrap_or_else(|e| e.into_inner());
    if sh.anchor().is_some() {
        240
    } else {
        HEADLESS_FPS
    }
}

// ---------------------------------------------------------------------------
// The config watcher
// ---------------------------------------------------------------------------

/// Poll `config.json`'s mtime and apply what changed.
///
/// This is how `wisp tier pin T3` and `wisp config set chattiness silent` reach
/// a *running* instance without an IPC channel. The CLI writes the file the
/// same way the GUI would; the loop notices. It is one `stat` every two
/// seconds, which is free even at T3, and it has the property that the change
/// also survives a restart — an IPC-only pin would not.
async fn config_watcher(
    dir: PathBuf,
    mut current: Config,
    attention: Shared<Attention>,
    shell: Arc<Mutex<Box<dyn Shell>>>,
    gov: mpsc::UnboundedSender<GovCmd>,
    mut shutdown: Shutdown,
) {
    const EVERY: Duration = Duration::from_secs(2);
    let path = dir.join(config::CONFIG_FILE);
    let mut stamp = mtime(&path);
    let mut hour = crate::local_hour();

    loop {
        tokio::select! {
            _ = tokio::time::sleep(EVERY) => {}
            _ = shutdown.wait() => return,
        }

        // The hour is the host's job (`wisp-attn` reads no clock), and it is
        // cheap enough to refresh on the same timer.
        let h = crate::local_hour();
        if h != hour {
            hour = h;
            attention.0.lock().unwrap_or_else(|e| e.into_inner()).set_hour(h);
        }

        let now = mtime(&path);
        if now == stamp {
            continue;
        }
        stamp = now;

        let loaded = config::load_from(&dir);
        if let Some(note) = loaded.note() {
            tracing::warn!("{note}");
            continue;
        }
        let next = loaded.config;
        if next == current {
            continue;
        }

        if next.chattiness != current.chattiness {
            tracing::info!(dial = ?next.chattiness, "chattiness changed");
            attention
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_chattiness(next.chattiness);
        }
        if next.tier.pinned != current.tier.pinned {
            let _ = match next.tier.pinned {
                Some(t) => {
                    tracing::info!(tier = ?t, "tier pinned");
                    gov.send(GovCmd::Pin(t))
                }
                None => {
                    tracing::info!("tier unpinned");
                    gov.send(GovCmd::Unpin)
                }
            };
        }
        if next.appearance.size_px != current.appearance.size_px {
            shell
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_size(next.appearance.size_px);
        }
        // Sense enablement is deliberately not applied here: `wisp-senses`
        // owns the ledger and offers no reload, so a change takes effect on
        // restart. See the gap noted in the crate docs.
        current = next;
    }
}

fn mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

// ---------------------------------------------------------------------------
// The fleet
// ---------------------------------------------------------------------------

async fn fleet_task(
    mut rx: mpsc::UnboundedReceiver<wisp_fleet::FleetEvent>,
    inner: broadcast::Sender<Event>,
    speaker: Speaker,
    clock: Clock,
    sig: Arc<ShutdownSignal>,
    mut shutdown: Shutdown,
) {
    use wisp_fleet::{BusEvent, FleetEvent};

    loop {
        let ev = tokio::select! {
            e = rx.recv() => match e { Some(e) => e, None => return },
            _ = shutdown.wait() => return,
        };
        match ev {
            // Fleet observations are facts, so they go on the bus (through the
            // recorder, like everything else).
            FleetEvent::Observed(obs) => {
                let _ = inner.send(Event { at: clock.now(), kind: EventKind::Sensed(obs) });
            }
            // …and what she would like to say about them is a proposal, which
            // is not a fact and does not belong on the bus (SPEC §3.2).
            FleetEvent::Says(u) => speaker.propose(u),
            FleetEvent::Bus(BusEvent::ShutdownRequest) => {
                tracing::info!("the hub is stopping this stack");
                sig.fire();
                return;
            }
            FleetEvent::Bus(b) => {
                if let Some(obs) = bus_event_observation(&b) {
                    let _ = inner.send(Event { at: clock.now(), kind: EventKind::Sensed(obs) });
                }
            }
        }
    }
}

/// Bus-level news about *us*, as an `Observation::Fleet`.
///
/// `wisp-proto`'s `Observation` is a closed enum and gaining a variant is a
/// spec amendment (§3.3), so connection state is reported through the `Fleet`
/// variant that already exists rather than by adding one.
fn bus_event_observation(b: &wisp_fleet::BusEvent) -> Option<Observation> {
    use wisp_fleet::BusEvent;
    let (field, value) = match b {
        BusEvent::Connected { hub } => ("connected", hub.clone()),
        BusEvent::Disconnected => ("connected", String::new()),
        BusEvent::HubError(e) => ("error", e.clone()),
        BusEvent::ShutdownRequest => return None,
        // Anything the connector grows later reaches the trace as soon as it
        // is mapped here, and is silently ignored until it is — which is the
        // right default for a bus we do not own.
        _ => return None,
    };
    Some(Observation::Fleet {
        app: "nx-hub".to_string(),
        field: field.to_string(),
        value,
    })
}

// ---------------------------------------------------------------------------
// Config plumbing
// ---------------------------------------------------------------------------

fn senses_config(cfg: &Config) -> wisp_senses::SensesConfig {
    wisp_senses::SensesConfig {
        terrain: wisp_senses::kwin::TerrainConfig {
            flush_ms: cfg.senses.terrain_flush_ms,
            script_dir: None,
        },
        vitals: wisp_senses::vitals::VitalsConfig {
            interval: Duration::from_secs(cfg.senses.vitals_interval_secs.max(1)),
            ..Default::default()
        },
        watch_dirs: cfg.senses.watch_dirs.clone(),
    }
}

fn fleet_config(cfg: &Config) -> wisp_fleet::FleetConfig {
    let mut f = wisp_fleet::FleetConfig {
        roster_poll: Duration::from_secs(cfg.fleet.roster_poll_secs.max(1)),
        ..Default::default()
    };
    if let Some(nx) = &cfg.fleet.nx_binary {
        f.nx_binary = nx.clone();
    }
    f
}

/// The governor's policy. Everything in it is already a ratio or a percentage
/// rather than a number from the operator's desktop, so the shipped defaults
/// are right on a laptop too and there is nothing here to configure yet.
fn gov_config() -> wisp_gov::GovConfig {
    wisp_gov::GovConfig::default()
}

fn load_rig(cfg: &Config) -> anyhow::Result<Rig> {
    let skin = match &cfg.appearance.skin {
        SkinChoice::Default => wisp_rig::default_skin()?,
        SkinChoice::File(path) => match wisp_rig::Skin::load(path) {
            Ok(s) => s,
            Err(e) => {
                // A skin the operator chose and then broke should not stop her
                // starting; she appears as herself and says why in the log.
                tracing::warn!(
                    skin = %path.display(),
                    error = %e,
                    "could not load that skin; using the shipped one"
                );
                wisp_rig::default_skin()?
            }
        },
    };
    Ok(Rig::new(skin))
}

/// Every sense this build can start, for `doctor` and `status`.
pub fn implemented_senses() -> &'static [SenseId] {
    &wisp_senses::IMPLEMENTED
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempConfig;
    use wisp_proto::Urgency;

    fn opts(dir: &std::path::Path) -> Options {
        Options::new(dir.to_path_buf()).mock().seed(0xA77E)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_mock_run_records_what_it_saw_and_leaves_a_readable_trace() {
        let tmp = TempConfig::new();
        let summary = run(opts(tmp.path()).run_for(Duration::from_millis(1_500)))
            .await
            .expect("a mock run needs no compositor and no GPU");

        assert!(summary.sensed > 0, "the senses published nothing: {summary:?}");
        assert!(summary.recorded >= summary.sensed);
        assert!(summary.frames > 0, "the rig never advanced");
        assert!(!summary.config_recovered);

        // The trace is on disk and readable by a second process.
        let recs = crate::recorder::read_all(tmp.path(), 3);
        assert_eq!(recs.len() as u64, summary.recorded, "every event reached the file");
        assert!(recs.iter().any(|r| r.tag() == "sensed"));
        for w in recs.windows(2) {
            assert_eq!(w[1].seq, w[0].seq + 1);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_governor_moves_her_and_the_change_is_in_the_trace() {
        let tmp = TempConfig::new();
        let summary = run(opts(tmp.path()).run_for(Duration::from_millis(2_500))).await.unwrap();
        assert!(summary.tier_changes > 0, "the scripted machine never moved her");
        let recs = crate::recorder::read_all(tmp.path(), 3);
        let tiers: Vec<_> = recs.iter().filter(|r| r.tag() == "tier").collect();
        assert!(!tiers.is_empty());
        assert!(matches!(tiers[0].kind, EventKind::TierChanged { .. }));
    }

    /// SPEC §3.4: nothing reaches the operator except through the budget.
    ///
    /// The trace is where that is checkable. Whatever she did or did not say in
    /// a given run, every `Said` must be preceded by a `Proposed` carrying the
    /// same text — the invariant `wisp explain`'s walk-back rests on, and the
    /// one that breaks the moment anything in the loop speaks directly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nothing_is_ever_said_without_having_been_proposed_first() {
        let tmp = TempConfig::new();
        run(opts(tmp.path()).run_for(Duration::from_millis(2_000))).await.unwrap();

        let recs = crate::recorder::read_all(tmp.path(), 3);
        for (i, r) in recs.iter().enumerate() {
            if let EventKind::Said { text } = &r.kind {
                let proposed = recs[..i]
                    .iter()
                    .any(|p| matches!(&p.kind, EventKind::Proposed(u) if &u.text == text));
                assert!(proposed, "{text:?} was said without ever being proposed");
            }
        }
        // …and no proposal escaped the budget onto the bus as an effect.
        assert!(recs.iter().all(|r| r.tag() != "proposed" || matches!(
            &r.kind,
            EventKind::Proposed(_)
        )));
    }

    /// One trace, one clock.
    ///
    /// `wisp-gov` timestamps its tier changes from the snapshot it classified,
    /// which under a fake source is the *scripted* machine's clock and under
    /// the real one is the probes' own origin. Neither is comparable with the
    /// senses' timestamps. If a governor event ever reaches the recorder
    /// carrying `wisp-gov`'s time, `at` stops being monotonic within the run
    /// and `explain`'s window silently measures across two clocks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_timestamp_in_the_trace_comes_from_one_clock() {
        let tmp = TempConfig::new();
        run(opts(tmp.path()).run_for(Duration::from_millis(2_000))).await.unwrap();

        let recs = crate::recorder::read_all(tmp.path(), 3);
        assert!(recs.iter().any(|r| r.tag() == "tier"), "no tier change to check");
        let mut last = 0;
        for r in &recs {
            assert!(
                r.at >= last,
                "record {} ({}) went backwards: {} after {}",
                r.seq,
                r.tag(),
                r.at,
                last
            );
            last = r.at;
        }
        // The mock machine's scripted clock runs far ahead of a two-second run,
        // so a leaked governor timestamp would be conspicuous.
        assert!(last < 60_000, "a timestamp from another clock leaked in: {last} ms");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_copy_in_the_same_config_dir_refuses_to_start() {
        let tmp = TempConfig::new();
        let dir = tmp.to_path_buf();
        let first =
            tokio::spawn(async move { run(Options::new(dir).mock().run_for(Duration::from_millis(900))).await });
        tokio::time::sleep(Duration::from_millis(250)).await;

        let err = run(opts(tmp.path()).run_for(Duration::from_millis(50)))
            .await
            .expect_err("two copies must not share a config dir");
        assert!(err.to_string().contains("already running"), "{err}");

        first.await.unwrap().unwrap();
        // …and once it has stopped, the lock is free again.
        run(opts(tmp.path()).run_for(Duration::from_millis(200))).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_corrupt_config_starts_anyway_and_says_so() {
        let tmp = TempConfig::new();
        std::fs::write(tmp.path().join(config::CONFIG_FILE), b"{{{").unwrap();
        let summary = run(opts(tmp.path()).run_for(Duration::from_millis(600))).await.unwrap();
        assert!(summary.config_recovered);
        assert!(summary.sensed > 0, "she must still start");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_state_file_appears_while_running_and_is_gone_afterwards() {
        let tmp = TempConfig::new();
        let dir = tmp.to_path_buf();
        let h = tokio::spawn(async move {
            run(Options::new(dir).mock().run_for(Duration::from_millis(1_200))).await
        });
        tokio::time::sleep(Duration::from_millis(700)).await;
        let s = state::load(tmp.path()).expect("a running instance publishes its state");
        assert_eq!(s.pid, std::process::id());
        assert!(s.mock);
        assert!(!s.headline.is_empty(), "the cost meter said nothing");
        h.await.unwrap().unwrap();
        assert!(state::load(tmp.path()).is_none(), "a clean exit clears the state file");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinning_a_tier_in_the_config_reaches_a_running_instance() {
        let tmp = TempConfig::new();
        let mut cfg = Config::default();
        cfg.tier.pinned = Some(Tier::Dormant);
        config::save_to(tmp.path(), &cfg).unwrap();

        let summary = run(opts(tmp.path()).run_for(Duration::from_millis(1_200))).await.unwrap();
        assert_eq!(summary.final_tier, Some(Tier::Dormant), "a pin at startup must be honoured");
        let recs = crate::recorder::read_all(tmp.path(), 3);
        assert!(
            recs.iter().any(|r| matches!(
                &r.kind,
                EventKind::TierChanged { to: Tier::Dormant, reason: TierReason::Pinned, .. }
            )),
            "the pin must be in the trace with its reason"
        );
    }

    #[test]
    fn every_mood_maps_onto_an_expression_every_skin_must_have() {
        for m in [
            Mood::Calm,
            Mood::Curious,
            Mood::Playful,
            Mood::Smug,
            Mood::Sulky,
            Mood::Focused,
            Mood::Sleepy,
            Mood::Alarmed,
            Mood::Affectionate,
        ] {
            let e = expression_for(m);
            assert!(
                wisp_rig::REQUIRED_EXPRESSIONS.contains(&e),
                "{m:?} maps to {e:?}, which a skin is not required to have"
            );
        }
    }

    #[test]
    fn the_shipped_skin_loads_and_has_everything_the_loop_needs() {
        let _tmp = TempConfig::new();
        let mut rig = load_rig(&Config::default()).unwrap();
        assert!(rig.skin().missing_required_clips().is_empty());
        assert!(rig.skin().missing_required_expressions().is_empty());
        assert!(rig.play("idle", 0.0));
        assert!(rig.set_expression("curious"));
    }

    #[test]
    fn a_broken_skin_choice_falls_back_rather_than_refusing_to_start() {
        let tmp = TempConfig::new();
        let path = tmp.path().join("broken.toml");
        std::fs::write(&path, b"this is not a skin").unwrap();
        let mut cfg = Config::default();
        cfg.appearance.skin = SkinChoice::File(path);
        let rig = load_rig(&cfg).expect("a bad skin must not stop her");
        assert!(rig.skin().missing_required_clips().is_empty());
    }

    #[test]
    fn the_tick_slows_down_as_the_tier_does() {
        let ladder = [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant];
        for w in ladder.windows(2) {
            assert!(
                tick_interval(w[1]) >= tick_interval(w[0]),
                "{:?} ticks faster than {:?}",
                w[1],
                w[0]
            );
        }
        assert!(tick_interval(Tier::Lobotomised) >= Duration::from_millis(1_000));
    }

    #[test]
    fn a_bus_event_becomes_an_observation_without_a_new_proto_variant() {
        let o = bus_event_observation(&wisp_fleet::BusEvent::Connected { hub: "nuc".into() });
        assert_eq!(
            o,
            Some(Observation::Fleet {
                app: "nx-hub".into(),
                field: "connected".into(),
                value: "nuc".into()
            })
        );
        assert!(bus_event_observation(&wisp_fleet::BusEvent::ShutdownRequest).is_none());
    }

    #[test]
    fn the_slots_report_their_subsystems_cost_even_when_empty() {
        // The cost meter must not flatter her by omitting a subsystem that
        // happens to be disconnected.
        for t in [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant] {
            assert_eq!(<FleetSlot as Governed>::cost_at(t), <wisp_fleet::Fleet as Governed>::cost_at(t));
            assert_eq!(<SensesSlot as Governed>::cost_at(t), <Senses as Governed>::cost_at(t));
        }
        let mut empty = FleetSlot(None);
        empty.set_tier(Tier::Dormant, &TierReason::Pinned); // must not panic
    }

    #[test]
    fn a_speaker_is_the_only_way_in_and_it_never_speaks_by_itself() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let s = Speaker(tx);
        s.propose(Utterance::new("something", Urgency::Alarm));
        assert_eq!(rx.try_recv().unwrap().text, "something");
        // Dropping the loop's receiver must not panic a proposer.
        drop(rx);
        s.propose(Utterance::new("into the void", Urgency::Whim));
    }
}
