//! Posing her at an arbitrary time, against the shipping rig.
//!
//! # Why this is a replay and not a sampler
//!
//! `wisp-rig` has no "give me the pose at t". [`wisp_rig::ClipPlayer`] advances
//! by a delta and has no `set_time`; `Rig::build_frame` — the thing that turns
//! a pose into drawable geometry — is private. Both are the right calls for a
//! per-frame renderer and both mean an editor cannot ask for an absolute time.
//! This is the adapter, and it lives here rather than in the rig because SPEC
//! §2 says `wisp-rig` owns that engine and this crate does not touch it.
//!
//! [`Preview::seek`] therefore **replays**: it rebuilds a `Rig` and steps it
//! forward in fixed increments to the time asked for. That is not a
//! compromise, it is the only correct answer — half of what she does is
//! spring-driven (the tails, the lean, the light inside her), and a spring's
//! state *is* its history. A sampler that jumped to t would show a pose the
//! runtime never produces. Replaying shows what actually happens, which is the
//! whole point of an editor that poses against the shipping renderer.
//!
//! It is also deterministic: the same `(clip, t)` always replays the same
//! fixed-step sequence from zero, so two scrubs to the same frame agree, and a
//! test can assert on a pose.
//!
//! Forward scrubs continue from where the last one stopped instead of starting
//! over, so dragging the playhead rightwards costs one step per frame rather
//! than N.
//!
//! # Canvas space
//!
//! A [`wisp_rig::RigFrame`] is in surface pixels. Give the rig
//! `size_px = the canvas's longest side` and `anchor = the canvas anchor`, and
//! its scale factor comes out as exactly 1 — so the frame's points are canvas
//! units and drop straight onto the editor's canvas with no second transform.
//! [`Preview::new`] does that.

use wisp_rig::math::Vec2;
use wisp_rig::rig::{Rig, RigInput};
use wisp_rig::skeleton::Pose;
use wisp_rig::{RigFrame, Skin};
use wisp_proto::Tier;

/// The replay step. 1/120 s: half a display frame, so the sampled pose is
/// never more than half a frame off what the runtime would have shown, and a
/// four-second clip replays in under five hundred steps.
pub const STEP_S: f32 = 1.0 / 120.0;

/// A safety valve on the replay. A clip longer than this is almost certainly a
/// typo'd `duration_ms`, and stepping through it would hang the editor.
pub const MAX_REPLAY_S: f32 = 120.0;

/// A rig posed at a time the operator picked.
pub struct Preview {
    skin: Skin,
    rig: Rig,
    input: RigInput,
    /// Where the replay currently stands, in seconds.
    at_s: f32,
    /// What was asked to play, so a change of clip forces a restart.
    playing: Option<(usize, usize)>,
    step_s: f32,
}

impl Preview {
    /// A preview whose frames come out in canvas units.
    pub fn new(skin: Skin) -> Preview {
        let size_px = skin
            .canvas
            .size
            .x
            .max(skin.canvas.size.y)
            .clamp(skin.meta.min_size_px, skin.meta.max_size_px);
        let anchor = skin.canvas.anchor;
        let rig = Rig::new(skin.clone());
        Preview {
            skin,
            rig,
            input: RigInput { size_px, anchor, on_ground: true, ..Default::default() },
            at_s: 0.0,
            playing: None,
            step_s: STEP_S,
        }
    }

    /// A preview at a real on-screen size, for the "what she looks like at
    /// 96 px" check that F73 makes the acceptance test.
    pub fn at_size(skin: Skin, size_px: f32, anchor: Vec2) -> Preview {
        let mut p = Preview::new(skin);
        p.input.size_px = size_px;
        p.input.anchor = anchor;
        p
    }

    pub fn skin(&self) -> &Skin {
        &self.skin
    }
    pub fn rig(&self) -> &Rig {
        &self.rig
    }
    pub fn rig_mut(&mut self) -> &mut Rig {
        &mut self.rig
    }
    pub fn frame(&self) -> &RigFrame {
        self.rig.frame()
    }
    pub fn pose(&self) -> &Pose {
        self.rig.pose()
    }
    pub fn input(&self) -> &RigInput {
        &self.input
    }
    pub fn input_mut(&mut self) -> &mut RigInput {
        &mut self.input
    }
    /// Where the replay currently stands, in milliseconds.
    pub fn time_ms(&self) -> f32 {
        self.at_s * 1000.0
    }

