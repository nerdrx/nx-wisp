//! The rig: a skin plus everything that moves it (F10, F67–F75).
//!
//! One `update` per frame does, in order:
//!
//! 1. advance and compose the clip layers (base, breathing, blinking, the
//!    current expression),
//! 2. apply the procedural motion layer — velocity squash, the overshoot lean,
//!    the internal light's displacement,
//! 3. resolve the bone hierarchy,
//! 4. run IK: look-at towards the cursor or whatever has her attention,
//! 5. simulate the secondary-motion chains and aim their bones at the result,
//! 6. skin every shape's points straight into surface pixels,
//! 7. resolve gradients, including the ones that follow a bone.
//!
//! Steps 2, 5 and part of 7 are what make her read as alive rather than as a
//! looping animation, and they are exactly the parts the governor sheds first
//! (see the [`Governed`] implementation at the bottom).
//!
//! The rig owns no clock, no window and no GPU. `update` takes a `dt` and an
//! input struct; physics lives in [`crate::physics`] and is stepped by the
//! shell, because the shell is what knows where the operator's windows are.

use wisp_proto::{Cost, Governed, Tier, TierReason};

use crate::contour::{trace, ContourOptions, Polygon};
use crate::deform::skin_points;
use crate::ease::Spring1;
use crate::frame::{DrawShape, RigFrame};
use crate::ik::{aim_bone_at, apply_look_at, apply_two_bone};
use crate::math::{clamp, Affine, Vec2};
use crate::motion::{squash_from_velocity, ChainParams, Follower, SpringChain};
use crate::paint::{sample_stops, LinearGradient, Paint, RadialGradient, Stroke};
use crate::player::ClipPlayer;
use crate::skeleton::{BoneOffsets, Pose};
use crate::skin::{GradientGeom, IkKind, IkTarget, PaintRef, Skin};

/// Everything the rig needs to know about the world this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RigInput {
    /// Rendered size in surface pixels — F75's slider.
    pub size_px: f32,
    /// Where her canvas anchor sits, in surface pixels.
    pub anchor: Vec2,
    /// Her velocity in surface pixels per second. Normally
    /// [`crate::physics::BodyState::vel`].
    pub velocity: Vec2,
    /// The pointer, in surface pixels.
    pub cursor: Option<Vec2>,
    /// Whatever currently has her attention — a notification, the active
    /// window, the operator (F69).
    pub attention: Option<Vec2>,
    /// True while the pointer is holding her.
    pub grabbed: bool,
    /// True while she is resting on something.
    pub on_ground: bool,
}

impl Default for RigInput {
    fn default() -> Self {
        RigInput {
            size_px: 128.0,
            anchor: Vec2::ZERO,
            velocity: Vec2::ZERO,
            cursor: None,
            attention: None,
            grabbed: false,
            on_ground: true,
        }
    }
}

/// Which optional work the rig is doing, set by the governor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detail {
    /// Secondary motion — the trailing tail. First thing to go.
    pub secondary_motion: bool,
    /// IK look-at.
    pub look_at: bool,
    /// Velocity squash and the overshoot lean.
    pub procedural_squash: bool,
    /// Cross-fades between clips. Off means clips cut.
    pub crossfade: bool,
    /// Samples per curve when flattening for the contour.
    pub curve_samples: usize,
}

impl Detail {
    pub const FULL: Detail = Detail {
        secondary_motion: true,
        look_at: true,
        procedural_squash: true,
        crossfade: true,
        curve_samples: 8,
    };
    pub const REDUCED: Detail = Detail {
        secondary_motion: true,
        look_at: true,
        procedural_squash: true,
        crossfade: true,
        curve_samples: 5,
    };
    /// T3: the sprite-atlas tier. The vector rig still runs — the atlas baker
    /// needs it — but everything that costs a simulation is off.
    pub const MINIMAL: Detail = Detail {
        secondary_motion: false,
        look_at: false,
        procedural_squash: false,
        crossfade: false,
        curve_samples: 3,
    };

