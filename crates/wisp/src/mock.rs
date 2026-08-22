//! **`--mock` — the whole loop, with no compositor, no GPU and no machine.**
//!
//! SPEC §4 asks for a mock inference backend so the suite never needs a model.
//! The same argument applies one level up: the *wiring* is the part of this
//! crate most likely to be wrong, and it is the part hardest to test, because
//! exercising it normally means a KWin session, a discrete GPU and an NX Hub.
//! So `--mock` replaces exactly two things and nothing else:
//!
//! * the **snapshot source** the governor probes, with a scripted trajectory
//!   ([`Trajectory`]) built from `wisp-gov`'s own [`fakes::Machine`];
//! * the **senses**, with plugins that emit scripted [`Observation`]s.
//!
//! Everything else is the real thing. In particular the consent layer is *not*
//! mocked: a mock sense obtains its [`SenseHandle`] from the real
//! [`ConsentLedger`], through the real [`ConsentLedger::grant`], and is refused
//! if the operator has that sense switched off. A CI run therefore exercises
//! the actual consent gate rather than a version of it that always says yes.
//!
//! [`fakes::Machine`]: wisp_gov::fakes::Machine
//! [`ConsentLedger`]: wisp_senses::ConsentLedger
//! [`ConsentLedger::grant`]: wisp_senses::ConsentLedger::grant
//! [`SenseHandle`]: wisp_senses::SenseHandle

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use wisp_gov::fakes::Machine;
use wisp_gov::probe::SnapshotSource;
use wisp_gov::{GovConfig, Governor, Snapshot};
use wisp_proto::{Observation, SenseId};
use wisp_senses::consent::{Sense, SenseCtx, SenseHandle, SensePlugin};

/// How fast a mock sense publishes. Fast enough that a two-second CI run has
/// plenty of trace, slow enough not to be a busy loop.
pub const TICK: Duration = Duration::from_millis(120);

// ---------------------------------------------------------------------------
// A scripted machine
// ---------------------------------------------------------------------------

/// A loop of snapshots that walks the tier ladder from end to end.
///
/// Deliberately a *story*, not noise: quiet desktop, the operator wanders off
/// (T0), they come back (T1), a compile starts (T2), a game launches (T3), a
/// headset connects, everything stops. A mock run that never leaves T1 would
/// test the loop's easy half only.
pub fn trajectory() -> Vec<Snapshot> {
    let base = Machine::desktop();
    let mut out = Vec::new();
    let mut at = 0u64;
    let push = |m: Machine, out: &mut Vec<Snapshot>, at: &mut u64| {
        *at += 1_000;
        out.push(m.at(*at).build());
    };

    // Quiet, operator present.
    for _ in 0..3 {
        push(base.clone(), &mut out, &mut at);
    }
    // They wander off.
    for _ in 0..3 {
        push(base.clone().idle_ms(300_000), &mut out, &mut at);
    }
    // Back at the keyboard.
    for _ in 0..2 {
        push(base.clone().idle_ms(0), &mut out, &mut at);
    }
    // A compile.
    for _ in 0..3 {
        push(base.clone().load(24.0).cpu_psi(30.0).top_cpu("cc1plus", 2_400), &mut out, &mut at);
    }
    // A game takes the GPU.
    for _ in 0..3 {
        push(
            base.clone().game("some-game").fullscreen("steam_app_1").gpu_busy(96),
            &mut out,
            &mut at,
        );
    }
    // A headset connects.
    for _ in 0..3 {
        push(base.clone().wivrn_streaming().gpu_busy(90), &mut out, &mut at);
    }
    // And back to quiet.
    for _ in 0..3 {
        push(base.clone(), &mut out, &mut at);
    }
    out
}

/// The scripted trajectory, on repeat. Unlike `wisp_gov::fakes::Replay`, which
/// holds the last frame forever, this wraps around — a long mock run keeps
/// moving instead of settling.
#[derive(Debug, Clone)]
pub struct Trajectory {
    frames: Vec<Snapshot>,
    at: usize,
    /// Frames are authored a second apart; a mock run advances faster than
    /// that, so the timestamps are rewritten as they are handed out. Without
    /// this the ladder's dwell timers would never expire.
    now: u64,
    step_ms: u64,
}

impl Trajectory {
    pub fn new(frames: Vec<Snapshot>, step_ms: u64) -> Self {
        assert!(!frames.is_empty(), "a trajectory needs at least one frame");
        Trajectory { frames, at: 0, now: 0, step_ms }
    }

