//! F37 — is the operator in flow?
//!
//! Nobody can answer that from the outside, so this does not pretend to. It
//! produces a **confidence**, shrunk toward a neutral prior when the evidence
//! is thin, and the budget treats "in flow" as a reason to *hold* chatter — an
//! `Answer` or an `Alarm` still goes through (`Urgency::breaks_flow`). A wrong
//! "they're in flow" costs a delayed remark; it must never cost a warning.
//!
//! ## What it reads, and why
//!
//! Everything comes from the closed [`Observation`] enum — there is no
//! keystroke sense, so *typing cadence* is inferred from the idle timer: the
//! idle sense resets whenever there is input, so a stream of `Idle` samples
//! whose `for_ms` keeps dropping back to near zero is someone typing.
//!
//! | Signal | Source | Reads as flow when |
//! |---|---|---|
//! | typing cadence | `Idle` resets/min, `Clipboard`, `Files` churn | high and sustained |
//! | focus purity | `Focus`/`Workspace` switch rate and app entropy | one app, few switches |
//! | settledness | how long the current focus has held | long *and* being typed into |
//! | music | `Media { playing: true }` | playing — a small nudge, not proof |
//! | time of day | host-supplied hour, `hour_bias` table | operator's own focus hours |
//! | away | `Idle { idle: true }` | never: away is certainty, not inference |
//! | on a call | `AudioLevel { mic_live: true }` | always, and it outranks even
//!   the idle timer — hands off the keyboard during a call is not "away" |
//!
//! Deliberately weak weights on music and time of day: they are priors about
//! people in general, and she is being told about *one* person.
//!
//! ## Openings
//!
//! The same stream produces [`Opportunity`] — the moments when speaking is
//! natural rather than intrusive. A held whim waits for one of these.
//!
//! No clock is read here. `now` is a parameter and the local hour is supplied
//! by the host, so a trace replays identically forever.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};
use wisp_proto::{Millis, Observation};

/// A moment when speaking up is natural rather than an interruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Opportunity {
    /// They stopped touching the machine.
    WentIdle,
    /// They came back from being away.
    CameBack,
    /// They moved to a different window or desktop.
    FocusChanged,
    /// Something they were waiting on finished — a build, a sync, a download.
    WorkFinished,
    /// The music stopped.
    MediaStopped,
    /// They spoke to her.
    Asked,
}

/// Everything the budget needs to know about *now*.
#[derive(Debug, Clone, PartialEq)]
pub struct Moment {
    /// Flow confidence, 0..=1.
    pub flow: f32,
    /// The most recent opening, and when it happened.
    pub opportunity: Option<(Opportunity, Millis)>,
    /// The governor said `Dormant`, or the operator muted her.
    pub silenced: bool,
}

impl Moment {
    /// Deep in something. No opening.
    pub fn busy() -> Self {
        Moment { flow: 1.0, opportunity: None, silenced: false }
    }
    /// Demonstrably calm, but nothing has just happened.
    pub fn free() -> Self {
        Moment { flow: 0.0, opportunity: None, silenced: false }
    }
    /// The opening, if there is one and it has not closed.
    pub fn opening_within(&self, now: Millis, window: Millis) -> Option<Opportunity> {
        match self.opportunity {
            Some((o, at)) if now.saturating_sub(at) <= window => Some(o),
            _ => None,
        }
    }
}

/// The full estimate, not just the number — so "why are you so sure?" is
/// answerable from data (SPEC §0.4).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Flow {
    pub confidence: f32,
    /// Sustained input, 0..=1.
    pub typing: f32,
    /// One app, few switches, 0..=1.
    pub purity: f32,
    /// How long the current focus has held, 0..=1.
    pub settled: f32,
    /// How much this estimate is worth: 0 with no observations, ->1 with many.
    pub evidence: f32,
    pub idle: bool,
    pub mic_live: bool,
    pub media_playing: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowConfig {
    /// Sliding window all the rates are measured over.
    pub window_ms: Millis,
    /// What she believes before she has seen anything. Below the budget's flow
    /// threshold on purpose: silence must be earned, not assumed.
    pub prior: f32,
    /// Observations needed for the estimate to be worth half its face value.
    pub evidence_half: f32,
    /// Idle for at least this long means away, whatever else the signals say.
    pub away_after_ms: Millis,
    /// Focus held this long counts as fully settled.
    pub long_focus_ms: Millis,
    /// Input resets per minute that count as full typing cadence.
    pub cadence_per_min: f32,
    /// Focus switches per minute that count as full churn.
    pub switches_per_min: f32,
    /// Weights for typing / purity / settledness.
    pub w_typing: f32,
    pub w_purity: f32,
    pub w_settled: f32,
    /// Music playing adds this much.
    pub music_bonus: f32,
    /// Confidence floor while the mic is live: she does not talk over a call.
    pub call_floor: f32,
    /// Additive nudge per local hour. Small on purpose.
    pub hour_bias: [f32; 24],
}