    pub fn for_tier(tier: Tier) -> Detail {
        match tier {
            Tier::Feral | Tier::Full => Detail::FULL,
            Tier::Reduced => Detail::REDUCED,
            Tier::Lobotomised | Tier::Dormant => Detail::MINIMAL,
        }
    }
}

impl Default for Detail {
    fn default() -> Self {
        Detail::FULL
    }
}

struct ChainSim {
    sim: SpringChain,
    params: ChainParams,
    bones: Vec<usize>,
    /// Scratch for the pose's idea of where the joints should be.
    targets: Vec<Vec2>,
}

pub struct Rig {
    skin: Skin,
    pose: Pose,
    player: ClipPlayer,
    frame: RigFrame,
    detail: Detail,
    tier: Tier,

    // Procedural motion state.
    lean: Follower,
    squash_x: Spring1,
    squash_y: Spring1,
    light: Follower,
    chains: Vec<ChainSim>,

    // Scratch, allocated once.
    offsets: Vec<BoneOffsets>,
    render_mat: Vec<Affine>,
    rings: Vec<Vec<Vec2>>,
    flatten_buf: Vec<Vec<Vec2>>,

    size_px: f32,
    anchor: Vec2,
    /// Seconds since the rig started. Deterministic, monotonic.
    elapsed: f32,
    /// True once the pose has been placed at least once.
    placed: bool,
}

impl Rig {
    pub fn new(skin: Skin) -> Rig {
        let n = skin.skeleton.len();
        let pose = skin.skeleton.rest_pose();
        let player = ClipPlayer::new(skin.layers.clone(), n);

        let chains = skin
            .chains
            .iter()
            .map(|c| {
                let joints = chain_joints(&skin, &pose, &c.bones);
                ChainSim {
                    sim: SpringChain::new(&joints),
                    params: c.params,
                    bones: c.bones.clone(),
                    targets: joints,
                }
            })
            .collect();

        let mut frame = RigFrame {
            size_px: skin.meta.default_size_px,
            scale: 1.0,
            anchor: Vec2::ZERO,
            shapes: Vec::with_capacity(skin.shapes.len()),
            bounds: crate::math::Rect::EMPTY,
        };
        for s in &skin.shapes {
            frame.shapes.push(DrawShape {
                name: s.name.clone(),
                z: s.z,
                opacity: s.opacity,
                silhouette: s.silhouette,
                fill_rule: s.fill_rule,
                verbs: s.path.verbs.clone(),
                points: vec![Vec2::ZERO; s.path.point_count()],
                fill: s.fill.map(|p| blank_paint(&skin, p)),
                stroke: s.stroke.map(|st| Stroke {
                    paint: blank_paint(&skin, st.paint),
                    width: st.width,
                    cap: st.cap,
                    join: st.join,
                }),
            });
        }

        let size_px = skin.meta.default_size_px;
        Rig {
            offsets: vec![BoneOffsets::IDENTITY; n],
            render_mat: vec![Affine::IDENTITY; n],
            rings: Vec::new(),
            flatten_buf: Vec::new(),
            lean: Follower::new(Vec2::ZERO, skin.motion.lean),
            squash_x: Spring1::new(1.0),
            squash_y: Spring1::new(1.0),
            light: Follower::new(Vec2::ZERO, skin.motion.lean),
            chains,
            pose,
            player,
            frame,
            detail: Detail::FULL,
            tier: Tier::Full,
            size_px,
            anchor: Vec2::ZERO,
            elapsed: 0.0,
            placed: false,
            skin,
        }
    }

    pub fn skin(&self) -> &Skin {
        &self.skin
    }
    pub fn frame(&self) -> &RigFrame {
        &self.frame
    }
    pub fn pose(&self) -> &Pose {
        &self.pose
    }
    pub fn detail(&self) -> Detail {
        self.detail
    }
    pub fn tier(&self) -> Tier {
        self.tier
    }
    pub fn elapsed(&self) -> f32 {
        self.elapsed
    }

    /// Play a clip by name on the base layer.
    pub fn play(&mut self, clip: &str, fade_ms: f32) -> bool {
        match self.skin.clip_index(clip) {
            Some(c) => {
                self.player.play(0, c, self.fade(fade_ms));
                true
            }
            None => false,
        }
    }