    /// Point the look-at IK somewhere, in the same space the frame is in.
    pub fn look_at(&mut self, target: Option<Vec2>) {
        self.input.cursor = target;
    }

    /// Throw the replay away and start again from the clip's first frame.
    pub fn restart(&mut self) {
        self.rig = Rig::new(self.skin.clone());
        self.at_s = 0.0;
        self.playing = None;
    }

    /// Pose her at `t_ms` of `clip`, on `layer`.
    ///
    /// Returns the number of steps the replay actually took, which the editor
    /// uses to decide whether the scrub was cheap enough to do every frame.
    pub fn seek(&mut self, layer: usize, clip: usize, t_ms: f32) -> usize {
        let target_s = (t_ms.max(0.0) / 1000.0).min(MAX_REPLAY_S);
        let want = (layer, clip);
        if self.playing != Some(want) || target_s < self.at_s {
            self.restart();
            self.rig.player_mut().play(layer, clip, 0.0);
            self.rig.player_mut().snap();
            self.rig.snap();
            self.playing = Some(want);
            // One zero-length step so the frame exists even at t = 0.
            self.rig.update(0.0, &self.input);
        }
        let mut steps = 0usize;
        while self.at_s + 1e-6 < target_s {
            let dt = self.step_s.min(target_s - self.at_s);
            self.rig.update(dt, &self.input);
            self.at_s += dt;
            steps += 1;
        }
        steps
    }

    /// Replay a cross-fade: play `from`, let it settle, then fade to `to` and
    /// stop `fade_ms + t_ms` later.
    ///
    /// This drives the real [`wisp_rig::ClipPlayer`] through the real `Rig`,
    /// which is why `tests/preview.rs` can assert that the editor's fade and a
    /// hand-driven player agree offset for offset.
    pub fn seek_crossfade(
        &mut self,
        layer: usize,
        from: usize,
        to: usize,
        fade_ms: f32,
        t_ms: f32,
    ) {
        self.restart();
        self.rig.player_mut().play(layer, from, 0.0);
        self.rig.player_mut().snap();
        self.rig.snap();
        self.rig.update(0.0, &self.input);
        self.rig.player_mut().play(layer, to, (fade_ms / 1000.0).max(0.0));
        self.playing = None; // a fade is not a plain playback; force a restart next seek
        let target_s = (t_ms.max(0.0) / 1000.0).min(MAX_REPLAY_S);
        while self.at_s + 1e-6 < target_s {
            let dt = self.step_s.min(target_s - self.at_s);
            self.rig.update(dt, &self.input);
            self.at_s += dt;
        }
    }

    /// Frames for the onion skin, one per `(time, alpha)` from
    /// [`crate::timeline::Onion::ghosts`].
    ///
    /// Each ghost is a full replay, so this is the expensive call in the
    /// editor and the reason onion skin is off by default.
    pub fn ghosts(&mut self, layer: usize, clip: usize, times: &[(f32, f32)]) -> Vec<Ghost> {
        let mut out = Vec::with_capacity(times.len());
        for &(t_ms, alpha) in times {
            self.seek(layer, clip, t_ms);
            out.push(Ghost { time_ms: t_ms, alpha, frame: self.rig.frame().clone() });
        }
        out
    }

    /// Set the tier the preview renders at, so the operator can see what she
    /// looks like once the governor has taken the springs away (SPEC §3.1).
    pub fn set_tier(&mut self, tier: Tier) {
        use wisp_proto::{Governed, TierReason};
        self.rig.set_tier(tier, &TierReason::Pinned);
    }
}

/// One onion-skin ghost.
#[derive(Debug, Clone)]
pub struct Ghost {
    pub time_ms: f32,
    pub alpha: f32,
    pub frame: RigFrame,
}