impl Default for FlowConfig {
    fn default() -> Self {
        let mut hour_bias = [0.0f32; 24];
        // Mid-morning and late night are when deep work usually happens; just
        // after lunch and early evening are when it usually does not. These are
        // weak priors that a skin or the operator can flatten to zero.
        for h in [9, 10, 11, 22, 23, 0, 1] {
            hour_bias[h] = 0.05;
        }
        for h in [12, 13, 18, 19] {
            hour_bias[h] = -0.05;
        }
        FlowConfig {
            window_ms: 300_000,
            prior: 0.35,
            evidence_half: 6.0,
            away_after_ms: 60_000,
            long_focus_ms: 600_000,
            cadence_per_min: 20.0,
            switches_per_min: 6.0,
            w_typing: 0.40,
            w_purity: 0.35,
            w_settled: 0.25,
            music_bonus: 0.08,
            call_floor: 0.95,
            hour_bias,
        }
    }
}

/// Rolling estimator. Feed it every [`Observation`]; ask it for a [`Moment`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowEstimator {
    cfg: FlowConfig,
    /// Times the idle timer was seen to reset — the typing-cadence proxy.
    resets: VecDeque<Millis>,
    /// Focus changes in the window, with the app that was moved to.
    switches: VecDeque<(Millis, String)>,
    focus_since: Millis,
    current_app: String,
    last_idle_for: u64,
    idle: bool,
    media_playing: bool,
    mic_live: bool,
    dirty_paths: std::collections::BTreeSet<String>,
    hour: u8,
    silenced: bool,
    opportunity: Option<(Opportunity, Millis)>,
    /// Observation timestamps, for the evidence weight.
    seen: VecDeque<Millis>,
}

impl Default for FlowEstimator {
    fn default() -> Self {
        FlowEstimator::new(FlowConfig::default())
    }
}

const MAX_SAMPLES: usize = 1024;

impl FlowEstimator {
    pub fn new(cfg: FlowConfig) -> Self {
        FlowEstimator {
            cfg,
            resets: VecDeque::new(),
            switches: VecDeque::new(),
            focus_since: 0,
            current_app: String::new(),
            last_idle_for: 0,
            idle: false,
            media_playing: false,
            mic_live: false,
            dirty_paths: Default::default(),
            hour: 12,
            silenced: false,
            opportunity: None,
            seen: VecDeque::new(),
        }
    }

    pub fn config(&self) -> &FlowConfig {
        &self.cfg
    }

    pub fn set_config(&mut self, cfg: FlowConfig) {
        self.cfg = cfg;
    }

    /// The host supplies the local hour; this crate never reads a clock.
    pub fn set_hour(&mut self, hour: u8) {
        self.hour = hour.min(23);
    }

    pub fn set_silenced(&mut self, silenced: bool) {
        self.silenced = silenced;
    }

    pub fn silenced(&self) -> bool {
        self.silenced
    }

    pub fn current_app(&self) -> &str {
        &self.current_app
    }

    pub fn is_idle(&self) -> bool {
        self.idle
    }

    pub fn mic_live(&self) -> bool {
        self.mic_live
    }

    pub fn media_playing(&self) -> bool {
        self.media_playing
    }

    /// How long the current window has held focus.
    pub fn focus_held(&self, now: Millis) -> Millis {
        now.saturating_sub(self.focus_since)
    }

    pub fn opportunity(&self) -> Option<(Opportunity, Millis)> {
        self.opportunity
    }

    /// Announce an opening by hand — the summon hotkey, a tool finishing.
    pub fn note_opportunity(&mut self, now: Millis, o: Opportunity) {
        self.opportunity = Some((o, now));
    }