    /// Play a clip by name on a named layer.
    pub fn play_on(&mut self, layer: &str, clip: &str, fade_ms: f32) -> bool {
        let (Some(l), Some(c)) = (self.skin.layer_index(layer), self.skin.clip_index(clip))
        else {
            return false;
        };
        self.player.play(l, c, self.fade(fade_ms));
        true
    }

    /// Set the current expression (F74). Unknown names are refused rather than
    /// silently ignored, so the mood FSM finds out it asked for something the
    /// skin does not have.
    pub fn set_expression(&mut self, name: &str) -> bool {
        let Some(i) = self.skin.expression_index(name) else {
            return false;
        };
        let e = &self.skin.expressions[i];
        let (layer, clip, weight, fade) = (e.layer, e.clip, e.weight, e.fade);
        self.player.set_weight(layer, weight);
        let fade = if self.detail.crossfade { fade } else { 0.0 };
        self.player.play(layer, clip, fade);
        true
    }

    /// Which expression is showing, if the current clip on any layer is one.
    pub fn current_expression(&self) -> Option<&str> {
        self.skin.expressions.iter().find_map(|e| {
            (self.player.current(e.layer) == Some(e.clip)).then_some(&*e.name)
        })
    }

    pub fn player(&self) -> &ClipPlayer {
        &self.player
    }
    pub fn player_mut(&mut self) -> &mut ClipPlayer {
        &mut self.player
    }

    fn fade(&self, ms: f32) -> f32 {
        if self.detail.crossfade {
            (ms / 1000.0).max(0.0)
        } else {
            0.0
        }
    }

    /// Drop every spring, chain and fade onto its target immediately. Call
    /// after a teleport, an output change, or a wake from dormant — anything
    /// where continuing to settle would animate motion nobody saw.
    pub fn snap(&mut self) {
        self.player.snap();
        self.lean.reset(self.anchor);
        self.light.reset(Vec2::ZERO);
        self.squash_x.reset(1.0);
        self.squash_y.reset(1.0);
        self.pose.reset_offsets();
        self.pose.resolve(&self.skin.skeleton);
        for c in &mut self.chains {
            let joints = chain_joints_from(&self.pose, &self.skin, &c.bones);
            c.sim.snap_to(&joints);
        }
    }

    /// Advance the rig by `dt` seconds and rebuild the frame.
    pub fn update(&mut self, dt: f32, input: &RigInput) {
        let dt = if dt.is_finite() { clamp(dt, 0.0, crate::ease::MAX_STEP) } else { 0.0 };
        self.elapsed += dt;
        self.size_px = clamp(input.size_px, self.skin.meta.min_size_px, self.skin.meta.max_size_px);
        self.anchor = input.anchor;
        let scale = self.skin.scale_for(self.size_px);
        if !self.placed {
            self.lean.reset(input.anchor);
            self.placed = true;
        }

        // 1 — clips.
        self.player.update(&self.skin.clips, dt);
        self.player.evaluate(&self.skin.clips, &mut self.offsets);
        self.pose.offsets.copy_from_slice(&self.offsets);

        // 2 — procedural motion, in canvas units.
        let vel_canvas = if scale > 1e-6 { input.velocity / scale } else { Vec2::ZERO };
        self.apply_squash(vel_canvas, dt);
        self.apply_lean(input.anchor, scale, dt);
        self.apply_light(vel_canvas, dt);

        // 3 — hierarchy.
        self.pose.resolve(&self.skin.skeleton);

        // 4 — IK.
        if self.detail.look_at {
            self.apply_ik(input, scale);
        }

        // 5 — secondary motion.
        if self.detail.secondary_motion {
            self.apply_chains(dt);
        }

        // 6/7 — geometry and paint.
        self.build_frame(scale, input.anchor);
    }

