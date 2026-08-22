//! Animation clips: named, keyframed bone-channel tracks.
//!
//! A track stores parallel `times` / `values` / `eases` arrays rather than a
//! `Vec<Key>` because that is exactly how it is authored in a skin file (one
//! readable stanza per bone-channel) and because sampling then touches two
//! small contiguous slices instead of a strided struct array.
//!
//! Times are seconds internally. A skin file authors milliseconds, which
//! matches DESIGN.md's duration tokens; the conversion happens once at compile.

use crate::ease::Ease;
use crate::math::lerp;
use crate::skeleton::{BoneOffsets, Channel};

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub bone: usize,
    pub channel: Channel,
    /// Strictly non-decreasing, in seconds from the clip's start.
    pub times: Vec<f32>,
    pub values: Vec<f32>,
    /// `eases[i]` governs the interpolation from key `i` to key `i + 1`. Always
    /// the same length as `times`; the last entry is unused.
    pub eases: Vec<Ease>,
}

impl Track {
    pub fn new(bone: usize, channel: Channel) -> Track {
        Track { bone, channel, times: Vec::new(), values: Vec::new(), eases: Vec::new() }
    }

    pub fn key(mut self, t: f32, v: f32, e: Ease) -> Track {
        self.times.push(t);
        self.values.push(v);
        self.eases.push(e);
        self
    }

    pub fn len(&self) -> usize {
        self.times.len()
    }
    pub fn is_empty(&self) -> bool {
        self.times.is_empty()
    }

    /// Sample at `t` seconds. Clamps outside the keyed range — looping is the
    /// clip's business, not the track's.
    pub fn sample(&self, t: f32) -> f32 {
        let n = self.times.len();
        if n == 0 {
            return self.channel.identity();
        }
        if n == 1 || t <= self.times[0] {
            return self.values[0];
        }
        if t >= self.times[n - 1] {
            return self.values[n - 1];
        }
        // Index of the last key at or before t.
        let i = self.times.partition_point(|&k| k <= t).saturating_sub(1);
        let (t0, t1) = (self.times[i], self.times[i + 1]);
        let span = t1 - t0;
        if span <= 1e-9 {
            // Coincident keys: a deliberate hard cut.
            return self.values[i + 1];
        }
        let u = self.eases[i].eval((t - t0) / span);
        lerp(self.values[i], self.values[i + 1], u)
    }

    /// Last keyed time, or 0.
    pub fn end(&self) -> f32 {
        self.times.last().copied().unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub name: Box<str>,
    /// Seconds. Always > 0 — validation rejects a zero-length clip.
    pub duration: f32,
    pub looping: bool,
    /// Authored as an additive clip. A layer may still play a non-additive
    /// clip additively; this is the skin's stated intent, used as the default.
    pub additive: bool,
    pub tracks: Vec<Track>,
}

impl Clip {
    pub fn new(name: impl Into<Box<str>>, duration: f32) -> Clip {
        Clip {
            name: name.into(),
            duration: duration.max(1e-4),
            looping: true,
            additive: false,
            tracks: Vec::new(),
        }
    }

    /// Map an arbitrary playhead onto the clip's timeline.
    pub fn wrap_time(&self, t: f32) -> f32 {
        if !t.is_finite() {
            return 0.0;
        }
        if self.looping {
            if self.duration <= 1e-6 {
                return 0.0;
            }
            let m = t % self.duration;
            if m < 0.0 {
                m + self.duration
            } else {
                m
            }
        } else {
            crate::math::clamp(t, 0.0, self.duration)
        }
    }

    /// Write this clip's contribution into `out`, which the caller has already
    /// reset to identity. Channels this clip does not key are left alone, so a
    /// cross-fade against a clip that keys fewer channels blends towards rest.
    pub fn eval(&self, t: f32, out: &mut [BoneOffsets]) {
        let t = self.wrap_time(t);
        for tr in &self.tracks {
            if let Some(slot) = out.get_mut(tr.bone) {
                slot.set(tr.channel, tr.sample(t));
            }
        }
    }

