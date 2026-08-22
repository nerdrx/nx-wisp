//! Motion quality (F67) — the difference between cute and cheap.
//!
//! Three reusable pieces, none of which know anything about the rig:
//!
//! * [`squash_from_velocity`] — squash and stretch driven by how fast she is
//!   moving, area-preserving so she does not appear to gain mass.
//! * [`Follower`] — an overshoot-and-settle follower. The visible body lags the
//!   physical anchor and rings past it on a direction change instead of
//!   snapping.
//! * [`SpringChain`] — secondary motion. A chain of spring-damped nodes with
//!   length constraints, so the tail arrives late and keeps going after she
//!   stops.
//!
//! All three are pure and deterministic: the same inputs give the same outputs
//! on every machine, which is why they can be unit-tested rather than reviewed
//! by eye.

use crate::ease::{SpringParams, Spring2, MAX_STEP};
use crate::math::{clamp, Vec2};

// ---------------------------------------------------------------------------
// Squash and stretch
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SquashParams {
    /// Stretch per unit of speed, in canvas units per second. Small: 400 px/s
    /// with a gain of 0.0006 gives a 24% stretch.
    pub gain: f32,
    /// Hard ceiling on the stretch factor minus one. 0.3 is a lot; beyond that
    /// she stops reading as a solid object.
    pub max: f32,
    /// Speed below which nothing happens at all, so a resting wobble does not
    /// make her breathe unevenly.
    pub deadzone: f32,
}

impl Default for SquashParams {
    fn default() -> Self {
        SquashParams { gain: 0.00055, max: 0.26, deadzone: 8.0 }
    }
}

/// An oriented squash: stretch `factor` along `angle`, compress by `1/factor`
/// across it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Squash {
    /// > 1 while moving, exactly 1 at rest.
    pub factor: f32,
    /// Direction of the stretch, radians.
    pub angle: f32,
}

impl Squash {
    pub const NONE: Squash = Squash { factor: 1.0, angle: 0.0 };

    /// Project onto the bone's own axes, for a rig that scales rather than
    /// rotating a dedicated squash bone.
    ///
    /// Exact for axis-aligned motion (`sx = factor, sy = 1/factor` when moving
    /// horizontally) and relaxes towards no deformation on the diagonal, which
    /// is the right answer visually — a diagonal stretch in an unrotated frame
    /// looks like a shear artefact, not like speed.
    pub fn axis_aligned(self) -> Vec2 {
        let (s, c) = self.angle.sin_cos();
        let (ax, ay) = (c.abs(), s.abs());
        let grow = self.factor - 1.0;
        let shrink = 1.0 - 1.0 / self.factor;
        Vec2::new(1.0 + grow * ax - shrink * ay, 1.0 + grow * ay - shrink * ax)
    }

    /// Area of the deformed unit square. Should stay ~1.
    pub fn area(self) -> f32 {
        let s = self.axis_aligned();
        s.x * s.y
    }
}

/// Squash and stretch from a velocity vector.
pub fn squash_from_velocity(vel: Vec2, p: SquashParams) -> Squash {
    let speed = vel.len();
    if !speed.is_finite() || speed <= p.deadzone.max(0.0) {
        return Squash::NONE;
    }
    let over = speed - p.deadzone.max(0.0);
    let amount = clamp(over * p.gain.max(0.0), 0.0, p.max.max(0.0));
    Squash { factor: 1.0 + amount, angle: vel.angle() }
}

/// Squash from an *impact* — a landing. Compresses across the impact direction
/// and is meant to be driven back to 1 by a spring over the next few frames.
pub fn squash_from_impact(impact_speed: f32, direction: Vec2, p: SquashParams) -> Squash {
    let s = impact_speed.abs();
    if !s.is_finite() || s <= p.deadzone.max(0.0) {
        return Squash::NONE;
    }
    let amount = clamp((s - p.deadzone.max(0.0)) * p.gain.max(0.0), 0.0, p.max.max(0.0));
    // A landing squashes *across* the direction of travel: the stretch axis is
    // perpendicular to the impact.
    Squash { factor: 1.0 + amount, angle: direction.perp().angle() }
}