    fn apply_squash(&mut self, vel_canvas: Vec2, dt: f32) {
        let Some(bone) = self.skin.motion.squash_bone else {
            return;
        };
        let (target_x, target_y) = if self.detail.procedural_squash {
            let s = squash_from_velocity(vel_canvas, self.skin.motion.squash).axis_aligned();
            (s.x, s.y)
        } else {
            (1.0, 1.0)
        };
        // Smoothed so a jittery velocity does not make her flicker, and so the
        // stretch decays after she stops instead of vanishing.
        let p = self.skin.motion.lean;
        self.squash_x.step(target_x, p, dt);
        self.squash_y.step(target_y, p, dt);
        self.pose.offsets[bone].sx *= self.squash_x.value;
        self.pose.offsets[bone].sy *= self.squash_y.value;
    }

    fn apply_lean(&mut self, anchor: Vec2, scale: f32, dt: f32) {
        let Some(bone) = self.skin.motion.lean_bone else {
            return;
        };
        self.lean.params = self.skin.motion.lean;
        self.lean.follow(anchor, dt);
        if !self.detail.procedural_squash {
            return;
        }
        // The visible body sits where the spring got to, not where she
        // actually is — so she leans into a turn and rings past it.
        let lag_px = self.lean.lag(anchor) * self.skin.motion.lean_gain;
        let lag_canvas = if scale > 1e-6 { lag_px / scale } else { Vec2::ZERO };
        self.pose.offsets[bone].tx += lag_canvas.x;
        self.pose.offsets[bone].ty += lag_canvas.y;
    }

    fn apply_light(&mut self, vel_canvas: Vec2, dt: f32) {
        let Some(bone) = self.skin.motion.light_bone else {
            return;
        };
        // DESIGN.md §1: light rides motion, it never flashes on command. The
        // internal mote is displaced *against* her travel, so the highlight
        // slides through the glass as she moves and drifts back when she stops.
        let range = self.skin.motion.light_range;
        let want = if self.detail.procedural_squash {
            let raw = -vel_canvas * self.skin.motion.light_gain;
            let l = raw.len();
            if l > range && l > 1e-6 {
                raw * (range / l)
            } else {
                raw
            }
        } else {
            Vec2::ZERO
        };
        self.light.params = self.skin.motion.lean;
        self.light.follow(want, dt);
        let v = self.light.value();
        self.pose.offsets[bone].tx += v.x;
        self.pose.offsets[bone].ty += v.y;
    }

    fn apply_ik(&mut self, input: &RigInput, scale: f32) {
        for i in 0..self.skin.iks.len() {
            let def = &self.skin.iks[i];
            let target_px = match def.target {
                IkTarget::Cursor => input.cursor,
                IkTarget::Attention => input.attention.or(input.cursor),
                IkTarget::None => None,
            };
            let Some(target_px) = target_px else {
                continue;
            };
            // IK works in canvas space; the input is in surface pixels.
            let target = if scale > 1e-6 {
                self.skin.canvas.anchor + (target_px - self.anchor) / scale
            } else {
                self.skin.canvas.anchor
            };
            match def.kind {
                IkKind::LookAt { bone, cfg } => {
                    apply_look_at(&self.skin.skeleton, &mut self.pose, bone, target, cfg);
                }
                IkKind::TwoBone { root, mid, end, bend_positive } => {
                    apply_two_bone(
                        &self.skin.skeleton,
                        &mut self.pose,
                        root,
                        mid,
                        end,
                        target,
                        bend_positive,
                        1.0,
                    );
                }
            }
        }
    }

    fn apply_chains(&mut self, dt: f32) {
        for ci in 0..self.chains.len() {
            // Where the pose wants each joint: one entry per bone, plus the
            // last bone's tip so the final segment has something to aim at.
            let mut targets = std::mem::take(&mut self.chains[ci].targets);
            targets.clear();
            {
                let bones = &self.chains[ci].bones;
                for (k, &b) in bones.iter().enumerate() {
                    targets.push(self.pose.world_pos(b));
                    if k + 1 == bones.len() {
                        targets.push(self.pose.world_tip(&self.skin.skeleton, b));
                    }
                }
            }
            let params = self.chains[ci].params;
            self.chains[ci].sim.step(&targets, params, dt);
            self.chains[ci].targets = targets;

            // ...and where the simulation actually put them. Turning each bone
            // to face the next simulated joint is what converts a position
            // simulation back into a pose.
            let joints = self.chains[ci].sim.len();
            for k in 0..self.chains[ci].bones.len() {
                if k + 1 >= joints {
                    break;
                }
                let bone = self.chains[ci].bones[k];
                let next = self.chains[ci].sim.positions()[k + 1];
                aim_bone_at(&self.skin.skeleton, &mut self.pose, bone, next);
            }
        }
    }

