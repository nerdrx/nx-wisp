//! SPEC §0.3 — feeding the visible tell. **Nothing invisible.**
//!
//! > Mic, clipboard and screen access have a visible tell on the character
//! > herself whenever they are live, and every use is recorded.
//!
//! The tell itself is already built and drawn by `wisp-shell` — a hard-cut
//! `#ff5470` chip pinned to her silhouette, carrying a three-bar level-meter
//! glyph for the microphone. This module does **not** draw anything and does not
//! depend on `wisp-shell` (SPEC §2 does not allow it). It produces the numbers
//! that shell's `tell::build(sense, active, anchor, size, phase, scene)` is fed,
//! and it exists so that the *liveness* of the tell is derived from the same
//! microphone state as the audio, in one place, rather than being reconstructed
//! by whoever happens to be drawing.
//!
//! ## What the tell can and cannot show today
//!
//! `wisp_shell::tell` takes a `phase: f32` — a 0..1 position it turns into a
//! triangle-wave halo pulse — and draws the microphone's three bars at **fixed
//! heights**. So the shell can render *that she is listening* and *a breathing
//! rhythm*, but it cannot currently render *how loud you are*. That is an API
//! gap, not a design decision here, and it is reported rather than worked around
//! by drawing something ourselves: §0.3's tell is deliberately one implementation
//! in one crate, and a second renderer would be a second thing to get wrong.
//!
//! [`TellDrive`] therefore carries both: `phase`, which shell consumes now, and
//! `level`, which is already correct and is what a level-driven glyph would use
//! the day shell grows one.
//!
//! ## The rule that shapes the phase
//!
//! **A live tell must never stop moving.** If the room is silent and the phase
//! froze, the chip would sit there static and read as decoration — or worse, as
//! an indicator that is stuck rather than live, which is precisely the failure
//! §0.3 is written against. So the pulse has a floor rate it never drops below,
//! and the operator's voice makes it faster and deeper rather than turning it
//! on. There is a test for exactly this.

use crate::Millis;

/// What the host hands to `wisp_shell::tell::build`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TellDrive {
    /// Is the microphone actually open? Drawn or not drawn; there is no middle.
    pub active: bool,
    /// Input level, 0..=1, smoothed. Not consumed by shell yet — see the module
    /// docs.
    pub level: f32,
    /// 0..=1 position in the pulse cycle, which is what shell renders.
    pub phase: f32,
}

impl TellDrive {
    pub const OFF: TellDrive = TellDrive { active: false, level: 0.0, phase: 0.0 };
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TellConfig {
    /// Pulses per second in a silent room. **Must be greater than zero** — see
    /// the module docs.
    pub idle_hz: f32,
    /// Extra pulses per second at full level.
    pub excited_hz: f32,
    /// How fast the displayed level rises towards the input.
    pub attack_ms: u32,
    /// How fast it falls. Much slower than the attack, so the tell does not
    /// strobe between syllables.
    pub release_ms: u32,
}

impl Default for TellConfig {
    fn default() -> Self {
        TellConfig {
            // Slow enough to read as breathing rather than as an alarm; fast
            // enough that a glance catches it. Roughly one and a half seconds
            // per cycle at rest.
            idle_hz: 0.65,
            excited_hz: 1.35,
            attack_ms: 40,
            release_ms: 320,
        }
    }
}

/// Turns "the microphone is open, and this loud" into a phase the shell can
/// draw. Owns no clock: the host passes `now`, exactly like `wisp-attn` and
/// `wisp-shell`'s own tell module.
#[derive(Debug, Clone)]
pub struct TellFeed {
    cfg: TellConfig,
    active: bool,
    /// Where the level is being pulled towards. Kept separate from `level` so
    /// `set_level` can be called at the capture rate and `tick` at the frame
    /// rate without either having to know the other's cadence.
    target: f32,
    level: f32,
    phase: f32,
    last: Option<Millis>,
}

impl Default for TellFeed {
    fn default() -> Self {
        TellFeed::new(TellConfig::default())
    }
}

impl TellFeed {
    pub fn new(cfg: TellConfig) -> Self {
        TellFeed { cfg, active: false, target: 0.0, level: 0.0, phase: 0.0, last: None }
    }