// ---------------------------------------------------------------------------
// Overshoot / settle
// ---------------------------------------------------------------------------

/// A visible position that chases a physical one, arriving late and going
/// slightly too far.
///
/// The rig drives this with the physics body's position and renders the
/// *difference* as a bone offset, so she leans into a change of direction and
/// rings back rather than snapping to a new heading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Follower {
    spring: Spring2,
    pub params: SpringParams,
}

impl Follower {
    pub fn new(at: Vec2, params: SpringParams) -> Follower {
        Follower { spring: Spring2::new(at), params }
    }

    /// Default feel: noticeably underdamped, so a direction change rings once
    /// and settles inside a few hundred milliseconds.
    pub fn springy(at: Vec2) -> Follower {
        Follower::new(at, SpringParams::with_ratio(240.0, 1.0, 0.55))
    }

    pub fn value(&self) -> Vec2 {
        self.spring.value()
    }
    pub fn velocity(&self) -> Vec2 {
        self.spring.vel()
    }

    /// Advance and return the new visible position.
    pub fn follow(&mut self, target: Vec2, dt: f32) -> Vec2 {
        self.spring.step(target, self.params, dt);
        self.spring.value()
    }

    /// How far behind (or past) the target the visible position currently is.
    /// This is what the rig adds to the body bone.
    pub fn lag(&self, target: Vec2) -> Vec2 {
        self.spring.value() - target
    }

    /// Teleport: no ringing, no trail.
    pub fn reset(&mut self, at: Vec2) {
        self.spring.reset(at);
    }

    pub fn settled(&self, target: Vec2, eps: f32) -> bool {
        self.spring.settled(target, eps)
    }
}

// ---------------------------------------------------------------------------
// Secondary motion
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChainParams {
    /// How hard each node is pulled back towards where the pose wants it.
    pub stiffness: f32,
    pub damping: f32,
    pub mass: f32,
    /// Downwards acceleration on the trailing parts, canvas units/s². Small —
    /// a wisp-trail is nearly weightless.
    pub gravity: f32,
    /// Velocity decay per second, as an exponential rate. Air resistance.
    pub drag: f32,
    /// How strictly the distance between neighbours is preserved, `0..=1`.
    /// 1 makes the tail inextensible; lower lets it stretch on a hard fling.
    pub stiff_length: f32,
}

impl Default for ChainParams {
    fn default() -> Self {
        ChainParams {
            stiffness: 150.0,
            damping: 16.0,
            mass: 1.0,
            gravity: 60.0,
            drag: 1.6,
            stiff_length: 1.0,
        }
    }
}

/// A chain of spring-damped nodes that lags a driving pose.
///
/// Node 0 is pinned to the chain's root and never simulates. Every other node
/// is pulled towards the position the pose asks for, damped, dragged, pulled
/// down by a little gravity, and finally projected back onto its rest distance
/// from its parent so the tail cannot pull apart.
#[derive(Debug, Clone, PartialEq)]
pub struct SpringChain {
    pos: Vec<Vec2>,
    vel: Vec<Vec2>,
    rest_len: Vec<f32>,
}

impl SpringChain {
    /// `joints` are the world positions of the chain at rest, root first.
    pub fn new(joints: &[Vec2]) -> SpringChain {
        let rest_len = joints.windows(2).map(|w| w[0].dist(w[1])).collect();
        SpringChain {
            pos: joints.to_vec(),
            vel: vec![Vec2::ZERO; joints.len()],
            rest_len,
        }
    }

    pub fn len(&self) -> usize {
        self.pos.len()
    }
    pub fn is_empty(&self) -> bool {
        self.pos.is_empty()
    }
    pub fn positions(&self) -> &[Vec2] {
        &self.pos
    }
    pub fn velocities(&self) -> &[Vec2] {
        &self.vel
    }