    fn build_frame(&mut self, scale: f32, anchor: Vec2) {
        // Canvas → surface: put the canvas anchor at her screen position and
        // scale around it. Folding it into the skinning matrices means the
        // deformed points come out in surface pixels with no second pass.
        let view = Affine::translate(anchor)
            .mul(Affine::scale(Vec2::splat(scale)))
            .mul(Affine::translate(-self.skin.canvas.anchor));
        for (i, m) in self.pose.skin_mat.iter().enumerate() {
            self.render_mat[i] = view.mul(*m);
        }

        for (i, def) in self.skin.shapes.iter().enumerate() {
            let out = &mut self.frame.shapes[i];
            skin_points(&def.path.points, &def.binding, &self.render_mat, &mut out.points);
            // Bone alpha follows the shape's first influence — a shape spans
            // one body part, and a per-point alpha has nowhere to go in a
            // single draw call.
            let bone_alpha = def
                .binding
                .influences_of(0)
                .first()
                .and_then(|inf| self.pose.alpha.get(inf.bone as usize))
                .copied()
                .unwrap_or(1.0);
            out.opacity = clamp(def.opacity * bone_alpha, 0.0, 1.0);
        }

        // Gradients last: a following gradient's geometry depends on the pose.
        for (i, def) in self.skin.shapes.iter().enumerate() {
            if let Some(p) = def.fill {
                let dst = self.frame.shapes[i].fill.as_mut();
                Self::resolve_paint(&self.skin, &self.pose, view, scale, p, dst);
            }
            if let Some(st) = def.stroke {
                if let Some(dst) = self.frame.shapes[i].stroke.as_mut() {
                    dst.width = st.width * scale;
                    Self::resolve_paint(
                        &self.skin,
                        &self.pose,
                        view,
                        scale,
                        st.paint,
                        Some(&mut dst.paint),
                    );
                }
            }
        }

        self.frame.size_px = self.size_px;
        self.frame.scale = scale;
        self.frame.anchor = anchor;
        self.frame.recompute_bounds();
    }