    pub fn default_loop() -> Self {
        Trajectory::new(trajectory(), MOCK_STEP_MS)
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

impl SnapshotSource for Trajectory {
    fn snapshot(&mut self) -> Snapshot {
        let mut s = self.frames[self.at % self.frames.len()].clone();
        self.at += 1;
        self.now += self.step_ms;
        s.at = self.now;
        s
    }
}

/// A governor reading the scripted machine instead of this one.
///
/// The **dwell timers are the real ones**, so hysteresis is genuinely
/// exercised rather than switched off — a mock that could not flap would not
/// prove the loop copes when the real one does. What is compressed is time:
/// the trajectory hands out timestamps [`MOCK_STEP_MS`] apart and the cadence
/// is [`MOCK_POLL_MS`], so a two-second CI run covers a couple of minutes of
/// scripted machine and walks most of the ladder.
pub fn governor() -> Governor {
    let cfg = GovConfig {
        cadence: wisp_gov::Cadence {
            t0_ms: MOCK_POLL_MS,
            t1_ms: MOCK_POLL_MS,
            t2_ms: MOCK_POLL_MS,
            t3_ms: MOCK_POLL_MS,
            t4_ms: MOCK_POLL_MS,
        },
        ..GovConfig::default()
    };
    Governor::with_source(cfg, Box::new(Trajectory::default_loop()))
}

/// Real milliseconds between mock polls.
pub const MOCK_POLL_MS: u64 = 100;
/// Scripted milliseconds each mock frame advances the machine's clock. Larger
/// than the dwell timers' granularity, so `to_full_ms` and friends are reached
/// within a phase of the trajectory rather than never.
pub const MOCK_STEP_MS: u64 = 5_000;

/// A governor with no hysteresis at all, for tests that want the tier to move
/// on the very next step.
pub fn governor_instant() -> Governor {
    Governor::with_source(GovConfig::instant(), Box::new(Trajectory::default_loop()))
}

// ---------------------------------------------------------------------------
// Scripted senses
// ---------------------------------------------------------------------------

/// Shared step counter, so the mock senses tell one coherent story rather than
/// each drifting on its own timer.
#[derive(Debug, Clone, Default)]
pub struct Beat(Arc<AtomicU64>);

impl Beat {
    pub fn new() -> Self {
        Beat::default()
    }
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
    pub fn count(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// The body every mock sense shares: publish, wait, repeat, stop on shutdown.
///
/// `emit` logs and swallows a refusal, which is the correct behaviour when the
/// operator revokes consent mid-run: the sense stops being heard, and keeps
/// running harmlessly until it is asked to stop.
macro_rules! mock_sense {
    ($name:ident, $id:expr, $make:expr) => {
        pub struct $name {
            pub beat: Beat,
            pub tick: Duration,
        }

        impl $name {
            pub fn new(beat: Beat) -> Self {
                $name { beat, tick: TICK }
            }
        }

        impl Sense for $name {
            const ID: SenseId = $id;
            const LABEL: &'static str = wisp_senses::consent::label_of($id);
            const DESCRIPTION: &'static str = wisp_senses::consent::description_of($id);
        }

        impl SensePlugin for $name {
            fn spawn(self, handle: SenseHandle<Self>, ctx: SenseCtx) -> tokio::task::JoinHandle<()> {
                let mut shutdown = ctx.shutdown.clone();
                tokio::spawn(async move {
                    let make: fn(u64) -> Option<Observation> = $make;
                    loop {
                        let n = self.beat.next();
                        if let Some(obs) = make(n) {
                            handle.emit(obs);
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(self.tick) => {}
                            _ = shutdown.wait() => return,
                        }
                    }
                })
            }
        }
    };
}

mock_sense!(MockIdle, SenseId::Idle, |n| {
    // Away for a stretch, then back. The ladder cares about this.
    let idle = (n / 8) % 2 == 1;
    Some(Observation::Idle { idle, for_ms: if idle { (n % 8) * 30_000 } else { 0 } })
});

mock_sense!(MockFocus, SenseId::ActiveWindow, |n| {
    const APPS: [(&str, &str); 4] = [
        ("org.kde.kate", "lib.rs"),
        ("org.kde.konsole", "cargo test"),
        ("firefox", "nx-wisp \u{2014} docs"),
        ("steam", "Library"),
    ];
    // Not every beat: focus that changed four times a second would swamp the
    // flow estimator and she would never look settled.
    if n % 5 != 0 {
        return None;
    }
    let (app, title) = APPS[(n / 5) as usize % APPS.len()];
    Some(Observation::Focus { app_id: app.to_string(), title: title.to_string() })
});

mock_sense!(MockVitals, SenseId::Vitals, |n| {
    if n % 8 != 0 {
        return None;
    }
    let cpu = ((n * 7) % 90) as u8;
    Some(Observation::Vitals {
        cpu_pct: cpu,
        gpu_pct: (cpu / 3),
        vram_used_mib: 512 + (n % 8) * 128,
        temp_c: 45 + (cpu / 10),
        on_battery: false,
    })
});

mock_sense!(MockWorkspace, SenseId::Workspace, |n| {
    if n % 17 != 0 {
        return None;
    }
    const NAMES: [&str; 3] = ["code", "web", "games"];
    let i = (n / 17) as usize % NAMES.len();
    Some(Observation::Workspace { index: i as u32 + 1, name: NAMES[i].to_string() })
});

mock_sense!(MockMedia, SenseId::Media, |n| {
    if n % 23 != 0 {
        return None;
    }
    Some(Observation::Media {
        player: "mock-player".to_string(),
        title: format!("track {}", n / 23),
        artist: "nobody".to_string(),
        playing: (n / 23) % 2 == 0,
    })
});

mock_sense!(MockNotifications, SenseId::Notifications, |n| {
    if n % 31 != 0 {
        return None;
    }
    Some(Observation::Notification {
        app: "mock".to_string(),
        summary: format!("something happened ({})", n / 31),
        body: String::new(),
    })
});

/// Start every mock sense the operator has left enabled.
///
/// `try_start` swallows "not enabled", so switching a sense off in
/// `senses.json` really does silence it here, exactly as it would in a live
/// run. That is the property worth having: `--mock` tests the consent gate, it
/// does not bypass it.
pub fn start_all(senses: &mut wisp_senses::Senses) -> Beat {
    let beat = Beat::new();
    senses.try_start(MockIdle::new(beat.clone()));
    senses.try_start(MockFocus::new(beat.clone()));
    senses.try_start(MockVitals::new(beat.clone()));
    senses.try_start(MockWorkspace::new(beat.clone()));
    senses.try_start(MockMedia::new(beat.clone()));
    senses.try_start(MockNotifications::new(beat.clone()));
    beat
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempConfig;
    use wisp_proto::Tier;

    #[test]
    fn the_trajectory_visits_every_tier_the_ladder_can_reach() {
        let _tmp = TempConfig::new();
        let mut gov = governor();
        let mut seen = std::collections::BTreeSet::new();
        // Two laps: dwell timers mean the first lap is spent climbing.
        for _ in 0..(trajectory().len() * 3) {
            let step = gov.step();
            seen.insert(step.tier);
        }
        assert!(seen.contains(&Tier::Full), "{seen:?}");
        assert!(seen.contains(&Tier::Lobotomised), "a game must reach T3: {seen:?}");
        assert!(seen.len() >= 3, "the mock machine barely moves: {seen:?}");
    }

    #[test]
    fn the_trajectory_wraps_and_its_clock_only_moves_forward() {
        let mut t = Trajectory::default_loop();
        let n = t.len();
        let mut last = 0;
        for _ in 0..(n * 2 + 3) {
            let s = t.snapshot();
            assert!(s.at > last, "the mock clock went backwards");
            last = s.at;
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn mock_senses_publish_through_the_real_consent_gate() {
        let tmp = TempConfig::new();
        let (tx, mut rx) = tokio::sync::broadcast::channel(256);
        let mut senses = wisp_senses::Senses::with_bus(tx, wisp_senses::Clock::new());
        let beat = start_all(&mut senses);

        tokio::time::sleep(TICK * 40).await;
        assert!(beat.count() > 0);

        let mut kinds = std::collections::BTreeSet::new();
        while let Ok(ev) = rx.try_recv() {
            if let wisp_proto::EventKind::Sensed(o) = ev.kind {
                kinds.insert(format!("{:?}", o.sense()));
            }
        }
        assert!(kinds.contains("Idle"), "{kinds:?}");
        assert!(kinds.contains("ActiveWindow"), "{kinds:?}");
        assert!(kinds.contains("Vitals"), "{kinds:?}");
        senses.shutdown().await;
        let _ = tmp;
    }

    /// An invasive sense is off by default and stays off in `--mock`. There is
    /// no mock clipboard sense at all, and if one were added it would still
    /// have to go through `grant`.
    #[tokio::test(flavor = "current_thread")]
    async fn mock_mode_never_switches_on_an_invasive_sense() {
        let tmp = TempConfig::new();
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let mut senses = wisp_senses::Senses::with_bus(tx, wisp_senses::Clock::new());
        start_all(&mut senses);
        let rows = senses.ledger().rows();
        for r in rows {
            if r.consent == wisp_proto::Consent::Invasive {
                assert!(!r.enabled, "{:?} is enabled in a mock run", r.id);
                assert!(!r.live, "{:?} is live in a mock run", r.id);
            }
        }
        senses.shutdown().await;
        let _ = tmp;
    }

    /// Switching a sense off in the ledger really silences the mock sense.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_disabled_sense_stays_quiet_in_mock_mode() {
        let tmp = TempConfig::new();
        crate::config::set_sense_enabled(tmp.path(), SenseId::Vitals, false).unwrap();

        let (tx, mut rx) = tokio::sync::broadcast::channel(256);
        let mut senses = wisp_senses::Senses::with_bus(tx, wisp_senses::Clock::new());
        start_all(&mut senses);
        tokio::time::sleep(TICK * 40).await;

        let mut saw_vitals = false;
        while let Ok(ev) = rx.try_recv() {
            if let wisp_proto::EventKind::Sensed(o) = ev.kind {
                saw_vitals |= o.sense() == SenseId::Vitals;
            }
        }
        assert!(!saw_vitals, "a sense the operator switched off still published");
        senses.shutdown().await;
    }
}