    /// Snap the whole chain onto `joints` with no velocity. Used on a
    /// teleport, on a skin reload, and whenever the simulation would otherwise
    /// have to catch up across a gap nobody saw.
    pub fn snap_to(&mut self, joints: &[Vec2]) {
        for (i, j) in joints.iter().enumerate() {
            if i < self.pos.len() {
                self.pos[i] = *j;
                self.vel[i] = Vec2::ZERO;
            }
        }
    }

    /// Advance one frame. `targets` is where the pose (clips plus FK) wants
    /// each joint; `targets[0]` pins the root.
    ///
    /// `dt` is clamped and substepped exactly like [`crate::ease::Spring1`], so
    /// a stalled frame cannot detonate the tail.
    pub fn step(&mut self, targets: &[Vec2], p: ChainParams, dt: f32) {
        let n = self.pos.len().min(targets.len());
        if n == 0 || !dt.is_finite() || dt <= 0.0 {
            return;
        }
        let dt = dt.min(MAX_STEP);
        let mass = p.mass.max(1e-4);
        let rate = (p.stiffness.max(0.0) / mass)
            .sqrt()
            .max(p.damping.max(0.0) / mass)
            .max(p.drag.max(0.0))
            .max(1.0);
        let steps = ((dt / (0.15 / rate)).ceil() as usize).clamp(1, 128);
        let h = dt / steps as f32;
        let decay = (-p.drag.max(0.0) * h).exp();
        let g = Vec2::new(0.0, p.gravity);
        let stiff_length = clamp(p.stiff_length, 0.0, 1.0);

        for _ in 0..steps {
            // The root is driven, not simulated.
            self.pos[0] = targets[0];
            self.vel[0] = Vec2::ZERO;

            // Indexed on purpose: each step reads `targets` and writes two
            // parallel arrays at the same index.
            #[allow(clippy::needless_range_loop)]
            for i in 1..n {
                let to_target = targets[i] - self.pos[i];
                let accel = (to_target * p.stiffness - self.vel[i] * p.damping) / mass + g;
                self.vel[i] += accel * h;
                self.vel[i] = self.vel[i] * decay;
                self.pos[i] += self.vel[i] * h;
            }

            // Length constraints, root outwards. Only the child moves, which
            // keeps the root pinned and makes one pass enough.
            for i in 1..n {
                let rest = self.rest_len.get(i - 1).copied().unwrap_or(0.0);
                if rest <= 1e-5 {
                    continue;
                }
                let d = self.pos[i] - self.pos[i - 1];
                let len = d.len();
                if len <= 1e-6 {
                    // Collapsed onto its parent: push it out along the target
                    // direction rather than picking an arbitrary one.
                    let dir = (targets[i] - targets[i - 1]).normalize();
                    let dir = if dir == Vec2::ZERO { Vec2::new(0.0, 1.0) } else { dir };
                    self.pos[i] = self.pos[i - 1] + dir * rest;
                    continue;
                }
                let corrected = self.pos[i - 1] + d * (rest / len);
                let before = self.pos[i];
                self.pos[i] = before.lerp(corrected, stiff_length);
                // Fold the correction back into velocity so the constraint
                // does not silently inject energy.
                if h > 1e-9 {
                    self.vel[i] += (self.pos[i] - before) / h * 0.5;
                }
            }
        }

        #[allow(clippy::needless_range_loop)]
        for i in 0..n {
            if !self.pos[i].is_finite() || !self.vel[i].is_finite() {
                self.pos[i] = targets[i];
                self.vel[i] = Vec2::ZERO;
            }
        }
    }

