//! F33's second half — she stops talking when the operator starts.
//!
//! A pure state machine. No clock, no thread, no device: the caller passes
//! `now` and every decision below is a function of the signals it has been fed
//! since the current utterance began. That is not an aesthetic preference. The
//! two bugs this module exists to prevent are both *timing* bugs, and a timing
//! bug you cannot write a test for is a timing bug you ship.
//!
//! ```text
//!   speaking_started(t0)
//!        │
//!        ├── t0 .. t0+grace ── everything except Explicit/PushToTalk ignored
//!        │                      (this is where she would interrupt herself)
//!        │
//!        ├── Keystroke ×N within a window ──▶ Some(Typing)
//!        ├── MicLevel over threshold for M ms ──▶ Some(Speech)
//!        ├── PushToTalk / Explicit ──▶ immediately, grace or no grace
//!        └── first cancel wins; everything after it is None
//!
//!   speaking_stopped(t1)  ── and every accumulator is dropped on the floor
//! ```
//!
//! ## The classic bug: she interrupts herself
//!
//! Her own voice comes out of the speakers and straight back into the
//! microphone. Without a grace window she hears herself say the first syllable,
//! decides the operator is talking, and cancels — every single time, so
//! reliably that mic barge-in appears to be completely broken rather than
//! subtly wrong.
//!
//! Three things stop it, and all three are needed:
//!
//! 1. **A grace window** at the start of the utterance, during which level
//!    signals are ignored outright. It has to outlast the acoustic round trip
//!    plus her first phoneme.
//! 2. **A level threshold** well above her own bleed, for the rest of the
//!    utterance. [`crate::duck::Ducker`] deliberately does *not* duck her own
//!    output stream, which is what makes that bleed a roughly constant level
//!    rather than something that changes with whatever else is playing.
//! 3. **A sustain requirement**: the level has to stay up for
//!    [`BargePolicy::mic_sustain_ms`]. A consonant of hers that clips the
//!    threshold for one frame is not a person starting a sentence.
//!
//! The real fix is acoustic echo cancellation, which is a `wisp-senses`
//! problem and not a small one. Until then this is the cheap ninety percent —
//! and it is also why [`BargePolicy::mic_enabled`] is **off by default**,
//! quite apart from the microphone being opt-in and invasive under SPEC §3.7.
//!
//! ## The other classic bug: one keystroke kills her
//!
//! Somebody hits `Ctrl` on the way to a shortcut, or the window manager sends a
//! spurious key event, and she stops mid-word. So typing needs
//! [`BargePolicy::keystrokes`] of them inside
//! [`BargePolicy::keystroke_window_ms`] — and *key autorepeat is not typing*.
//! A held-down arrow key fires thirty events a second and would clear any
//! threshold instantly, so events closer together than
//! [`BargePolicy::key_repeat_ms`] are treated as one.
//!
//! ## Nothing accumulates across utterances
//!
//! [`BargeIn::speaking_started`] clears every counter. A keystroke from thirty
//! seconds ago must not cancel the sentence she is starting now, and a cancel
//! is one per utterance — after it fires, further signals return `None` until
//! the next [`BargeIn::speaking_started`]. Without that, a caller that reacts to
//! `Some(_)` by tearing down the play queue would get a second teardown for the
//! next keystroke of the same burst.

use crate::Millis;

/// Something that might mean "stop talking".
///
/// Deliberately not `Copy`-cheap events from one source: they come from four
/// different places (the input sense, the mic level meter, a global shortcut,
/// and the shell) and each has its own idea of how urgent it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BargeSignal {
    /// The operator pressed a key. One is noise; several are a decision.
    Keystroke,
    /// A microphone level reading, linear peak `0.0..=1.0`. Only meaningful if
    /// the operator has the microphone on, which is not the default.
    MicLevel { peak: f32 },
    /// The push-to-talk key went down. Unambiguous: nobody holds that key by
    /// accident.
    PushToTalk,
    /// They asked her to stop — the shell's stop button, or the escape key
    /// bound to it.
    Explicit,
    /// The focused window changed.
    FocusChanged,
}

/// Why she stopped. Carried into `EventKind` so the shell can say so, and kept
/// distinct per source because "you started typing" and "the governor took the
/// machine away" deserve different faces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    Typing,
    Speech,
    PushToTalk,
    Explicit,
    Focus,
    /// SPEC §3.1 — a downgrade sheds work in flight. Not a [`BargeSignal`]
    /// because it does not come from the operator; see
    /// [`BargeIn::cancel_for_tier`].
    Tier,
}