    pub fn observe(&mut self, now: Millis, obs: &Observation) {
        self.prune(now);
        self.seen.push_back(now);
        if self.seen.len() > MAX_SAMPLES {
            self.seen.pop_front();
        }
        match obs {
            Observation::Idle { idle, for_ms } => {
                if *idle && !self.idle {
                    self.idle = true;
                    self.opportunity = Some((Opportunity::WentIdle, now));
                } else if !*idle {
                    if self.idle {
                        self.idle = false;
                        self.opportunity = Some((Opportunity::CameBack, now));
                    }
                    // The idle timer running backwards means input happened.
                    if *for_ms < self.last_idle_for || *for_ms <= 1_000 {
                        self.mark_input(now);
                    }
                }
                self.last_idle_for = *for_ms;
            }
            Observation::Focus { app_id, .. } => {
                if *app_id != self.current_app {
                    self.current_app = app_id.clone();
                    self.focus_since = now;
                    self.switches.push_back((now, app_id.clone()));
                    if self.switches.len() > MAX_SAMPLES {
                        self.switches.pop_front();
                    }
                    self.opportunity = Some((Opportunity::FocusChanged, now));
                }
            }
            Observation::Workspace { name, .. } => {
                // A desktop switch is a context switch: it counts as churn and
                // as an opening.
                self.switches.push_back((now, format!("desktop:{name}")));
                self.opportunity = Some((Opportunity::FocusChanged, now));
            }
            Observation::Media { playing, .. } => {
                if self.media_playing && !*playing {
                    self.opportunity = Some((Opportunity::MediaStopped, now));
                }
                self.media_playing = *playing;
            }
            Observation::AudioLevel { mic_live, .. } => {
                self.mic_live = *mic_live;
            }
            Observation::Notification { app, summary, body } => {
                if finished_something(app, summary, body) {
                    self.opportunity = Some((Opportunity::WorkFinished, now));
                }
            }
            Observation::Files { path, dirty } => {
                // Editing is input. A watched tree going clean is work finishing.
                if *dirty {
                    self.dirty_paths.insert(path.clone());
                    self.mark_input(now);
                } else if self.dirty_paths.remove(path) {
                    self.opportunity = Some((Opportunity::WorkFinished, now));
                }
            }
            Observation::Clipboard { .. } => self.mark_input(now),
            Observation::Speech { final_, .. } => {
                self.mark_input(now);
                if *final_ {
                    self.opportunity = Some((Opportunity::Asked, now));
                }
            }
            Observation::Window { .. } | Observation::Vitals { .. } | Observation::Fleet { .. } => {}
        }
    }

    fn mark_input(&mut self, now: Millis) {
        self.resets.push_back(now);
        if self.resets.len() > MAX_SAMPLES {
            self.resets.pop_front();
        }
    }

    fn prune(&mut self, now: Millis) {
        let cut = now.saturating_sub(self.cfg.window_ms);
        while self.resets.front().is_some_and(|t| *t < cut) {
            self.resets.pop_front();
        }
        while self.switches.front().is_some_and(|(t, _)| *t < cut) {
            self.switches.pop_front();
        }
        while self.seen.front().is_some_and(|t| *t < cut) {
            self.seen.pop_front();
        }
    }

    /// Shannon entropy of the focus distribution in the window, normalised to
    /// 0..=1. One app is 0; evenly split between many is 1.
    fn focus_entropy(&self) -> f32 {
        let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
        for (_, app) in &self.switches {
            *counts.entry(app.as_str()).or_insert(0) += 1;
        }
        let k = counts.len();
        if k < 2 {
            return 0.0;
        }
        let total: u32 = counts.values().sum();
        let mut h = 0.0f32;
        for c in counts.values() {
            let p = *c as f32 / total as f32;
            h -= p * p.log2();
        }
        (h / (k as f32).log2()).clamp(0.0, 1.0)
    }

    /// How much history the rates are actually divided by. Measured over what
    /// we have, not over the nominal window, so the first minute of a session
    /// is not read as "they never type" — floored at 15s so a single sample
    /// cannot produce an enormous rate.
    fn window_minutes(&self, now: Millis) -> f32 {
        let cut = now.saturating_sub(self.cfg.window_ms);
        let oldest = self
            .seen
            .iter()
            .chain(self.resets.iter())
            .copied()
            .filter(|t| *t >= cut)
            .min()
            .unwrap_or(now);
        let span = now.saturating_sub(oldest).min(self.cfg.window_ms);
        (span as f32 / 60_000.0).max(0.25)
    }