    /// Largest distance between a node and where the pose wanted it. The rig
    /// uses this to decide the tail has stopped moving and it may sleep.
    pub fn max_deviation(&self, targets: &[Vec2]) -> f32 {
        self.pos
            .iter()
            .zip(targets.iter())
            .map(|(p, t)| p.dist(*t))
            .fold(0.0, f32::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- squash ------------------------------------------------------------

    #[test]
    fn at_rest_there_is_no_squash() {
        let s = squash_from_velocity(Vec2::ZERO, SquashParams::default());
        assert_eq!(s, Squash::NONE);
        assert_eq!(s.axis_aligned(), Vec2::ONE);
    }

    #[test]
    fn slow_motion_inside_the_deadzone_does_nothing() {
        let p = SquashParams { deadzone: 20.0, ..Default::default() };
        assert_eq!(squash_from_velocity(Vec2::new(5.0, 0.0), p), Squash::NONE);
    }

    #[test]
    fn stretch_grows_with_speed_and_is_capped() {
        let p = SquashParams::default();
        let slow = squash_from_velocity(Vec2::new(200.0, 0.0), p);
        let fast = squash_from_velocity(Vec2::new(900.0, 0.0), p);
        let absurd = squash_from_velocity(Vec2::new(100_000.0, 0.0), p);
        assert!(slow.factor > 1.0 && fast.factor > slow.factor);
        assert!((absurd.factor - (1.0 + p.max)).abs() < 1e-5, "cap not applied: {absurd:?}");
    }

    #[test]
    fn horizontal_motion_stretches_x_and_squashes_y() {
        let s = squash_from_velocity(Vec2::new(500.0, 0.0), SquashParams::default());
        let a = s.axis_aligned();
        assert!(a.x > 1.0 && a.y < 1.0, "{a:?}");
        assert!((a.x - s.factor).abs() < 1e-5);
        assert!((a.y - 1.0 / s.factor).abs() < 1e-5);
    }

    #[test]
    fn vertical_motion_stretches_y_and_squashes_x() {
        let s = squash_from_velocity(Vec2::new(0.0, 500.0), SquashParams::default());
        let a = s.axis_aligned();
        assert!(a.y > 1.0 && a.x < 1.0, "{a:?}");
    }

    #[test]
    fn axis_aligned_squash_roughly_preserves_area() {
        // She must not appear to gain or lose mass while moving.
        for deg in (0..360).step_by(7) {
            let a = (deg as f32).to_radians();
            let v = Vec2::from_angle(a) * 700.0;
            let s = squash_from_velocity(v, SquashParams::default());
            let area = s.area();
            assert!((area - 1.0).abs() < 0.09, "at {deg} deg area was {area}");
        }
    }

    #[test]
    fn diagonal_motion_relaxes_towards_neutral() {
        let p = SquashParams::default();
        let axis = squash_from_velocity(Vec2::new(700.0, 0.0), p).axis_aligned();
        let diag = squash_from_velocity(Vec2::new(495.0, 495.0), p).axis_aligned();
        assert!((diag.x - 1.0).abs() < (axis.x - 1.0).abs());
    }

    #[test]
    fn impact_squash_compresses_across_the_direction_of_travel() {
        // Landing after falling straight down: she should widen, not lengthen.
        let s = squash_from_impact(600.0, Vec2::new(0.0, 1.0), SquashParams::default());
        let a = s.axis_aligned();
        assert!(a.x > 1.0 && a.y < 1.0, "landing did not flatten her: {a:?}");
    }

    #[test]
    fn a_gentle_impact_does_nothing() {
        assert_eq!(
            squash_from_impact(1.0, Vec2::new(0.0, 1.0), SquashParams::default()),
            Squash::NONE
        );
    }

    #[test]
    fn squash_survives_non_finite_velocity() {
        let s = squash_from_velocity(Vec2::new(f32::NAN, 0.0), SquashParams::default());
        assert_eq!(s, Squash::NONE);
    }

    // -- follower ----------------------------------------------------------

    #[test]
    fn follower_lags_behind_a_moving_target() {
        let mut f = Follower::springy(Vec2::ZERO);
        let target = Vec2::new(100.0, 0.0);
        f.follow(target, 1.0 / 120.0);
        let lag = f.lag(target);
        assert!(lag.x < -50.0, "should still be far behind: {lag:?}");
    }

    #[test]
    fn follower_overshoots_on_a_direction_change_then_settles() {
        let mut f = Follower::springy(Vec2::ZERO);
        let dt = 1.0 / 240.0;
        for _ in 0..400 {
            f.follow(Vec2::new(100.0, 0.0), dt);
        }
        assert!(f.settled(Vec2::new(100.0, 0.0), 0.05));
        let mut min_x = f32::MAX;
        for _ in 0..800 {
            min_x = min_x.min(f.follow(Vec2::ZERO, dt).x);
        }
        assert!(min_x < -0.5, "no overshoot on reversal, min x {min_x}");
        assert!(f.settled(Vec2::ZERO, 0.05), "never settled: {:?}", f.value());
    }

    #[test]
    fn follower_reset_kills_the_ring() {
        let mut f = Follower::springy(Vec2::ZERO);
        f.follow(Vec2::new(500.0, 0.0), 0.05);
        f.reset(Vec2::new(9.0, 9.0));
        assert_eq!(f.value(), Vec2::new(9.0, 9.0));
        assert_eq!(f.velocity(), Vec2::ZERO);
    }

    #[test]
    fn follower_is_deterministic() {
        let run = || {
            let mut f = Follower::springy(Vec2::ZERO);
            for i in 0..200 {
                f.follow(Vec2::new(i as f32, (i % 13) as f32), 1.0 / 60.0);
            }
            f.value()
        };
        assert_eq!(run(), run());
    }

    // -- spring chain ------------------------------------------------------

    fn straight_chain(n: usize, spacing: f32) -> (SpringChain, Vec<Vec2>) {
        let joints: Vec<Vec2> = (0..n).map(|i| Vec2::new(0.0, i as f32 * spacing)).collect();
        (SpringChain::new(&joints), joints)
    }

    #[test]
    fn a_chain_at_rest_with_no_gravity_does_not_move() {
        let (mut c, joints) = straight_chain(4, 10.0);
        let p = ChainParams { gravity: 0.0, ..Default::default() };
        for _ in 0..200 {
            c.step(&joints, p, 1.0 / 60.0);
        }
        assert!(c.max_deviation(&joints) < 0.05, "drifted: {:?}", c.positions());
    }

    #[test]
    fn the_root_is_pinned_to_its_target() {
        let (mut c, mut joints) = straight_chain(4, 10.0);
        joints[0] = Vec2::new(50.0, 50.0);
        c.step(&joints, ChainParams::default(), 1.0 / 60.0);
        assert_eq!(c.positions()[0], Vec2::new(50.0, 50.0));
    }

    #[test]
    fn the_tail_arrives_late_when_the_root_moves() {
        let (mut c, joints) = straight_chain(4, 10.0);
        let moved: Vec<Vec2> = joints.iter().map(|j| *j + Vec2::new(80.0, 0.0)).collect();
        c.step(&moved, ChainParams::default(), 1.0 / 60.0);
        let pos = c.positions();
        // Each node further from the root should be further behind.
        let lag1 = pos[1].dist(moved[1]);
        let lag3 = pos[3].dist(moved[3]);
        assert!(lag1 > 0.5, "tip 1 did not lag at all");
        assert!(lag3 > lag1, "lag should grow along the chain: {lag1} then {lag3}");
    }

    #[test]
    fn the_tail_keeps_moving_after_the_root_stops() {
        let (mut c, joints) = straight_chain(4, 10.0);
        let moved: Vec<Vec2> = joints.iter().map(|j| *j + Vec2::new(120.0, 0.0)).collect();
        for _ in 0..6 {
            c.step(&moved, ChainParams::default(), 1.0 / 60.0);
        }
        let before = c.positions()[3];
        c.step(&moved, ChainParams::default(), 1.0 / 60.0);
        let after = c.positions()[3];
        assert!(before.dist(after) > 0.05, "tail froze the instant the root stopped");
    }

    #[test]
    fn the_chain_settles_onto_its_targets_eventually() {
        let (mut c, joints) = straight_chain(5, 12.0);
        let moved: Vec<Vec2> = joints.iter().map(|j| *j + Vec2::new(60.0, -30.0)).collect();
        let p = ChainParams { gravity: 0.0, ..Default::default() };
        for _ in 0..3000 {
            c.step(&moved, p, 1.0 / 120.0);
        }
        assert!(c.max_deviation(&moved) < 0.5, "never settled: {}", c.max_deviation(&moved));
    }

    #[test]
    fn length_constraints_keep_the_chain_from_pulling_apart() {
        let (mut c, joints) = straight_chain(5, 12.0);
        // Yank the root a long way, repeatedly, in alternating directions.
        for i in 0..300 {
            let x = if i % 2 == 0 { 900.0 } else { -900.0 };
            let moved: Vec<Vec2> = joints.iter().map(|j| *j + Vec2::new(x, 0.0)).collect();
            c.step(&moved, ChainParams::default(), 1.0 / 60.0);
            for k in 1..c.len() {
                let d = c.positions()[k].dist(c.positions()[k - 1]);
                assert!((d - 12.0).abs() < 0.6, "segment {k} stretched to {d} on frame {i}");
            }
        }
    }

    #[test]
    fn gravity_pulls_a_free_tail_downwards() {
        let (mut c, joints) = straight_chain(4, 10.0);
        let horizontal: Vec<Vec2> = (0..4).map(|i| Vec2::new(i as f32 * 10.0, 0.0)).collect();
        let _ = joints;
        c.snap_to(&horizontal);
        let p = ChainParams { gravity: 400.0, stiffness: 5.0, ..Default::default() };
        for _ in 0..60 {
            c.step(&horizontal, p, 1.0 / 60.0);
        }
        assert!(c.positions()[3].y > 1.0, "gravity did nothing: {:?}", c.positions());
    }

    #[test]
    fn a_stalled_frame_does_not_detonate_the_chain() {
        let (mut c, joints) = straight_chain(6, 10.0);
        let moved: Vec<Vec2> = joints.iter().map(|j| *j + Vec2::new(400.0, 0.0)).collect();
        c.step(&moved, ChainParams::default(), 4.0);
        for pnt in c.positions() {
            assert!(pnt.is_finite(), "NaN in the chain: {:?}", c.positions());
            assert!(pnt.len() < 5000.0, "chain exploded: {:?}", c.positions());
        }
    }

    #[test]
    fn chain_ignores_bad_dt() {
        let (mut c, joints) = straight_chain(3, 10.0);
        let before = c.positions().to_vec();
        c.step(&joints, ChainParams::default(), 0.0);
        c.step(&joints, ChainParams::default(), -1.0);
        c.step(&joints, ChainParams::default(), f32::NAN);
        assert_eq!(before, c.positions());
    }

    #[test]
    fn snap_to_clears_velocity() {
        let (mut c, joints) = straight_chain(3, 10.0);
        let moved: Vec<Vec2> = joints.iter().map(|j| *j + Vec2::new(200.0, 0.0)).collect();
        c.step(&moved, ChainParams::default(), 1.0 / 60.0);
        c.snap_to(&moved);
        assert!(c.velocities().iter().all(|v| *v == Vec2::ZERO));
        assert_eq!(c.positions(), moved.as_slice());
    }

    #[test]
    fn a_collapsed_segment_recovers_instead_of_producing_nan() {
        let joints = vec![Vec2::ZERO, Vec2::new(0.0, 10.0), Vec2::new(0.0, 20.0)];
        let mut c = SpringChain::new(&joints);
        // Force every node onto the same point.
        c.snap_to(&[Vec2::ZERO, Vec2::ZERO, Vec2::ZERO]);
        c.step(&joints, ChainParams::default(), 1.0 / 60.0);
        for pnt in c.positions() {
            assert!(pnt.is_finite());
        }
        assert!(c.positions()[1].dist(c.positions()[0]) > 1.0);
    }

    #[test]
    fn chain_step_is_deterministic() {
        let run = || {
            let (mut c, joints) = straight_chain(5, 11.0);
            for i in 0..120 {
                let moved: Vec<Vec2> = joints
                    .iter()
                    .map(|j| *j + Vec2::new((i as f32 * 0.3).sin() * 90.0, 0.0))
                    .collect();
                c.step(&moved, ChainParams::default(), 1.0 / 60.0);
            }
            c.positions().to_vec()
        };
        assert_eq!(run(), run());
    }
}