impl CancelReason {
    /// Did the operator mean to do this? Used to decide whether she says
    /// anything about having been cut off.
    pub fn was_deliberate(self) -> bool {
        matches!(self, CancelReason::Explicit | CancelReason::PushToTalk)
    }
}

/// Thresholds, and which sources are live at all.
#[derive(Debug, Clone, PartialEq)]
pub struct BargePolicy {
    /// How long after she starts speaking before level and typing signals are
    /// listened to at all. See the module docs: this is the difference between
    /// working barge-in and barge-in that appears not to exist.
    pub grace_ms: u32,
    /// Keystrokes needed inside `keystroke_window_ms`. Clamped to at least 1.
    pub keystrokes: u32,
    pub keystroke_window_ms: u32,
    /// Keystrokes closer together than this are autorepeat, and count once.
    /// X11 defaults to a 25 ms repeat interval and Wayland compositors are in
    /// the same range, so this sits just above it.
    pub key_repeat_ms: u32,
    /// Linear peak the microphone must exceed. Above her own bleed, not above
    /// the noise floor.
    pub mic_threshold: f32,
    /// …and stay above, for this long, before it counts as somebody talking.
    pub mic_sustain_ms: u32,
    /// A dip below the threshold longer than this ends the run. Sized to be
    /// longer than the gap between two syllables and shorter than the gap
    /// between two sentences.
    pub mic_gap_ms: u32,
    pub typing_enabled: bool,
    /// **Off by default.** The microphone is opt-in (SPEC §3.7) and, until
    /// there is echo cancellation, level-based barge-in is the feature most
    /// likely to misfire.
    pub mic_enabled: bool,
    pub push_to_talk_enabled: bool,
    /// **Off by default.** A notification popup, a tooltip, or a tray menu all
    /// generate focus changes on KDE, and none of them mean the operator has
    /// stopped listening.
    pub focus_enabled: bool,
}

impl Default for BargePolicy {
    fn default() -> Self {
        BargePolicy {
            // Speaker to microphone across a desk is a couple of milliseconds;
            // the rest of this is her first syllable plus the level meter's own
            // smoothing. Short enough that a genuine interruption in the first
            // half-second is only slightly late.
            grace_ms: 400,
            keystrokes: 3,
            keystroke_window_ms: 900,
            key_repeat_ms: 40,
            // About -18 dBFS. Her own voice arrives at a desk mic well below
            // this; a person speaking into it does not.
            mic_threshold: 0.12,
            mic_sustain_ms: 250,
            mic_gap_ms: 150,
            typing_enabled: true,
            mic_enabled: false,
            push_to_talk_enabled: true,
            focus_enabled: false,
        }
    }
}

impl BargePolicy {
    /// Everything on. What the operator gets after they have enabled the
    /// microphone and asked for focus-follows-attention — not a default.
    pub fn everything() -> Self {
        BargePolicy {
            mic_enabled: true,
            focus_enabled: true,
            ..BargePolicy::default()
        }
    }

    /// Nothing but the two signals that are unambiguously a person: the stop
    /// button and push-to-talk. The safe fallback if the input sense is not
    /// consented to.
    pub fn deliberate_only() -> Self {
        BargePolicy {
            typing_enabled: false,
            mic_enabled: false,
            focus_enabled: false,
            ..BargePolicy::default()
        }
    }

    fn keystrokes_needed(&self) -> u32 {
        self.keystrokes.max(1)
    }
}

/// Decides whether to stop her, from signals and a clock the caller owns.
#[derive(Debug, Clone)]
pub struct BargeIn {
    policy: BargePolicy,
    /// `None` when she is not speaking. Signals are only ever meaningful
    /// against a live utterance.
    speaking_since: Option<Millis>,
    /// Set once per utterance. The first cancel wins and the rest are `None`,
    /// so a caller can tear the play queue down exactly once.
    cancelled: Option<CancelReason>,
    /// Accepted (non-autorepeat) keystroke times inside the window.
    keys: Vec<Millis>,
    /// When the last keystroke *event* arrived, accepted or not. Autorepeat has
    /// to be measured against the event stream rather than against the last
    /// accepted key: a 30 ms repeat compared to the last *accepted* one clears
    /// a 40 ms debounce every other event, which lets a held key spell out a
    /// cancel at half speed.
    last_key_at: Option<Millis>,
    /// When the current above-threshold run began.
    mic_run_since: Option<Millis>,
    /// Last reading that was above threshold, so a dip can end the run.
    mic_last_over: Millis,
    /// How many utterances this has cancelled. For the consent panel's "she
    /// stopped for you N times" and for spotting a threshold that is too low.
    pub cancels: u64,
}