    /// The estimate, with its parts exposed.
    pub fn estimate(&self, now: Millis) -> Flow {
        let mins = self.window_minutes(now);
        let cut = now.saturating_sub(self.cfg.window_ms);
        let resets = self.resets.iter().filter(|t| **t >= cut).count() as f32;
        let switches = self.switches.iter().filter(|(t, _)| *t >= cut).count() as f32;
        let seen = self.seen.iter().filter(|t| **t >= cut).count() as f32;

        let typing = (resets / mins / self.cfg.cadence_per_min).clamp(0.0, 1.0);
        let churn = (switches / mins / self.cfg.switches_per_min).clamp(0.0, 1.0);
        let purity = 1.0 - churn.max(self.focus_entropy());
        let settled_raw =
            (self.focus_held(now) as f32 / self.cfg.long_focus_ms as f32).clamp(0.0, 1.0);
        // A window that has held focus for an hour with nobody typing into it is
        // an abandoned desk, not deep work.
        let settled = settled_raw * (0.3 + 0.7 * typing);

        let w = self.cfg.w_typing + self.cfg.w_purity + self.cfg.w_settled;
        let mut raw = if w > 0.0 {
            (self.cfg.w_typing * typing + self.cfg.w_purity * purity + self.cfg.w_settled * settled)
                / w
        } else {
            self.cfg.prior
        };
        if self.media_playing {
            raw += self.cfg.music_bonus;
        }
        raw += self.cfg.hour_bias[self.hour.min(23) as usize];
        let raw = raw.clamp(0.0, 1.0);

        // Shrink toward the prior by how much we have actually seen. Thin
        // evidence must not buy silence.
        let evidence = seen / (seen + self.cfg.evidence_half);
        let mut confidence = self.cfg.prior + (raw - self.cfg.prior) * evidence;

        // Two things we are not guessing about, in order: being away collapses
        // the estimate, and then a live mic overrides even that, because
        // sitting still on a call is not the same as having left the room.
        if self.idle {
            confidence = 0.0;
        }
        if self.mic_live {
            confidence = confidence.max(self.cfg.call_floor);
        }

        Flow {
            confidence: confidence.clamp(0.0, 1.0),
            typing,
            purity: purity.clamp(0.0, 1.0),
            settled,
            evidence,
            idle: self.idle,
            mic_live: self.mic_live,
            media_playing: self.media_playing,
        }
    }

    /// The estimate plus the current opening, ready for the budget.
    pub fn moment(&self, now: Millis) -> Moment {
        Moment {
            flow: self.estimate(now).confidence,
            opportunity: self.opportunity,
            silenced: self.silenced,
        }
    }
}