    fn resolve_paint(
        skin: &Skin,
        pose: &Pose,
        view: Affine,
        scale: f32,
        src: PaintRef,
        dst: Option<&mut Paint>,
    ) {
        let Some(dst) = dst else {
            return;
        };
        match src {
            PaintRef::Solid(_) => {}
            PaintRef::Gradient { index, .. } => {
                let g = &skin.gradients[index];
                // A following gradient rides its bone's displacement from rest.
                let shift = match g.follow_bone {
                    Some(b) => pose.world[b].origin() - skin.skeleton.rest_world()[b].origin(),
                    None => Vec2::ZERO,
                };
                match (&g.geom, dst) {
                    (GradientGeom::Linear { start, end }, Paint::Linear(out)) => {
                        out.start = view.apply(*start + shift);
                        out.end = view.apply(*end + shift);
                    }
                    (GradientGeom::Radial { center, focus, radius }, Paint::Radial(out)) => {
                        out.center = view.apply(*center + shift);
                        out.focus = view.apply(*focus + shift);
                        out.radius = radius * scale;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Trace the click-through outline of the current frame (F2).
    ///
    /// Costs a small rasterisation, so the shell should call it only when the
    /// silhouette has actually changed, not on every frame she breathes.
    pub fn contour(&mut self, opts: ContourOptions) -> Polygon {
        self.rings.clear();
        for s in &self.frame.shapes {
            if !s.silhouette || !s.is_visible() {
                continue;
            }
            s.flatten_into(self.detail.curve_samples, &mut self.flatten_buf);
            self.rings.append(&mut self.flatten_buf);
        }
        trace(&self.rings, opts)
    }
}

fn chain_joints(skin: &Skin, pose: &Pose, bones: &[usize]) -> Vec<Vec2> {
    chain_joints_from(pose, skin, bones)
}

fn chain_joints_from(pose: &Pose, skin: &Skin, bones: &[usize]) -> Vec<Vec2> {
    let mut v: Vec<Vec2> = bones.iter().map(|&b| pose.world_pos(b)).collect();
    if let Some(&last) = bones.last() {
        v.push(pose.world_tip(&skin.skeleton, last));
    }
    v
}

/// Build the frame's paint with its stop list in place; the geometry is
/// overwritten every frame, the stops never are.
fn blank_paint(skin: &Skin, p: PaintRef) -> Paint {
    match p {
        PaintRef::Solid(c) => Paint::Solid(c),
        PaintRef::Gradient { index, alpha } => {
            let g = &skin.gradients[index];
            let stops: Vec<_> = g
                .stops
                .iter()
                .map(|s| crate::paint::GradientStop { at: s.at, color: s.color.scale_alpha(alpha) })
                .collect();
            match g.geom {
                GradientGeom::Linear { start, end } => Paint::Linear(LinearGradient {
                    start,
                    end,
                    extend: g.extend,
                    stops,
                }),
                GradientGeom::Radial { center, focus, radius } => {
                    Paint::Radial(RadialGradient {
                        center,
                        focus,
                        radius,
                        extend: g.extend,
                        stops,
                    })
                }
            }
        }
    }
}

/// Average colour of a paint, for the sprite-atlas baker and for tests that
/// want to assert "this shape is violet" without a renderer.
pub fn average_color(p: &Paint) -> crate::paint::Rgba {
    match p {
        Paint::Solid(c) => *c,
        Paint::Linear(g) => average_stops(&g.stops),
        Paint::Radial(g) => average_stops(&g.stops),
    }
}

fn average_stops(stops: &[crate::paint::GradientStop]) -> crate::paint::Rgba {
    let n = 9;
    let mut acc = crate::paint::Rgba::new(0.0, 0.0, 0.0, 0.0);
    for i in 0..n {
        let c = sample_stops(stops, i as f32 / (n - 1) as f32);
        acc.r += c.r;
        acc.g += c.g;
        acc.b += c.b;
        acc.a += c.a;
    }
    let k = 1.0 / n as f32;
    crate::paint::Rgba::new(acc.r * k, acc.g * k, acc.b * k, acc.a * k)
}

/// SPEC §3.1. Downgrades are applied immediately and cannot fail: the rig
/// sheds simulations rather than queueing them.
impl Governed for Rig {
    fn set_tier(&mut self, tier: Tier, _reason: &TierReason) {
        if self.tier == tier {
            return;
        }
        let was = self.detail;
        self.tier = tier;
        self.detail = Detail::for_tier(tier);
        // Shedding a simulation means putting it back at rest *now*, not
        // leaving it frozen mid-swing.
        if was.secondary_motion && !self.detail.secondary_motion {
            for c in &mut self.chains {
                let joints = chain_joints_from(&self.pose, &self.skin, &c.bones);
                c.sim.snap_to(&joints);
            }
        }
        if was.crossfade && !self.detail.crossfade {
            self.player.snap();
        }
        if was.procedural_squash && !self.detail.procedural_squash {
            self.squash_x.reset(1.0);
            self.squash_y.reset(1.0);
            self.light.reset(Vec2::ZERO);
            self.lean.reset(self.anchor);
        }
    }

    fn cost_at(tier: Tier) -> Cost {
        // The vector rig is CPU-side geometry only; its VRAM is the renderer's
        // to account for. Numbers are worst case for a skin of this scale.
        match tier {
            Tier::Feral | Tier::Full => Cost { ram_mib: 3, vram_mib: 0, cpu_centi_pct: 90 },
            Tier::Reduced => Cost { ram_mib: 3, vram_mib: 0, cpu_centi_pct: 45 },
            Tier::Lobotomised => Cost { ram_mib: 2, vram_mib: 0, cpu_centi_pct: 12 },
            Tier::Dormant => Cost::FREE,
        }
    }
}
