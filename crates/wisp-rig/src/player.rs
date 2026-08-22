//! Layered clip playback with cross-fades (F67, F70).
//!
//! A layer holds at most two playing clips: the current one and the one it is
//! fading out of. Layers evaluate in declaration order; a non-additive layer
//! blends *over* what came before, an additive layer accumulates on top. That
//! is what lets "breathing" and "blinking" run on top of "walking" without any
//! of the three knowing about the others.
//!
//! Every buffer is allocated once in [`ClipPlayer::new`]; `update` and
//! `evaluate` do not allocate.

use crate::clip::Clip;
use crate::ease::Ease;
use crate::math::clamp;
use crate::skeleton::BoneOffsets;

#[derive(Debug, Clone, PartialEq)]
pub struct LayerSpec {
    pub name: Box<str>,
    pub additive: bool,
    /// Played automatically when the rig starts and whenever the layer is asked
    /// to return to rest.
    pub default_clip: Option<usize>,
    pub weight: f32,
}

impl LayerSpec {
    pub fn new(name: impl Into<Box<str>>, additive: bool) -> LayerSpec {
        LayerSpec { name: name.into(), additive, default_clip: None, weight: 1.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Playing {
    clip: usize,
    time: f32,
    speed: f32,
    /// Set once a non-looping clip reaches its end.
    finished: bool,
}

#[derive(Debug, Clone)]
struct Layer {
    additive: bool,
    weight: f32,
    cur: Option<Playing>,
    prev: Option<Playing>,
    /// Elapsed fade time, seconds.
    fade_t: f32,
    fade_dur: f32,
    fade_ease: Ease,
}

impl Layer {
    fn fade_alpha(&self) -> f32 {
        if self.fade_dur <= 1e-6 {
            1.0
        } else {
            self.fade_ease.eval(clamp(self.fade_t / self.fade_dur, 0.0, 1.0))
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClipPlayer {
    specs: Vec<LayerSpec>,
    layers: Vec<Layer>,
    by_name: Vec<Box<str>>,
    // Scratch, sized to the bone count. Never reallocated after `new`.
    buf_cur: Vec<BoneOffsets>,
    buf_prev: Vec<BoneOffsets>,
    buf_layer: Vec<BoneOffsets>,
}

impl ClipPlayer {
    pub fn new(specs: Vec<LayerSpec>, bone_count: usize) -> ClipPlayer {
        let layers = specs
            .iter()
            .map(|s| Layer {
                additive: s.additive,
                weight: s.weight,
                cur: s.default_clip.map(|c| Playing {
                    clip: c,
                    time: 0.0,
                    speed: 1.0,
                    finished: false,
                }),
                prev: None,
                fade_t: 0.0,
                fade_dur: 0.0,
                fade_ease: Ease::Soft,
            })
            .collect();
        let by_name = specs.iter().map(|s| s.name.clone()).collect();
        ClipPlayer {
            specs,
            layers,
            by_name,
            buf_cur: vec![BoneOffsets::IDENTITY; bone_count],
            buf_prev: vec![BoneOffsets::IDENTITY; bone_count],
            buf_layer: vec![BoneOffsets::IDENTITY; bone_count],
        }
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    pub fn layer_index(&self, name: &str) -> Option<usize> {
        self.by_name.iter().position(|n| &**n == name)
    }

    pub fn layer_spec(&self, layer: usize) -> Option<&LayerSpec> {
        self.specs.get(layer)
    }

    /// Clip currently playing on a layer, if any.
    pub fn current(&self, layer: usize) -> Option<usize> {
        self.layers.get(layer).and_then(|l| l.cur).map(|p| p.clip)
    }

    /// Playhead on a layer, in seconds.
    pub fn time(&self, layer: usize) -> f32 {
        self.layers.get(layer).and_then(|l| l.cur).map_or(0.0, |p| p.time)
    }

    /// True once a non-looping clip has run to its end. A looping clip is never
    /// finished.
    pub fn finished(&self, layer: usize) -> bool {
        self.layers.get(layer).and_then(|l| l.cur).is_some_and(|p| p.finished)
    }

    /// Still blending between two clips?
    pub fn fading(&self, layer: usize) -> bool {
        self.layers
            .get(layer)
            .is_some_and(|l| l.prev.is_some() && l.fade_t < l.fade_dur)
    }

    pub fn weight(&self, layer: usize) -> f32 {
        self.layers.get(layer).map_or(0.0, |l| l.weight)
    }

    pub fn set_weight(&mut self, layer: usize, w: f32) {
        if let Some(l) = self.layers.get_mut(layer) {
            l.weight = clamp(w, 0.0, 1.0);
        }
    }

    pub fn set_speed(&mut self, layer: usize, speed: f32) {
        if let Some(p) = self.layers.get_mut(layer).and_then(|l| l.cur.as_mut()) {
            p.speed = if speed.is_finite() { speed } else { 1.0 };
        }
    }

    /// Cross-fade a layer onto `clip` over `fade` seconds.
    ///
    /// Re-playing the clip that is already current is a no-op, so calling this
    /// every frame from a behaviour tree does not restart the animation. Pass
    /// `restart = true` via [`ClipPlayer::replay`] when you do want it to.
    pub fn play(&mut self, layer: usize, clip: usize, fade: f32) {
        if self.current(layer) == Some(clip) && !self.fading(layer) {
            return;
        }
        self.start(layer, Some(clip), fade);
    }

    /// Restart a clip from zero even if it is already playing.
    pub fn replay(&mut self, layer: usize, clip: usize, fade: f32) {
        self.start(layer, Some(clip), fade);
    }

    /// Fade a layer out to rest. On an additive layer that means "contribute
    /// nothing".
    pub fn stop(&mut self, layer: usize, fade: f32) {
        if self.current(layer).is_none() {
            return;
        }
        self.start(layer, None, fade);
    }

    /// Return a layer to its declared default clip.
    pub fn play_default(&mut self, layer: usize, fade: f32) {
        match self.specs.get(layer).and_then(|s| s.default_clip) {
            Some(c) => self.play(layer, c, fade),
            None => self.stop(layer, fade),
        }
    }

    fn start(&mut self, layer: usize, clip: Option<usize>, fade: f32) {
        let Some(l) = self.layers.get_mut(layer) else {
            return;
        };
        let fade = if fade.is_finite() { fade.max(0.0) } else { 0.0 };
        // If a fade is already in flight, the clip we are leaving is the one
        // that was visible most recently — the current one.
        l.prev = l.cur;
        l.cur = clip.map(|c| Playing { clip: c, time: 0.0, speed: 1.0, finished: false });
        l.fade_t = 0.0;
        l.fade_dur = fade;
        if fade <= 1e-6 {
            l.prev = None;
        }
    }

    /// Snap every layer to its current clip's first frame and drop any fade.
    /// Used on a teleport, and on a tier downgrade where continuing to blend
    /// would cost frames for nothing.
    pub fn snap(&mut self) {
        for l in &mut self.layers {
            l.prev = None;
            l.fade_t = 0.0;
            l.fade_dur = 0.0;
        }
    }

    /// Advance playheads and fades. `dt` is seconds.
    pub fn update(&mut self, clips: &[Clip], dt: f32) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        for l in &mut self.layers {
            for slot in [&mut l.cur, &mut l.prev] {
                if let Some(p) = slot.as_mut() {
                    if let Some(c) = clips.get(p.clip) {
                        p.time += dt * p.speed;
                        if !c.looping && p.time >= c.duration {
                            p.time = c.duration;
                            p.finished = true;
                        }
                    }
                }
            }
            if l.fade_dur > 0.0 {
                l.fade_t += dt;
                if l.fade_t >= l.fade_dur {
                    l.fade_t = l.fade_dur;
                    l.prev = None;
                }
            }
        }
    }

    /// Compose every layer into `out`, which must be sized to the bone count.
    pub fn evaluate(&mut self, clips: &[Clip], out: &mut [BoneOffsets]) {
        for o in out.iter_mut() {
            *o = BoneOffsets::IDENTITY;
        }
        for li in 0..self.layers.len() {
            let (weight, additive, alpha, cur, prev) = {
                let l = &self.layers[li];
                (l.weight, l.additive, l.fade_alpha(), l.cur, l.prev)
            };
            if weight <= 1e-4 || (cur.is_none() && prev.is_none()) {
                continue;
            }

            reset(&mut self.buf_cur);
            if let Some(p) = cur {
                if let Some(c) = clips.get(p.clip) {
                    c.eval(p.time, &mut self.buf_cur);
                }
            }

            let layer_vals: &[BoneOffsets] = if let Some(pp) = prev {
                reset(&mut self.buf_prev);
                if let Some(c) = clips.get(pp.clip) {
                    c.eval(pp.time, &mut self.buf_prev);
                }
                for (i, slot) in self.buf_layer.iter_mut().enumerate() {
                    *slot = self.buf_prev[i];
                    slot.blend(&self.buf_cur[i], alpha);
                }
                &self.buf_layer
            } else if alpha < 1.0 {
                // Fading *in* with nothing behind: blend up from rest.
                for (i, slot) in self.buf_layer.iter_mut().enumerate() {
                    *slot = BoneOffsets::IDENTITY;
                    slot.blend(&self.buf_cur[i], alpha);
                }
                &self.buf_layer
            } else {
                &self.buf_cur
            };

            for (i, o) in out.iter_mut().enumerate() {
                let v = &layer_vals[i];
                if additive {
                    o.accumulate(v, weight);
                } else {
                    o.blend(v, weight);
                }
            }
        }
    }
}

fn reset(buf: &mut [BoneOffsets]) {
    for b in buf.iter_mut() {
        *b = BoneOffsets::IDENTITY;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::Track;
    use crate::skeleton::Channel;

    /// clip 0: body.ty flat 10. clip 1: body.ty flat 20. clip 2: additive
    /// body.ty flat 5.
    fn clips() -> Vec<Clip> {
        let mk = |name: &str, v: f32, additive: bool| {
            let mut c = Clip::new(name, 1.0);
            c.additive = additive;
            c.tracks
                .push(Track::new(0, Channel::Ty).key(0.0, v, Ease::Linear).key(1.0, v, Ease::Linear));
            c
        };
        vec![mk("a", 10.0, false), mk("b", 20.0, false), mk("add", 5.0, true)]
    }

    fn player() -> ClipPlayer {
        ClipPlayer::new(
            vec![
                LayerSpec { name: "base".into(), additive: false, default_clip: Some(0), weight: 1.0 },
                LayerSpec { name: "extra".into(), additive: true, default_clip: None, weight: 1.0 },
            ],
            2,
        )
    }

    fn eval(p: &mut ClipPlayer, cl: &[Clip]) -> f32 {
        let mut out = vec![BoneOffsets::IDENTITY; 2];
        p.evaluate(cl, &mut out);
        out[0].ty
    }

    #[test]
    fn default_clip_plays_from_the_start() {
        let cl = clips();
        let mut p = player();
        assert_eq!(p.current(0), Some(0));
        assert!((eval(&mut p, &cl) - 10.0).abs() < 1e-4);
    }

    #[test]
    fn layer_names_resolve() {
        let p = player();
        assert_eq!(p.layer_index("base"), Some(0));
        assert_eq!(p.layer_index("extra"), Some(1));
        assert_eq!(p.layer_index("nope"), None);
    }

    #[test]
    fn crossfade_moves_smoothly_from_one_clip_to_the_other() {
        let cl = clips();
        let mut p = player();
        p.play(0, 1, 1.0);
        assert!((eval(&mut p, &cl) - 10.0).abs() < 1e-3, "should start at the old clip");
        p.update(&cl, 0.5);
        let mid = eval(&mut p, &cl);
        assert!(mid > 10.5 && mid < 19.5, "mid-fade value was {mid}");
        p.update(&cl, 0.5);
        assert!((eval(&mut p, &cl) - 20.0).abs() < 1e-3);
        assert!(!p.fading(0));
    }

    #[test]
    fn zero_length_fade_switches_immediately() {
        let cl = clips();
        let mut p = player();
        p.play(0, 1, 0.0);
        assert!((eval(&mut p, &cl) - 20.0).abs() < 1e-4);
    }

    #[test]
    fn replaying_the_current_clip_is_a_no_op() {
        let cl = clips();
        let mut p = player();
        p.update(&cl, 0.4);
        p.play(0, 0, 0.2);
        assert!((p.time(0) - 0.4).abs() < 1e-4, "play() restarted the clip");
    }

    #[test]
    fn replay_does_restart_the_clip() {
        let cl = clips();
        let mut p = player();
        p.update(&cl, 0.4);
        p.replay(0, 0, 0.0);
        assert_eq!(p.time(0), 0.0);
    }

    #[test]
    fn additive_layer_stacks_on_top_of_the_base() {
        let cl = clips();
        let mut p = player();
        p.play(1, 2, 0.0);
        assert!((eval(&mut p, &cl) - 15.0).abs() < 1e-3, "additive layer did not stack");
    }

    #[test]
    fn additive_layer_respects_its_weight() {
        let cl = clips();
        let mut p = player();
        p.play(1, 2, 0.0);
        p.set_weight(1, 0.5);
        assert!((eval(&mut p, &cl) - 12.5).abs() < 1e-3);
    }

    #[test]
    fn a_zero_weight_layer_contributes_nothing() {
        let cl = clips();
        let mut p = player();
        p.play(1, 2, 0.0);
        p.set_weight(1, 0.0);
        assert!((eval(&mut p, &cl) - 10.0).abs() < 1e-3);
    }

    #[test]
    fn stopping_an_additive_layer_fades_it_back_to_rest() {
        let cl = clips();
        let mut p = player();
        p.play(1, 2, 0.0);
        p.stop(1, 1.0);
        p.update(&cl, 1.0);
        assert!((eval(&mut p, &cl) - 10.0).abs() < 1e-3);
        assert_eq!(p.current(1), None);
    }

    #[test]
    fn fading_in_an_additive_layer_ramps_up_from_rest() {
        let cl = clips();
        let mut p = player();
        p.play(1, 2, 1.0);
        // At t = 0 the layer contributes nothing yet.
        assert!((eval(&mut p, &cl) - 10.0).abs() < 1e-3);
        p.update(&cl, 1.0);
        assert!((eval(&mut p, &cl) - 15.0).abs() < 1e-3);
    }

    #[test]
    fn non_looping_clips_finish_and_hold_the_last_frame() {
        let mut cl = clips();
        cl[1].looping = false;
        let mut p = player();
        p.play(0, 1, 0.0);
        assert!(!p.finished(0));
        p.update(&cl, 5.0);
        assert!(p.finished(0));
        assert_eq!(p.time(0), cl[1].duration);
        assert!((eval(&mut p, &cl) - 20.0).abs() < 1e-3);
    }

    #[test]
    fn looping_clips_never_report_finished() {
        let cl = clips();
        let mut p = player();
        p.update(&cl, 100.0);
        assert!(!p.finished(0));
    }

    #[test]
    fn snap_drops_an_in_flight_fade() {
        let cl = clips();
        let mut p = player();
        p.play(0, 1, 1.0);
        p.snap();
        assert!(!p.fading(0));
        assert!((eval(&mut p, &cl) - 20.0).abs() < 1e-3);
    }

    #[test]
    fn update_ignores_pathological_dt() {
        let cl = clips();
        let mut p = player();
        p.update(&cl, f32::NAN);
        p.update(&cl, -1.0);
        p.update(&cl, 0.0);
        assert_eq!(p.time(0), 0.0);
    }

    #[test]
    fn play_default_returns_a_layer_home() {
        let mut p = player();
        p.play(0, 1, 0.0);
        p.play_default(0, 0.0);
        assert_eq!(p.current(0), Some(0));
        // A layer with no default stops instead.
        p.play(1, 2, 0.0);
        p.play_default(1, 0.0);
        assert_eq!(p.current(1), None);
    }

    #[test]
    fn a_fade_interrupted_by_another_fade_stays_bounded() {
        let cl = clips();
        let mut p = player();
        p.play(0, 1, 1.0);
        p.update(&cl, 0.3);
        p.play(0, 0, 1.0);
        p.update(&cl, 0.3);
        let v = eval(&mut p, &cl);
        assert!((10.0..=20.0).contains(&v), "interrupted fade left the range: {v}");
    }

    #[test]
    fn evaluate_does_not_allocate_after_construction() {
        // Proxy for the no-allocation claim: repeated evaluation must not grow
        // any internal buffer.
        let cl = clips();
        let mut p = player();
        let caps = (p.buf_cur.capacity(), p.buf_prev.capacity(), p.buf_layer.capacity());
        let mut out = vec![BoneOffsets::IDENTITY; 2];
        for _ in 0..500 {
            p.update(&cl, 1.0 / 60.0);
            p.evaluate(&cl, &mut out);
        }
        assert_eq!(
            caps,
            (p.buf_cur.capacity(), p.buf_prev.capacity(), p.buf_layer.capacity())
        );
    }
}