impl Default for BargeIn {
    fn default() -> Self {
        BargeIn::new(BargePolicy::default())
    }
}

impl BargeIn {
    pub fn new(policy: BargePolicy) -> Self {
        BargeIn {
            policy,
            speaking_since: None,
            cancelled: None,
            keys: Vec::new(),
            last_key_at: None,
            mic_run_since: None,
            mic_last_over: 0,
            cancels: 0,
        }
    }

    pub fn policy(&self) -> &BargePolicy {
        &self.policy
    }

    /// Change the policy — the operator flipped the microphone on mid-session.
    /// Clears the accumulators, because a run measured against the old
    /// threshold means nothing against the new one.
    pub fn set_policy(&mut self, policy: BargePolicy) {
        self.policy = policy;
        self.clear_accumulators();
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking_since.is_some()
    }

    /// Why the current utterance was cancelled, if it was.
    pub fn cancelled(&self) -> Option<CancelReason> {
        self.cancelled
    }

    /// How long she has been talking. `None` if she is not.
    pub fn speaking_for(&self, now: Millis) -> Option<Millis> {
        self.speaking_since.map(|t| now.saturating_sub(t))
    }

    /// Are we still inside the window where she would hear herself?
    pub fn in_grace(&self, now: Millis) -> bool {
        match self.speaking_since {
            None => false,
            Some(t) => now.saturating_sub(t) < self.policy.grace_ms as Millis,
        }
    }

    /// A new utterance begins. **Clears every accumulator**, which is the whole
    /// mechanism behind "signals do not survive from one sentence to the next".
    pub fn speaking_started(&mut self, now: Millis) {
        self.speaking_since = Some(now);
        self.cancelled = None;
        self.clear_accumulators();
    }

    /// She finished, or the caller acted on a cancel. Signals after this do
    /// nothing at all: there is nothing left to interrupt.
    pub fn speaking_stopped(&mut self, _now: Millis) {
        self.speaking_since = None;
        self.clear_accumulators();
    }

    /// Feed one signal. `Some(reason)` means *stop now*, exactly once.
    pub fn observe(&mut self, sig: BargeSignal, now: Millis) -> Option<CancelReason> {
        // Not speaking, or already cancelled: nothing to interrupt, and nothing
        // worth remembering for later either. Recording it would be how a
        // keystroke from before the utterance ends up cancelling it.
        if self.speaking_since.is_none() || self.cancelled.is_some() {
            return None;
        }

        let reason = match sig {
            // The two immediate ones. Neither waits for grace: a person holding
            // the push-to-talk key or hitting the stop button has already made
            // the decision this module is otherwise trying to infer, and making
            // them press it twice because she only just started is the rudest
            // possible behaviour.
            BargeSignal::Explicit => Some(CancelReason::Explicit),
            BargeSignal::PushToTalk if self.policy.push_to_talk_enabled => {
                Some(CancelReason::PushToTalk)
            }
            BargeSignal::PushToTalk => None,

            BargeSignal::Keystroke => self.on_keystroke(now),
            BargeSignal::MicLevel { peak } => self.on_mic(peak, now),
            BargeSignal::FocusChanged => self.on_focus(now),
        };

        if let Some(r) = reason {
            self.cancelled = Some(r);
            self.cancels += 1;
            tracing::debug!(reason = ?r, "barge-in: stopping mid-utterance");
        }
        reason
    }

    /// The governor's path. SPEC §3.1: a downgrade is synchronous and work is
    /// shed, so this ignores grace, policy and every accumulator — but it is
    /// still one cancel per utterance, so a tier drop while she is already
    /// stopping does not tear the queue down twice.
    pub fn cancel_for_tier(&mut self, _now: Millis) -> Option<CancelReason> {
        if self.speaking_since.is_none() || self.cancelled.is_some() {
            return None;
        }
        self.cancelled = Some(CancelReason::Tier);
        self.cancels += 1;
        Some(CancelReason::Tier)
    }

    /// Back to the state a fresh [`BargeIn`] is in, keeping the policy and the
    /// counter.
    pub fn reset(&mut self) {
        self.speaking_since = None;
        self.cancelled = None;
        self.clear_accumulators();
    }

    // -- per-source rules ---------------------------------------------------

    fn clear_accumulators(&mut self) {
        self.keys.clear();
        self.last_key_at = None;
        self.mic_run_since = None;
        self.mic_last_over = 0;
    }