/// Does this notification look like something the operator was waiting on?
/// Keyword matching, deliberately: it is inspectable, and a false positive
/// only ever opens a *chance* to speak, never forces one.
fn finished_something(app: &str, summary: &str, body: &str) -> bool {
    const SUBJECT: &[&str] = &[
        "build", "cargo", "compil", "make", "test", "deploy", "sync", "backup", "download",
        "render", "export", "install", "update",
    ];
    const OUTCOME: &[&str] =
        &["finish", "complete", "done", "succeed", "success", "failed", "failure", "passed", "ready"];
    let hay = format!("{} {} {}", app.to_lowercase(), summary.to_lowercase(), body.to_lowercase());
    SUBJECT.iter().any(|k| hay.contains(k)) && OUTCOME.iter().any(|k| hay.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::isolate;

    fn kate() -> Observation {
        Observation::Focus { app_id: "org.kde.kate".into(), title: "flow.rs".into() }
    }

    fn typing(for_ms: u64) -> Observation {
        Observation::Idle { idle: false, for_ms }
    }

    /// Twenty minutes in one editor, typing steadily, music on.
    fn in_flow_trace(f: &mut FlowEstimator) -> Millis {
        f.set_hour(10);
        f.observe(0, &kate());
        f.observe(0, &Observation::Media {
            player: "mpv".into(),
            title: "brown noise".into(),
            artist: String::new(),
            playing: true,
        });
        let mut t = 0;
        while t < 1_200_000 {
            t += 2_000;
            f.observe(t, &typing(0));
        }
        t
    }

    /// Away from the desk.
    fn idle_trace(f: &mut FlowEstimator) -> Millis {
        f.set_hour(13);
        f.observe(0, &kate());
        f.observe(1_000, &typing(0));
        f.observe(300_000, &Observation::Idle { idle: true, for_ms: 300_000 });
        300_000
    }

    #[test]
    fn a_clearly_in_flow_trace_reads_as_flow() {
        isolate();
        let mut f = FlowEstimator::default();
        let t = in_flow_trace(&mut f);
        let e = f.estimate(t);
        assert!(e.confidence > 0.75, "{e:?}");
        assert!(e.typing > 0.9, "{e:?}");
        assert!(e.purity > 0.9, "{e:?}");
        assert!(e.settled > 0.9, "{e:?}");
        assert!(e.evidence > 0.9, "{e:?}");
        assert!(!e.idle);
    }

    #[test]
    fn a_clearly_idle_trace_reads_as_free() {
        isolate();
        let mut f = FlowEstimator::default();
        let t = idle_trace(&mut f);
        let e = f.estimate(t);
        assert_eq!(e.confidence, 0.0, "{e:?}");
        assert!(e.idle);
        assert_eq!(f.opportunity().map(|(o, _)| o), Some(Opportunity::WentIdle));
    }

    #[test]
    fn coming_back_is_an_opening_and_clears_away() {
        isolate();
        let mut f = FlowEstimator::default();
        let t = idle_trace(&mut f);
        f.observe(t + 1_000, &typing(0));
        assert!(!f.is_idle());
        assert_eq!(f.opportunity(), Some((Opportunity::CameBack, t + 1_000)));
    }

    #[test]
    fn window_thrashing_does_not_read_as_flow() {
        isolate();
        let mut f = FlowEstimator::default();
        f.set_hour(15);
        let apps = ["firefox", "kate", "konsole", "dolphin", "discord", "steam"];
        let mut t = 0;
        for i in 0..60 {
            t += 10_000;
            f.observe(t, &Observation::Focus {
                app_id: apps[i % apps.len()].into(),
                title: format!("window {i}"),
            });
            f.observe(t + 1_000, &typing(0));
        }
        let e = f.estimate(t + 2_000);
        assert!(e.purity < 0.1, "{e:?}");
        assert!(e.confidence < 0.45, "thrashing scored {e:?}");
    }

    #[test]
    fn thin_evidence_stays_near_the_prior() {
        isolate();
        let mut f = FlowEstimator::default();
        f.set_hour(15);
        f.observe(0, &kate());
        let e = f.estimate(1_000);
        assert!(
            (e.confidence - f.config().prior).abs() < 0.15,
            "one observation should not buy silence: {e:?}"
        );
        assert!(e.confidence < 0.6, "and it must stay under the budget's threshold");
        // With nothing at all seen, it is exactly the prior.
        let fresh = FlowEstimator::default();
        assert_eq!(fresh.estimate(0).confidence, FlowConfig::default().prior);
        assert_eq!(fresh.estimate(0).evidence, 0.0);
    }

    #[test]
    fn an_abandoned_desk_is_not_deep_work() {
        isolate();
        // Focus held for an hour, but nobody has typed in the last five minutes
        // and the idle sense never fired (a stuck sense, a video playing).
        let mut f = FlowEstimator::default();
        f.set_hour(15);
        f.observe(0, &kate());
        for i in 1..=20 {
            f.observe(i * 1_000, &typing(0));
        }
        let e = f.estimate(3_600_000);
        assert!(e.settled < 0.35, "settledness must lean on real input: {e:?}");
        assert!(e.confidence < 0.6, "{e:?}");
    }

    #[test]
    fn a_live_mic_is_treated_as_do_not_interrupt() {
        isolate();
        let mut f = FlowEstimator::default();
        f.observe(0, &Observation::AudioLevel { out: 20, mic_live: true });
        let e = f.estimate(1_000);
        assert!(e.confidence >= 0.95, "{e:?}");
        assert!(e.mic_live);
        // ...and it lifts as soon as the mic goes quiet.
        f.observe(2_000, &Observation::AudioLevel { out: 20, mic_live: false });
        assert!(f.estimate(3_000).confidence < 0.6);
    }

    #[test]
    fn being_away_beats_every_signal_except_a_live_mic() {
        isolate();
        let mut f = FlowEstimator::default();
        let t = in_flow_trace(&mut f);
        assert!(f.estimate(t).confidence > 0.75);
        f.observe(t + 1_000, &Observation::Idle { idle: true, for_ms: 120_000 });
        assert_eq!(f.estimate(t + 2_000).confidence, 0.0, "away is certainty, not inference");

        // ...but hands off the keyboard *during a call* is not being away, and
        // it is exactly when she must not chirp.
        f.observe(t + 3_000, &Observation::AudioLevel { out: 0, mic_live: true });
        assert!(f.estimate(t + 4_000).confidence >= 0.95);
        f.observe(t + 5_000, &Observation::AudioLevel { out: 0, mic_live: false });
        assert_eq!(f.estimate(t + 6_000).confidence, 0.0);
    }

    #[test]
    fn music_is_a_nudge_not_a_verdict() {
        isolate();
        let mut quiet = FlowEstimator::default();
        let mut loud = FlowEstimator::default();
        quiet.set_hour(15);
        loud.set_hour(15);
        loud.observe(0, &Observation::Media {
            player: "mpv".into(),
            title: "x".into(),
            artist: "y".into(),
            playing: true,
        });
        for f in [&mut quiet, &mut loud] {
            f.observe(0, &kate());
            for i in 1..=100 {
                f.observe(i * 2_000, &typing(0));
            }
        }
        let d = loud.estimate(200_000).confidence - quiet.estimate(200_000).confidence;
        assert!(d > 0.0 && d < 0.1, "music moved flow by {d}");
    }

    #[test]
    fn openings_are_detected_from_the_stream() {
        isolate();
        let mut f = FlowEstimator::default();
        f.observe(0, &kate());
        assert_eq!(f.opportunity(), Some((Opportunity::FocusChanged, 0)));
        f.observe(1_000, &Observation::Notification {
            app: "cargo".into(),
            summary: "build finished".into(),
            body: "in 41s".into(),
        });
        assert_eq!(f.opportunity(), Some((Opportunity::WorkFinished, 1_000)));
        f.observe(2_000, &Observation::Media {
            player: "mpv".into(),
            title: "t".into(),
            artist: "a".into(),
            playing: true,
        });
        f.observe(3_000, &Observation::Media {
            player: "mpv".into(),
            title: "t".into(),
            artist: "a".into(),
            playing: false,
        });
        assert_eq!(f.opportunity(), Some((Opportunity::MediaStopped, 3_000)));
        f.observe(4_000, &Observation::Speech { text: "hey".into(), final_: true });
        assert_eq!(f.opportunity(), Some((Opportunity::Asked, 4_000)));
        f.observe(5_000, &Observation::Files { path: "/src".into(), dirty: true });
        f.observe(6_000, &Observation::Files { path: "/src".into(), dirty: false });
        assert_eq!(f.opportunity(), Some((Opportunity::WorkFinished, 6_000)));
    }

    #[test]
    fn ordinary_notifications_are_not_openings() {
        isolate();
        let mut f = FlowEstimator::default();
        f.observe(0, &Observation::Notification {
            app: "discord".into(),
            summary: "someone said hi".into(),
            body: "in a channel".into(),
        });
        assert_eq!(f.opportunity(), None, "every ping must not become a chance to chatter");
    }

    #[test]
    fn openings_close() {
        isolate();
        let mut f = FlowEstimator::default();
        f.observe(0, &kate());
        let m = f.moment(0);
        assert_eq!(m.opening_within(0, 45_000), Some(Opportunity::FocusChanged));
        assert_eq!(m.opening_within(45_001, 45_000), None);
    }

    #[test]
    fn the_same_trace_gives_the_same_estimate() {
        isolate();
        let run = || {
            let mut f = FlowEstimator::default();
            let t = in_flow_trace(&mut f);
            f.estimate(t)
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn state_round_trips_through_serde() {
        isolate();
        let mut f = FlowEstimator::default();
        let t = in_flow_trace(&mut f);
        let json = serde_json::to_string(&f).unwrap();
        let back: FlowEstimator = serde_json::from_str(&json).unwrap();
        assert_eq!(back.estimate(t), f.estimate(t));
    }

    #[test]
    fn history_stays_bounded() {
        isolate();
        let mut f = FlowEstimator::default();
        let mut t = 0;
        for _ in 0..20_000 {
            t += 100;
            f.observe(t, &typing(0));
        }
        assert!(f.resets.len() <= MAX_SAMPLES);
        assert!(f.seen.len() <= MAX_SAMPLES);
    }
}