    /// Which bones this clip touches. Used by the editor and by tests.
    pub fn touched_bones(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.tracks.iter().map(|t| t.bone).collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}

/// The clip names F67/F70/F72 require of every skin, plus the eight
/// expressions of F74. A skin missing one of these still loads — the rig falls
/// back to the base clip — but `Skin::missing_required_clips` reports it and
/// `wispkit` can turn that into a warning.
pub const REQUIRED_CLIPS: [&str; 6] = ["idle", "walk", "hop", "sleep", "thrown", "pet"];

/// F74's expression set. These are *expression* names, not clip names — a skin
/// maps each to whatever clip it likes.
pub const REQUIRED_EXPRESSIONS: [&str; 8] = [
    "neutral", "curious", "delighted", "smug", "worried", "bored", "sleepy", "alarmed",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Track {
        Track::new(0, Channel::Ty)
            .key(0.0, 0.0, Ease::Soft)
            .key(1.0, 10.0, Ease::Soft)
            .key(2.0, 0.0, Ease::Soft)
    }

    #[test]
    fn sampling_hits_the_keys_exactly() {
        let tr = track();
        assert_eq!(tr.sample(0.0), 0.0);
        assert!((tr.sample(1.0) - 10.0).abs() < 1e-4);
        assert!((tr.sample(2.0) - 0.0).abs() < 1e-4);
    }

    #[test]
    fn sampling_clamps_outside_the_range() {
        let tr = track();
        assert_eq!(tr.sample(-5.0), 0.0);
        assert_eq!(tr.sample(99.0), 0.0);
    }

    #[test]
    fn empty_track_returns_the_channel_identity() {
        assert_eq!(Track::new(0, Channel::Sx).sample(0.5), 1.0);
        assert_eq!(Track::new(0, Channel::Tx).sample(0.5), 0.0);
    }

    #[test]
    fn single_key_track_holds_its_value() {
        let tr = Track::new(0, Channel::Rot).key(0.0, 1.25, Ease::Soft);
        assert_eq!(tr.sample(-1.0), 1.25);
        assert_eq!(tr.sample(50.0), 1.25);
    }

    #[test]
    fn easing_is_applied_between_keys_not_linear() {
        let soft = Track::new(0, Channel::Ty)
            .key(0.0, 0.0, Ease::Soft)
            .key(1.0, 100.0, Ease::Soft);
        let linear = Track::new(0, Channel::Ty)
            .key(0.0, 0.0, Ease::Linear)
            .key(1.0, 100.0, Ease::Linear);
        assert!((linear.sample(0.5) - 50.0).abs() < 1e-3);
        assert!(
            (soft.sample(0.5) - 50.0).abs() > 5.0,
            "soft should not be linear, got {}",
            soft.sample(0.5)
        );
    }

    #[test]
    fn spring_easing_may_overshoot_the_key_value() {
        let tr = Track::new(0, Channel::Ty)
            .key(0.0, 0.0, Ease::Spring)
            .key(1.0, 10.0, Ease::Spring);
        let peak = (0..=100)
            .map(|i| tr.sample(i as f32 / 100.0))
            .fold(f32::MIN, f32::max);
        assert!(peak > 10.2, "expected overshoot, peaked at {peak}");
    }

    #[test]
    fn step_easing_holds_then_jumps() {
        let tr = Track::new(0, Channel::Alpha)
            .key(0.0, 1.0, Ease::Step)
            .key(1.0, 0.0, Ease::Step);
        assert_eq!(tr.sample(0.99), 1.0);
        assert_eq!(tr.sample(1.0), 0.0);
    }

    #[test]
    fn coincident_keys_are_a_hard_cut_not_a_nan() {
        let tr = Track::new(0, Channel::Ty)
            .key(0.0, 0.0, Ease::Soft)
            .key(1.0, 5.0, Ease::Soft)
            .key(1.0, -5.0, Ease::Soft)
            .key(2.0, 0.0, Ease::Soft);
        let v = tr.sample(1.0);
        assert!(v.is_finite(), "{v}");
    }

    #[test]
    fn looping_wraps_the_playhead() {
        let mut c = Clip::new("idle", 2.0);
        c.looping = true;
        assert!((c.wrap_time(2.5) - 0.5).abs() < 1e-5);
        assert!((c.wrap_time(-0.5) - 1.5).abs() < 1e-5);
        assert!((c.wrap_time(4.0) - 0.0).abs() < 1e-5);
    }

    #[test]
    fn non_looping_clamps_the_playhead() {
        let mut c = Clip::new("hop", 2.0);
        c.looping = false;
        assert_eq!(c.wrap_time(5.0), 2.0);
        assert_eq!(c.wrap_time(-5.0), 0.0);
    }

    #[test]
    fn wrap_time_survives_nan() {
        let c = Clip::new("idle", 2.0);
        assert_eq!(c.wrap_time(f32::NAN), 0.0);
    }

    #[test]
    fn eval_writes_only_keyed_channels() {
        let mut c = Clip::new("t", 2.0);
        c.tracks.push(track());
        let mut out = vec![BoneOffsets::IDENTITY; 3];
        c.eval(1.0, &mut out);
        assert!((out[0].ty - 10.0).abs() < 1e-4);
        assert_eq!(out[0].tx, 0.0);
        assert_eq!(out[0].sx, 1.0);
        assert_eq!(out[1], BoneOffsets::IDENTITY);
    }

    #[test]
    fn eval_ignores_tracks_pointing_past_the_skeleton() {
        // Defence in depth: validation rejects these, but eval must not panic.
        let mut c = Clip::new("t", 1.0);
        c.tracks.push(Track::new(99, Channel::Ty).key(0.0, 1.0, Ease::Soft));
        let mut out = vec![BoneOffsets::IDENTITY; 2];
        c.eval(0.5, &mut out);
    }

    #[test]
    fn touched_bones_is_sorted_and_deduped() {
        let mut c = Clip::new("t", 1.0);
        c.tracks.push(Track::new(3, Channel::Ty));
        c.tracks.push(Track::new(1, Channel::Tx));
        c.tracks.push(Track::new(3, Channel::Rot));
        assert_eq!(c.touched_bones(), vec![1, 3]);
    }

    #[test]
    fn required_sets_are_the_ones_the_plan_names() {
        assert_eq!(REQUIRED_CLIPS.len(), 6);
        assert!(REQUIRED_EXPRESSIONS.contains(&"alarmed"));
        assert_eq!(REQUIRED_EXPRESSIONS.len(), 8);
    }
}