    fn on_keystroke(&mut self, now: Millis) -> Option<CancelReason> {
        if !self.policy.typing_enabled || self.in_grace(now) {
            // Inside grace this is dropped rather than buffered. A key pressed
            // in the first fifth of a second is far more likely to be the tail
            // of whatever they typed to summon her than the start of an
            // interruption.
            return None;
        }
        // Autorepeat: a held key is one intention, however many events it
        // generates. Measured against the previous *event* — see `last_key_at`.
        let is_repeat = match self.last_key_at {
            Some(last) => now.saturating_sub(last) < self.policy.key_repeat_ms as Millis,
            None => false,
        };
        self.last_key_at = Some(now);
        if is_repeat {
            return None;
        }

        let window = self.policy.keystroke_window_ms as Millis;
        self.keys.retain(|&t| now.saturating_sub(t) <= window);
        self.keys.push(now);

        if self.keys.len() as u32 >= self.policy.keystrokes_needed() {
            Some(CancelReason::Typing)
        } else {
            None
        }
    }

    fn on_mic(&mut self, peak: f32, now: Millis) -> Option<CancelReason> {
        if !self.policy.mic_enabled {
            return None;
        }
        // A NaN from a dead capture stream must not compare its way into a
        // cancel; `NaN >= x` is false, so this is belt and braces, but the run
        // state would still be advanced by the `else` branch below.
        if !peak.is_finite() {
            return None;
        }
        if self.in_grace(now) {
            // Not merely ignored — the run is *reset*. Her own first syllable
            // would otherwise leave a run already part-way to the sustain
            // threshold the moment grace lifts.
            self.mic_run_since = None;
            return None;
        }

        if peak >= self.policy.mic_threshold {
            let gap = self.policy.mic_gap_ms as Millis;
            let restart = match self.mic_run_since {
                None => true,
                Some(_) => now.saturating_sub(self.mic_last_over) > gap,
            };
            if restart {
                self.mic_run_since = Some(now);
            }
            self.mic_last_over = now;
            let since = self.mic_run_since.unwrap_or(now);
            if now.saturating_sub(since) >= self.policy.mic_sustain_ms as Millis {
                return Some(CancelReason::Speech);
            }
        } else if self.mic_run_since.is_some()
            && now.saturating_sub(self.mic_last_over) > self.policy.mic_gap_ms as Millis
        {
            self.mic_run_since = None;
        }
        None
    }