    /// The microphone opened or closed.
    ///
    /// Closing resets the phase so the next session starts at the bottom of the
    /// pulse — the tell appearing mid-flash would read as a glitch.
    pub fn set_active(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        self.active = active;
        self.phase = 0.0;
        self.level = 0.0;
        self.target = 0.0;
        self.last = None;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Raw input level for this instant, 0..=1, from
    /// `crate::mic::Listener::level`.
    pub fn set_level(&mut self, level: f32) {
        self.target = if level.is_finite() { level.clamp(0.0, 1.0) } else { 0.0 };
    }

    /// Advance and read. Idempotent for a repeated `now`, so a host that draws
    /// twice in one frame does not double the pulse rate.
    pub fn tick(&mut self, now: Millis) -> TellDrive {
        if !self.active {
            self.last = Some(now);
            return TellDrive::OFF;
        }
        let dt_ms = match self.last {
            Some(prev) if now > prev => (now - prev) as f32,
            _ => 0.0,
        };
        self.last = Some(now);

        // Fast attack, slow release: she should react to a voice at once and
        // settle back over a moment, or the chip strobes between syllables.
        let tau = if self.target > self.level {
            self.cfg.attack_ms
        } else {
            self.cfg.release_ms
        } as f32;
        let k = if tau <= 0.0 { 1.0 } else { (dt_ms / tau).clamp(0.0, 1.0) };
        self.level += (self.target - self.level) * k;

        let hz = (self.cfg.idle_hz + self.cfg.excited_hz * self.level).max(0.05);
        self.phase = (self.phase + hz * dt_ms / 1000.0).rem_euclid(1.0);

        TellDrive { active: true, level: self.level, phase: self.phase }
    }

    /// The smoothed level, without advancing.
    pub fn level(&self) -> f32 {
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed() -> TellFeed {
        TellFeed::default()
    }

    #[test]
    fn nothing_is_drawn_while_the_microphone_is_shut() {
        let mut f = feed();
        assert_eq!(f.tick(0), TellDrive::OFF);
        assert_eq!(f.tick(5_000), TellDrive::OFF);
    }

    #[test]
    fn the_tell_goes_live_the_moment_the_microphone_does() {
        let mut f = feed();
        f.set_active(true);
        assert!(f.tick(0).active, "SPEC §0.3: it is live, so it is visible");
    }

    /// The property this module exists for. A frozen indicator reads as a stuck
    /// one, and a stuck one teaches the operator to ignore it.
    #[test]
    fn a_live_tell_never_stops_moving_even_in_a_silent_room() {
        let mut f = feed();
        f.set_active(true);
        f.set_level(0.0);
        let mut seen: Vec<f32> = Vec::new();
        let mut t: Millis = 0;
        for _ in 0..60 {
            seen.push(f.tick(t).phase);
            t += 50;
        }
        let spread = seen.iter().cloned().fold(f32::MIN, f32::max)
            - seen.iter().cloned().fold(f32::MAX, f32::min);
        assert!(spread > 0.5, "the phase barely moved: spread {spread}");
        // And it wrapped rather than saturating.
        assert!(seen.iter().all(|p| (0.0..1.0).contains(p)));
    }

    #[test]
    fn a_voice_makes_the_pulse_faster_rather_than_turning_it_on() {
        let mut quiet = feed();
        let mut loud = feed();
        quiet.set_active(true);
        loud.set_active(true);
        quiet.set_level(0.0);
        loud.set_level(1.0);
        // Let the smoothing settle first.
        let mut t: Millis = 0;
        for _ in 0..20 {
            quiet.tick(t);
            loud.tick(t);
            t += 50;
        }
        let (q0, l0) = (quiet.tick(t).phase, loud.tick(t).phase);
        t += 200;
        let advance = |p0: f32, p1: f32| (p1 - p0).rem_euclid(1.0);
        let dq = advance(q0, quiet.tick(t).phase);
        let dl = advance(l0, loud.tick(t).phase);
        assert!(dl > dq, "loud advanced {dl}, quiet {dq}");
        assert!(dq > 0.0, "quiet must still move");
    }

    #[test]
    fn the_level_rises_fast_and_falls_slowly() {
        let mut f = feed();
        f.set_active(true);
        f.tick(0);
        f.set_level(1.0);
        f.tick(60);
        let after_rise = f.level();
        assert!(after_rise > 0.5, "attack was too slow: {after_rise}");
        f.set_level(0.0);
        f.tick(120);
        let after_fall = f.level();
        assert!(
            after_fall > 0.4,
            "release must be slow or the chip strobes between syllables: {after_fall}"
        );
        for t in (180..4_000).step_by(60) {
            f.tick(t as Millis);
        }
        assert!(f.level() < 0.05, "…but it must eventually settle: {}", f.level());
    }

    #[test]
    fn closing_the_microphone_resets_the_pulse_so_it_never_reappears_mid_flash() {
        let mut f = feed();
        f.set_active(true);
        f.set_level(0.8);
        for t in (0..1_000).step_by(50) {
            f.tick(t as Millis);
        }
        assert!(f.level() > 0.1);
        f.set_active(false);
        assert_eq!(f.tick(1_050), TellDrive::OFF);
        f.set_active(true);
        let d = f.tick(1_100);
        assert_eq!(d.phase, 0.0);
        assert_eq!(d.level, 0.0);
    }

    #[test]
    fn a_repeated_timestamp_does_not_double_the_pulse_rate() {
        let mut f = feed();
        f.set_active(true);
        f.tick(0);
        let a = f.tick(500).phase;
        let b = f.tick(500).phase;
        assert_eq!(a, b, "drawing twice in one frame must not advance time twice");
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_panic_or_run_the_pulse_in_reverse() {
        let mut f = feed();
        f.set_active(true);
        f.tick(10_000);
        let a = f.tick(5_000).phase;
        assert!(a.is_finite() && (0.0..1.0).contains(&a));
    }

    #[test]
    fn a_poisoned_level_is_ignored_rather_than_propagated() {
        let mut f = feed();
        f.set_active(true);
        f.set_level(f32::NAN);
        f.tick(0);
        let d = f.tick(100);
        assert!(d.level.is_finite());
        assert!(d.phase.is_finite());
        f.set_level(17.0);
        let d = f.tick(400);
        assert!(d.level <= 1.0);
    }

    #[test]
    fn the_idle_rate_is_greater_than_zero_by_construction() {
        // If this ever becomes zero the tell freezes, so it is asserted rather
        // than merely commented.
        assert!(TellConfig::default().idle_hz > 0.0);
        assert!(TellConfig::default().attack_ms < TellConfig::default().release_ms);
    }
}