    fn on_focus(&mut self, now: Millis) -> Option<CancelReason> {
        if !self.policy.focus_enabled || self.in_grace(now) {
            return None;
        }
        Some(CancelReason::Focus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// She is talking and past the grace window — the state most of these are
    /// really about.
    fn speaking(policy: BargePolicy) -> BargeIn {
        let mut b = BargeIn::new(policy);
        b.speaking_started(0);
        b
    }

    /// The first instant at which a non-immediate signal is listened to.
    fn past_grace(b: &BargeIn) -> Millis {
        b.policy().grace_ms as Millis
    }

    // -- typing -------------------------------------------------------------

    #[test]
    fn one_stray_keystroke_does_not_cancel_her() {
        let mut b = speaking(BargePolicy::default());
        let t = past_grace(&b);
        assert_eq!(b.observe(BargeSignal::Keystroke, t), None, "Ctrl on the way to a shortcut");
        assert!(b.cancelled().is_none());
    }

    #[test]
    fn enough_keystrokes_inside_the_window_do_cancel_her() {
        let mut b = speaking(BargePolicy::default());
        let t = past_grace(&b);
        assert_eq!(b.observe(BargeSignal::Keystroke, t), None);
        assert_eq!(b.observe(BargeSignal::Keystroke, t + 100), None);
        assert_eq!(b.observe(BargeSignal::Keystroke, t + 200), Some(CancelReason::Typing));
    }

    #[test]
    fn keystrokes_older_than_the_window_do_not_count_towards_the_next_ones() {
        let mut b = speaking(BargePolicy::default());
        let t = past_grace(&b);
        let window = b.policy().keystroke_window_ms as Millis;
        b.observe(BargeSignal::Keystroke, t);
        b.observe(BargeSignal::Keystroke, t + 100);
        // A long pause: they went back to reading.
        assert_eq!(b.observe(BargeSignal::Keystroke, t + 100 + window + 1), None);
        assert_eq!(b.observe(BargeSignal::Keystroke, t + 200 + window + 1), None);
        assert_eq!(
            b.observe(BargeSignal::Keystroke, t + 300 + window + 1),
            Some(CancelReason::Typing),
            "three fresh ones, not one fresh plus two stale"
        );
    }

    #[test]
    fn key_autorepeat_is_one_intention_however_many_events_it_sends() {
        let mut b = speaking(BargePolicy::default());
        let t = past_grace(&b);
        // A held arrow key at roughly 30 Hz for a third of a second.
        for i in 0..10u64 {
            assert_eq!(
                b.observe(BargeSignal::Keystroke, t + i * 30),
                None,
                "autorepeat must not read as typing (event {i})"
            );
        }
    }

    #[test]
    fn typing_can_be_switched_off_entirely() {
        let mut b = speaking(BargePolicy { typing_enabled: false, ..Default::default() });
        let t = past_grace(&b);
        for i in 0..20u64 {
            assert_eq!(b.observe(BargeSignal::Keystroke, t + i * 100), None);
        }
    }

    #[test]
    fn a_policy_of_one_keystroke_cancels_on_the_first_one() {
        let mut b = speaking(BargePolicy { keystrokes: 1, ..Default::default() });
        let t = past_grace(&b);
        assert_eq!(b.observe(BargeSignal::Keystroke, t), Some(CancelReason::Typing));
    }

    #[test]
    fn a_policy_of_zero_keystrokes_is_read_as_one_rather_than_as_always() {
        let mut b = speaking(BargePolicy { keystrokes: 0, ..Default::default() });
        let t = past_grace(&b);
        assert_eq!(b.observe(BargeSignal::Keystroke, t), Some(CancelReason::Typing));
    }

    // -- the grace window ---------------------------------------------------

    #[test]
    fn she_does_not_interrupt_herself_during_the_grace_window() {
        let mut b = speaking(BargePolicy::everything());
        // Her own first syllable, loud, straight back into the microphone.
        for t in 0..8u64 {
            assert_eq!(
                b.observe(BargeSignal::MicLevel { peak: 0.9 }, t * 40),
                None,
                "the classic bug: she hears herself and stops"
            );
        }
        assert!(b.cancelled().is_none());
    }

    #[test]
    fn her_own_bleed_does_not_leave_a_run_part_way_to_the_sustain_threshold() {
        let mut b = speaking(BargePolicy::everything());
        let g = past_grace(&b);
        // Loud all through grace, then silence the instant grace lifts.
        for t in (0..g).step_by(20) {
            b.observe(BargeSignal::MicLevel { peak: 0.9 }, t);
        }
        assert_eq!(
            b.observe(BargeSignal::MicLevel { peak: 0.9 }, g),
            None,
            "the run has to start when grace lifts, not before it"
        );
    }

    #[test]
    fn typing_inside_the_grace_window_is_dropped_rather_than_buffered() {
        let mut b = speaking(BargePolicy::default());
        let g = past_grace(&b);
        for i in 0..5u64 {
            assert_eq!(b.observe(BargeSignal::Keystroke, i * 50), None);
        }
        // Whatever they typed to summon her does not carry over the boundary.
        assert_eq!(b.observe(BargeSignal::Keystroke, g), None);
        assert_eq!(b.observe(BargeSignal::Keystroke, g + 100), None);
        assert_eq!(b.observe(BargeSignal::Keystroke, g + 200), Some(CancelReason::Typing));
    }

    #[test]
    fn explicit_is_immediate_even_in_the_first_millisecond() {
        let mut b = speaking(BargePolicy::default());
        assert!(b.in_grace(0));
        assert_eq!(b.observe(BargeSignal::Explicit, 0), Some(CancelReason::Explicit));
    }

    #[test]
    fn push_to_talk_is_immediate_too() {
        let mut b = speaking(BargePolicy::default());
        assert_eq!(b.observe(BargeSignal::PushToTalk, 5), Some(CancelReason::PushToTalk));
    }

    #[test]
    fn push_to_talk_can_be_switched_off_but_explicit_cannot() {
        let mut b = speaking(BargePolicy { push_to_talk_enabled: false, ..Default::default() });
        assert_eq!(b.observe(BargeSignal::PushToTalk, 5), None, "no key is bound to it");
        assert_eq!(
            b.observe(BargeSignal::Explicit, 6),
            Some(CancelReason::Explicit),
            "the stop button is never something she declines to listen to"
        );
    }

    #[test]
    fn deliberate_only_keeps_the_two_unambiguous_signals_and_drops_the_rest() {
        let mut b = speaking(BargePolicy::deliberate_only());
        let t = past_grace(&b);
        for i in 0..10u64 {
            assert_eq!(b.observe(BargeSignal::Keystroke, t + i * 100), None);
            assert_eq!(b.observe(BargeSignal::MicLevel { peak: 1.0 }, t + i * 100), None);
        }
        assert_eq!(b.observe(BargeSignal::Explicit, t + 2000), Some(CancelReason::Explicit));
    }

    // -- the microphone -----------------------------------------------------

    #[test]
    fn the_microphone_is_off_by_default_so_a_level_never_cancels_her() {
        let mut b = speaking(BargePolicy::default());
        assert!(!b.policy().mic_enabled, "the mic is opt-in and invasive; SPEC §3.7");
        let t = past_grace(&b);
        for i in 0..50u64 {
            assert_eq!(b.observe(BargeSignal::MicLevel { peak: 1.0 }, t + i * 20), None);
        }
    }

    #[test]
    fn a_sustained_level_over_the_threshold_cancels_her() {
        let p = BargePolicy::everything();
        let sustain = p.mic_sustain_ms as Millis;
        let mut b = speaking(p);
        let t = past_grace(&b);
        assert_eq!(b.observe(BargeSignal::MicLevel { peak: 0.5 }, t), None, "the run starts here");
        assert_eq!(b.observe(BargeSignal::MicLevel { peak: 0.5 }, t + sustain / 2), None);
        assert_eq!(
            b.observe(BargeSignal::MicLevel { peak: 0.5 }, t + sustain),
            Some(CancelReason::Speech)
        );
    }

    #[test]
    fn a_single_loud_frame_is_a_door_and_not_a_person() {
        let p = BargePolicy::everything();
        let sustain = p.mic_sustain_ms as Millis;
        let gap = p.mic_gap_ms as Millis;
        let mut b = speaking(p);
        let t = past_grace(&b);
        assert_eq!(b.observe(BargeSignal::MicLevel { peak: 1.0 }, t), None);
        // Back to silence for longer than a syllable gap.
        assert_eq!(b.observe(BargeSignal::MicLevel { peak: 0.0 }, t + gap + 1), None);
        assert_eq!(b.observe(BargeSignal::MicLevel { peak: 0.0 }, t + gap + 50), None);
        // Loud again, but the earlier frame must not count towards this run.
        assert_eq!(b.observe(BargeSignal::MicLevel { peak: 1.0 }, t + sustain), None);
    }

    /// The level meter reports continuously — `wisp_senses::audio` reduces one
    /// PipeWire quantum to one peak — so a gap is a run of *quiet readings*,
    /// not an absence of readings. Feeding it every 25 ms is what the real
    /// caller does, and it is the only way this rule means anything.
    #[test]
    fn a_dip_between_two_syllables_does_not_end_the_run() {
        let p = BargePolicy::everything();
        let sustain = p.mic_sustain_ms as Millis;
        let gap = p.mic_gap_ms as Millis;
        let mut b = speaking(p);
        let t = past_grace(&b);

        let mut cancelled_at = None;
        let mut ms = 0;
        while ms <= sustain {
            // Quiet from 100 ms to 175 ms: a 75 ms dip, half the allowed gap.
            let peak = if (100..175).contains(&ms) { 0.01 } else { 0.5 };
            if let Some(r) = b.observe(BargeSignal::MicLevel { peak }, t + ms) {
                cancelled_at = Some((ms, r));
                break;
            }
            ms += 25;
        }
        assert_eq!(
            cancelled_at,
            Some((sustain, CancelReason::Speech)),
            "the run must survive the pause inside a word and land on the sustain, not after it"
        );
        assert!(gap > 75, "the fixture only means something while the dip is under the gap");
    }

    #[test]
    fn a_dip_longer_than_the_gap_does_end_the_run() {
        let p = BargePolicy::everything();
        let sustain = p.mic_sustain_ms as Millis;
        let gap = p.mic_gap_ms as Millis;
        let mut b = speaking(p);
        let t = past_grace(&b);

        let mut ms = 0;
        while ms <= sustain {
            // They said one word and stopped: quiet for longer than `mic_gap_ms`.
            let peak = if ms < 50 || ms > 50 + gap + 25 { 0.5 } else { 0.01 };
            assert_eq!(
                b.observe(BargeSignal::MicLevel { peak }, t + ms),
                None,
                "the run restarted after the pause, so the sustain starts over (at {ms} ms)"
            );
            ms += 25;
        }
    }

    #[test]
    fn a_level_below_the_threshold_never_starts_a_run_at_all() {
        let p = BargePolicy::everything();
        let thresh = p.mic_threshold;
        let mut b = speaking(p);
        let t = past_grace(&b);
        for i in 0..40u64 {
            assert_eq!(
                b.observe(BargeSignal::MicLevel { peak: thresh - 0.01 }, t + i * 20),
                None,
                "room tone and her own bleed sit here"
            );
        }
    }

    #[test]
    fn a_nan_or_infinite_peak_from_a_dead_capture_stream_is_ignored() {
        let p = BargePolicy::everything();
        let sustain = p.mic_sustain_ms as Millis;
        let mut b = speaking(p);
        let t = past_grace(&b);
        for i in 0..20u64 {
            assert_eq!(b.observe(BargeSignal::MicLevel { peak: f32::NAN }, t + i * 20), None);
            assert_eq!(b.observe(BargeSignal::MicLevel { peak: f32::INFINITY }, t + i * 20), None);
        }
        assert!(b.cancelled().is_none());
        // …and it did not leave a run behind either.
        b.observe(BargeSignal::MicLevel { peak: 0.5 }, t + 1000);
        assert_eq!(b.observe(BargeSignal::MicLevel { peak: 0.5 }, t + 1000 + sustain - 1), None);
    }

    #[test]
    fn a_zero_sustain_policy_cancels_on_the_first_frame_over_the_threshold() {
        let mut b = speaking(BargePolicy { mic_sustain_ms: 0, ..BargePolicy::everything() });
        let t = past_grace(&b);
        assert_eq!(b.observe(BargeSignal::MicLevel { peak: 0.5 }, t), Some(CancelReason::Speech));
    }

    #[test]
    fn turning_the_microphone_on_mid_session_does_not_inherit_an_old_run() {
        let p = BargePolicy::everything();
        let sustain = p.mic_sustain_ms as Millis;
        let mut b = speaking(p);
        let t = past_grace(&b);
        b.observe(BargeSignal::MicLevel { peak: 0.5 }, t);
        b.set_policy(BargePolicy { mic_threshold: 0.4, ..BargePolicy::everything() });
        assert_eq!(
            b.observe(BargeSignal::MicLevel { peak: 0.5 }, t + sustain),
            None,
            "a run measured against the old threshold means nothing against the new one"
        );
    }

    // -- focus --------------------------------------------------------------

    #[test]
    fn focus_changes_are_ignored_unless_the_operator_asked_for_them() {
        let mut b = speaking(BargePolicy::default());
        let t = past_grace(&b);
        assert_eq!(
            b.observe(BargeSignal::FocusChanged, t),
            None,
            "a tooltip is not the operator walking away"
        );
    }

    #[test]
    fn focus_changes_cancel_her_when_they_are_switched_on() {
        let mut b = speaking(BargePolicy::everything());
        let t = past_grace(&b);
        assert_eq!(b.observe(BargeSignal::FocusChanged, t), Some(CancelReason::Focus));
    }

    #[test]
    fn the_window_she_was_summoned_from_losing_focus_during_grace_is_not_a_barge_in() {
        let mut b = speaking(BargePolicy::everything());
        assert_eq!(b.observe(BargeSignal::FocusChanged, 10), None);
    }

    // -- utterance boundaries ----------------------------------------------

    #[test]
    fn nothing_cancels_while_she_is_not_speaking() {
        let mut b = BargeIn::new(BargePolicy::everything());
        for sig in [
            BargeSignal::Keystroke,
            BargeSignal::Explicit,
            BargeSignal::PushToTalk,
            BargeSignal::FocusChanged,
            BargeSignal::MicLevel { peak: 1.0 },
        ] {
            assert_eq!(b.observe(sig, 1000), None, "{sig:?} with nothing to interrupt");
        }
        assert_eq!(b.cancels, 0);
    }

    #[test]
    fn a_keystroke_from_thirty_seconds_ago_does_not_cancel_the_next_sentence() {
        let mut b = BargeIn::new(BargePolicy::default());
        b.speaking_started(0);
        let t = past_grace(&b);
        b.observe(BargeSignal::Keystroke, t);
        b.observe(BargeSignal::Keystroke, t + 50);
        b.speaking_stopped(t + 100);

        // Half a minute later she says something else.
        b.speaking_started(30_000);
        let t2 = 30_000 + past_grace(&b);
        assert_eq!(
            b.observe(BargeSignal::Keystroke, t2),
            None,
            "the two from the last utterance must not be waiting for a third"
        );
        assert_eq!(b.observe(BargeSignal::Keystroke, t2 + 50), None);
        assert_eq!(b.observe(BargeSignal::Keystroke, t2 + 100), Some(CancelReason::Typing));
    }

    #[test]
    fn signals_between_utterances_are_dropped_rather_than_queued() {
        let mut b = BargeIn::new(BargePolicy::default());
        b.speaking_started(0);
        b.speaking_stopped(500);
        // A whole paragraph typed while she is silent.
        for i in 0..30u64 {
            assert_eq!(b.observe(BargeSignal::Keystroke, 600 + i * 40), None);
        }
        b.speaking_started(2000);
        let t = 2000 + past_grace(&b);
        assert_eq!(b.observe(BargeSignal::Keystroke, t), None, "not one of those carried over");
    }

    #[test]
    fn only_the_first_cancel_of_an_utterance_is_reported() {
        let mut b = speaking(BargePolicy::everything());
        assert_eq!(b.observe(BargeSignal::Explicit, 10), Some(CancelReason::Explicit));
        assert_eq!(b.observe(BargeSignal::Explicit, 11), None, "the queue is torn down once");
        assert_eq!(b.observe(BargeSignal::PushToTalk, 12), None);
        let t = past_grace(&b);
        for i in 0..10u64 {
            assert_eq!(b.observe(BargeSignal::Keystroke, t + i * 100), None);
        }
        assert_eq!(b.cancelled(), Some(CancelReason::Explicit), "and the reason does not change");
        assert_eq!(b.cancels, 1);
    }

    #[test]
    fn a_new_utterance_clears_the_previous_cancel() {
        let mut b = speaking(BargePolicy::default());
        b.observe(BargeSignal::Explicit, 5);
        assert!(b.cancelled().is_some());
        b.speaking_started(1000);
        assert!(b.cancelled().is_none());
        assert_eq!(b.observe(BargeSignal::Explicit, 1005), Some(CancelReason::Explicit));
        assert_eq!(b.cancels, 2);
    }

    // -- the governor -------------------------------------------------------

    #[test]
    fn the_governor_can_cancel_her_at_any_point_including_inside_grace() {
        let mut b = speaking(BargePolicy::deliberate_only());
        assert!(b.in_grace(0));
        assert_eq!(b.cancel_for_tier(0), Some(CancelReason::Tier), "SPEC §3.1 sheds, it does not ask");
    }

    #[test]
    fn a_tier_drop_after_a_cancel_does_not_tear_the_queue_down_twice() {
        let mut b = speaking(BargePolicy::default());
        assert_eq!(b.observe(BargeSignal::Explicit, 5), Some(CancelReason::Explicit));
        assert_eq!(b.cancel_for_tier(6), None);
        assert_eq!(b.cancels, 1);
    }

    #[test]
    fn the_governor_cancelling_silence_is_a_no_op() {
        let mut b = BargeIn::new(BargePolicy::default());
        assert_eq!(b.cancel_for_tier(0), None);
    }

    // -- robustness ---------------------------------------------------------

    #[test]
    fn a_clock_that_goes_backwards_does_not_panic_or_cancel() {
        let mut b = speaking(BargePolicy::everything());
        // Every arm of every rule, fed a `now` before the utterance began.
        assert_eq!(b.observe(BargeSignal::Keystroke, 0), None);
        assert_eq!(b.observe(BargeSignal::MicLevel { peak: 1.0 }, 0), None);
        assert_eq!(b.observe(BargeSignal::FocusChanged, 0), None);
        b.speaking_started(10_000);
        assert_eq!(b.observe(BargeSignal::Keystroke, 5), None);
        assert_eq!(b.observe(BargeSignal::MicLevel { peak: 1.0 }, 5), None);
        assert_eq!(b.speaking_for(5), Some(0));
        assert!(b.in_grace(5));
    }

    #[test]
    fn reset_puts_her_back_to_silent_and_uncancelled() {
        let mut b = speaking(BargePolicy::default());
        b.observe(BargeSignal::Explicit, 5);
        b.reset();
        assert!(!b.is_speaking());
        assert!(b.cancelled().is_none());
        assert_eq!(b.observe(BargeSignal::Explicit, 6), None);
        assert_eq!(b.cancels, 1, "the counter is history, not state");
    }

    #[test]
    fn a_deliberate_cancel_is_distinguishable_from_an_inferred_one() {
        assert!(CancelReason::Explicit.was_deliberate());
        assert!(CancelReason::PushToTalk.was_deliberate());
        for r in [CancelReason::Typing, CancelReason::Speech, CancelReason::Focus, CancelReason::Tier]
        {
            assert!(!r.was_deliberate(), "{r:?}");
        }
    }
}
