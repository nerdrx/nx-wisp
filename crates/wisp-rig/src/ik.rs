//! Inverse kinematics: a two-bone analytic solver and a look-at constraint.
//!
//! These are the F69 "head/eyes track the cursor" primitives. Both are pure
//! functions over a pose — they compute rotations, the caller writes them into
//! `Pose::offsets` and re-resolves. Nothing here allocates.
//!
//! **Unreachable targets are a normal case, not an error.** A cursor at the
//! far corner of a 4K screen is out of range of a 40px neck every time the
//! operator moves the mouse. The solvers degrade to "reach as far as you can
//! in the right direction" and report `reachable: false` so the caller can
//! decide whether that deserves a lean, a step, or nothing at all.

use crate::math::{angle_delta, clamp, wrap_angle, Vec2};
use crate::skeleton::{Pose, Skeleton};

/// A solved two-bone chain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TwoBoneSolution {
    /// World-space rotation for the root bone.
    pub root_world_rot: f32,
    /// Local rotation for the middle bone, relative to the root.
    pub mid_local_rot: f32,
    /// False when the target is beyond `l1 + l2` or inside `|l1 - l2|`. The
    /// chain still points at it; it just cannot touch it.
    pub reachable: bool,
}

/// Analytic two-bone IK in world space.
///
/// `root` is the chain's origin, `l1`/`l2` the bone lengths, `target` where the
/// end effector should land. `bend_positive` picks which of the two mirror
/// solutions to take — the elbow's side.
pub fn solve_two_bone(
    root: Vec2,
    l1: f32,
    l2: f32,
    target: Vec2,
    bend_positive: bool,
) -> TwoBoneSolution {
    let l1 = l1.max(1e-5);
    let l2 = l2.max(1e-5);
    let to_target = target - root;
    let raw_dist = to_target.len();

    // A target exactly on the root has no direction: hold the chain straight
    // along +x rather than producing NaN.
    let dir_angle = if raw_dist <= 1e-6 { 0.0 } else { to_target.angle() };

    let reach_max = l1 + l2;
    let reach_min = (l1 - l2).abs();
    let reachable = raw_dist <= reach_max && raw_dist >= reach_min;
    // Clamp into the annulus the chain can actually reach. Nudged inwards so
    // the acos arguments stay strictly inside [-1, 1] under float error.
    let d = clamp(raw_dist, reach_min + 1e-4, reach_max - 1e-4).max(1e-5);

    let cos_a = clamp((l1 * l1 + d * d - l2 * l2) / (2.0 * l1 * d), -1.0, 1.0);
    let a = cos_a.acos();
    let cos_b = clamp((l1 * l1 + l2 * l2 - d * d) / (2.0 * l1 * l2), -1.0, 1.0);
    let b = cos_b.acos();

    let sign = if bend_positive { 1.0 } else { -1.0 };
    TwoBoneSolution {
        root_world_rot: wrap_angle(dir_angle - sign * a),
        mid_local_rot: wrap_angle(sign * (std::f32::consts::PI - b)),
        reachable,
    }
}

/// Segment length and direction from a bone's origin to its child's origin, as
/// authored at rest. IK measures the chain by where the joints actually are,
/// not by the `length` field, so a rig whose bones are not laid out along
/// their own +x axis still solves correctly. `length` is the fallback for a
/// child sitting exactly on its parent's origin.
fn segment(sk: &Skeleton, parent: usize, child: usize) -> (f32, f32) {
    let off = sk.bone(child).rest.pos;
    if off.len() > 1e-4 {
        (off.len(), off.angle())
    } else {
        (sk.bone(parent).length.max(1e-4), 0.0)
    }
}

/// Apply [`solve_two_bone`] to a real chain and write the result into the pose.
///
/// `root`, `mid` and `end` must be a contiguous parent chain; skin validation
/// guarantees that. The solver **owns** the rotation channel of `root` and
/// `mid` — like every IK constraint, it replaces what the clips asked for
/// rather than adding to it, and `weight` blends between the two. Anything you
/// want to keep animating by hand belongs on a bone outside the chain.
///
/// Returns the solution, including whether the target was in range.
#[allow(clippy::too_many_arguments)]
pub fn apply_two_bone(
    sk: &Skeleton,
    pose: &mut Pose,
    root: usize,
    mid: usize,
    end: usize,
    target: Vec2,
    bend_positive: bool,
    weight: f32,
) -> TwoBoneSolution {
    let (l1, phi1) = segment(sk, root, mid);
    let (l2, phi2) = segment(sk, mid, end);
    let origin = pose.world_pos(root);
    let sol = solve_two_bone(origin, l1, l2, target, bend_positive);
    let w = clamp(weight, 0.0, 1.0);
    if w <= 1e-4 {
        return sol;
    }

    let parent_rot = match sk.bone(root).parent {
        Some(p) => pose.world[p].rotation(),
        None => 0.0,
    };
    // `sol` gives the world direction each *segment* should point. Back out the
    // bone rotations that put the segments there.
    let root_world = sol.root_world_rot - phi1;
    let mid_world = sol.root_world_rot + sol.mid_local_rot - phi2;
    let want_root = wrap_angle(root_world - parent_rot - sk.bone(root).rest.rot);
    let want_mid = wrap_angle(mid_world - root_world - sk.bone(mid).rest.rot);

    pose.offsets[root].rot += angle_delta(pose.offsets[root].rot, want_root) * w;
    pose.offsets[mid].rot += angle_delta(pose.offsets[mid].rot, want_mid) * w;
    pose.resolve_from(sk, root);
    sol
}

/// A look-at constraint: rotate one bone so its forward axis points at a
/// target, clamped to a cone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LookAt {
    /// The bone's forward direction in its own local space. `(1, 0)` is along
    /// the bone; `(0, -1)` is "up" in a y-down canvas.
    pub forward: Vec2,
    /// Maximum rotation away from rest, radians. Keeps a neck from spinning.
    pub max_angle: f32,
    /// 0 ignores the target entirely, 1 tracks it fully.
    pub weight: f32,
}

impl Default for LookAt {
    fn default() -> Self {
        LookAt { forward: Vec2::new(0.0, -1.0), max_angle: 0.5, weight: 1.0 }
    }
}

/// Compute the local rotation offset a look-at wants, without applying it.
///
/// The result is **absolute**: it is the offset from the bone's rest rotation,
/// already clamped to the cone and scaled by the weight. Writing it (rather
/// than adding it) is what makes the constraint idempotent — applying it twice
/// in a frame leaves the head where it was instead of doubling the turn.
///
/// Returns `None` when the target gives no usable direction (it sits on the
/// bone's own origin), which is the graceful case the caller should treat as
/// "make no change" rather than as an error.
pub fn look_at_offset(
    sk: &Skeleton,
    pose: &Pose,
    bone: usize,
    target: Vec2,
    cfg: LookAt,
) -> Option<f32> {
    let origin = pose.world_pos(bone);
    let to_target = target - origin;
    if to_target.len_sq() <= 1e-8 {
        return None;
    }
    let fwd = cfg.forward.normalize();
    if fwd == Vec2::ZERO {
        return None;
    }

    // Where the forward axis currently points, in world space, if this bone
    // had no animation rotation at all. Measuring against that (rather than
    // against the live pose) keeps the cone anchored to rest, so a clip that
    // turns the head does not drag the look-at cone with it.
    let parent_rot = match sk.bone(bone).parent {
        Some(p) => pose.world[p].rotation(),
        None => 0.0,
    };
    let rest_forward_world = parent_rot + sk.bone(bone).rest.rot + fwd.angle();
    let want = to_target.angle();
    let delta = angle_delta(rest_forward_world, want);
    let clamped = clamp(delta, -cfg.max_angle, cfg.max_angle);
    Some(clamped * clamp(cfg.weight, 0.0, 1.0))
}

/// Apply a look-at and re-resolve the affected subtree.
///
/// The constraint **owns** this bone's rotation channel: it writes the
/// rotation rather than adding to it, so it is idempotent and cannot drift
/// frame over frame. Give the look-at its own bone (a `gaze` child of `head`)
/// and keep any authored head motion on the parent, and the two compose
/// naturally.
///
/// Returns true if the target was inside the cone, false if it was clamped —
/// which is how "she can see it" differs from "she is straining towards it",
/// and what drives the lean in F69.
pub fn apply_look_at(
    sk: &Skeleton,
    pose: &mut Pose,
    bone: usize,
    target: Vec2,
    cfg: LookAt,
) -> bool {
    let Some(off) = look_at_offset(sk, pose, bone, target, cfg) else {
        return true;
    };
    pose.offsets[bone].rot = off;
    pose.resolve_from(sk, bone);
    let limit = cfg.max_angle * clamp(cfg.weight, 0.0, 1.0);
    off.abs() < limit - 1e-4
}

/// Rotate a bone so its length axis points straight at a world target, with no
/// cone and no blending.
///
/// This is the write-back for a simulated chain (see [`crate::motion::SpringChain`]):
/// the simulation produces joint *positions*, and the bones have to be turned
/// to match them. Returns false when the target gives no direction.
pub fn aim_bone_at(sk: &Skeleton, pose: &mut Pose, bone: usize, target: Vec2) -> bool {
    let origin = pose.world_pos(bone);
    let d = target - origin;
    if d.len_sq() <= 1e-8 {
        return false;
    }
    let parent_rot = match sk.bone(bone).parent {
        Some(p) => pose.world[p].rotation(),
        None => 0.0,
    };
    pose.offsets[bone].rot = wrap_angle(d.angle() - parent_rot - sk.bone(bone).rest.rot);
    pose.resolve_from(sk, bone);
    true
}

/// Point the whole eye at a target without rotating it: an offset inside a
/// bounded ellipse. Rotating a two-facet eye looks broken; sliding the pupil
/// reads as looking (F69, F73's "two simple expressive eyes").
pub fn eye_offset(from: Vec2, target: Vec2, radius: Vec2, falloff: f32) -> Vec2 {
    let d = target - from;
    let len = d.len();
    if len <= 1e-6 {
        return Vec2::ZERO;
    }
    // Saturating response: nearby targets move the pupil a lot, distant ones
    // pin it at the rim instead of growing without bound.
    let k = (len / falloff.max(1e-3)).tanh();
    let dir = d / len;
    Vec2::new(dir.x * radius.x, dir.y * radius.y) * k
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{deg_to_rad, Vec2};
    use crate::skeleton::{BoneRest, BoneSpec, Skeleton};

    fn end_effector(root: Vec2, l1: f32, l2: f32, s: TwoBoneSolution) -> Vec2 {
        let mid = root + Vec2::from_angle(s.root_world_rot) * l1;
        mid + Vec2::from_angle(s.root_world_rot + s.mid_local_rot) * l2
    }

    #[test]
    fn two_bone_reaches_a_target_inside_its_range() {
        let root = Vec2::new(10.0, 20.0);
        let (l1, l2) = (30.0, 25.0);
        for (tx, ty) in [(40.0, 10.0), (-20.0, 5.0), (10.0, 60.0), (25.0, -15.0)] {
            let target = Vec2::new(tx, ty);
            let s = solve_two_bone(root, l1, l2, target, true);
            assert!(s.reachable, "{target:?} should be reachable");
            let e = end_effector(root, l1, l2, s);
            assert!(e.dist(target) < 0.05, "target {target:?}, landed {e:?}");
        }
    }

    #[test]
    fn both_bend_directions_reach_the_same_target() {
        let root = Vec2::ZERO;
        let target = Vec2::new(30.0, 10.0);
        for bend in [true, false] {
            let s = solve_two_bone(root, 25.0, 20.0, target, bend);
            let e = end_effector(root, 25.0, 20.0, s);
            assert!(e.dist(target) < 0.05, "bend={bend} landed {e:?}");
        }
    }

    #[test]
    fn the_two_bend_solutions_are_mirror_images() {
        let s1 = solve_two_bone(Vec2::ZERO, 25.0, 20.0, Vec2::new(30.0, 0.0), true);
        let s2 = solve_two_bone(Vec2::ZERO, 25.0, 20.0, Vec2::new(30.0, 0.0), false);
        assert!((s1.root_world_rot + s2.root_world_rot).abs() < 1e-4);
        assert!((s1.mid_local_rot + s2.mid_local_rot).abs() < 1e-4);
    }

    #[test]
    fn an_unreachable_far_target_extends_straight_towards_it() {
        let root = Vec2::ZERO;
        let (l1, l2) = (20.0, 20.0);
        let target = Vec2::new(500.0, 500.0);
        let s = solve_two_bone(root, l1, l2, target, true);
        assert!(!s.reachable);
        // Fully extended: the middle joint is essentially straight...
        assert!(s.mid_local_rot.abs() < 0.02, "not extended: {}", s.mid_local_rot);
        // ...and pointing at the target.
        let e = end_effector(root, l1, l2, s);
        assert!(
            (e.angle() - target.angle()).abs() < 0.01,
            "aimed at {:?} not {:?}",
            e.angle(),
            target.angle()
        );
        assert!((e.len() - (l1 + l2)).abs() < 0.1);
    }

    #[test]
    fn an_unreachable_near_target_folds_without_nan() {
        // Target inside the inner dead zone of an uneven chain.
        let s = solve_two_bone(Vec2::ZERO, 40.0, 10.0, Vec2::new(1.0, 0.0), true);
        assert!(!s.reachable);
        assert!(s.root_world_rot.is_finite() && s.mid_local_rot.is_finite(), "{s:?}");
    }

    #[test]
    fn a_target_on_the_root_folds_the_chain_back_on_itself() {
        // With equal bone lengths the root *is* reachable — by folding flat.
        let root = Vec2::new(5.0, 5.0);
        let s = solve_two_bone(root, 20.0, 20.0, root, true);
        assert!(s.root_world_rot.is_finite() && s.mid_local_rot.is_finite(), "{s:?}");
        assert!(s.reachable);
        let e = end_effector(root, 20.0, 20.0, s);
        assert!(e.dist(root) < 0.1, "did not fold back: {e:?}");
    }

    #[test]
    fn zero_length_bones_do_not_divide_by_zero() {
        let s = solve_two_bone(Vec2::ZERO, 0.0, 0.0, Vec2::new(10.0, 10.0), true);
        assert!(s.root_world_rot.is_finite() && s.mid_local_rot.is_finite(), "{s:?}");
    }

    fn neck_skeleton() -> Skeleton {
        let b = |name: &str, parent: Option<&str>, pos: Vec2, len: f32| BoneSpec {
            name: name.into(),
            parent: parent.map(str::to_string),
            rest: BoneRest { pos, rot: 0.0, scale: Vec2::ONE },
            length: len,
        };
        Skeleton::build(&[
            b("root", None, Vec2::new(100.0, 100.0), 0.0),
            b("neck", Some("root"), Vec2::new(0.0, -20.0), 20.0),
            b("head", Some("neck"), Vec2::new(0.0, -20.0), 15.0),
            b("tip", Some("head"), Vec2::new(0.0, -15.0), 0.0),
        ])
        .unwrap()
    }

    #[test]
    fn look_at_turns_the_bone_towards_the_target() {
        let sk = neck_skeleton();
        let mut pose = sk.rest_pose();
        let head = sk.index_of("head").unwrap();
        let cfg = LookAt { forward: Vec2::new(0.0, -1.0), max_angle: 2.0, weight: 1.0 };
        // Target directly to the right of the head — a 90 degree turn, inside a
        // 2 radian cone.
        let inside = apply_look_at(&sk, &mut pose, head, Vec2::new(300.0, 60.0), cfg);
        assert!(inside);
        // "Up" in head space should now lean right.
        let up_world = pose.world[head].apply_vec(Vec2::new(0.0, -1.0));
        assert!(up_world.x > 0.5, "head did not turn right: {up_world:?}");
    }

    #[test]
    fn look_at_is_clamped_to_its_cone_and_says_so() {
        let sk = neck_skeleton();
        let mut pose = sk.rest_pose();
        let head = sk.index_of("head").unwrap();
        let cfg = LookAt { forward: Vec2::new(0.0, -1.0), max_angle: deg_to_rad(20.0), weight: 1.0 };
        // Straight down — 180 degrees away, far outside a 20 degree cone.
        let inside = apply_look_at(&sk, &mut pose, head, Vec2::new(100.0, 900.0), cfg);
        assert!(!inside, "a target behind her should report as clamped");
        assert!(
            pose.offsets[head].rot.abs() <= cfg.max_angle + 1e-4,
            "cone was exceeded: {}",
            pose.offsets[head].rot
        );
    }

    #[test]
    fn look_at_weight_scales_the_turn() {
        let sk = neck_skeleton();
        let head = sk.index_of("head").unwrap();
        let target = Vec2::new(300.0, 60.0);
        let full = LookAt { forward: Vec2::new(0.0, -1.0), max_angle: 2.0, weight: 1.0 };
        let half = LookAt { weight: 0.5, ..full };
        let mut a = sk.rest_pose();
        apply_look_at(&sk, &mut a, head, target, full);
        let mut b = sk.rest_pose();
        apply_look_at(&sk, &mut b, head, target, half);
        assert!((a.offsets[head].rot * 0.5 - b.offsets[head].rot).abs() < 1e-4);
    }

    #[test]
    fn look_at_zero_weight_changes_nothing() {
        let sk = neck_skeleton();
        let head = sk.index_of("head").unwrap();
        let mut pose = sk.rest_pose();
        let cfg = LookAt { forward: Vec2::new(0.0, -1.0), max_angle: 1.5, weight: 0.0 };
        apply_look_at(&sk, &mut pose, head, Vec2::new(500.0, 0.0), cfg);
        assert_eq!(pose.offsets[head].rot, 0.0);
    }

    #[test]
    fn look_at_target_on_the_bone_origin_makes_no_change() {
        let sk = neck_skeleton();
        let head = sk.index_of("head").unwrap();
        let mut pose = sk.rest_pose();
        let here = pose.world_pos(head);
        let before = pose.offsets[head].rot;
        apply_look_at(&sk, &mut pose, head, here, LookAt::default());
        assert_eq!(pose.offsets[head].rot, before);
    }

    #[test]
    fn look_at_is_idempotent_and_anchored_to_rest() {
        // A look-at that added to the existing rotation would drift further
        // every frame; it must land on the same answer however many times it
        // runs, and whatever the clips left in the channel.
        let sk = neck_skeleton();
        let head = sk.index_of("head").unwrap();
        let cfg = LookAt { forward: Vec2::new(0.0, -1.0), max_angle: 2.0, weight: 1.0 };
        let target = Vec2::new(300.0, 60.0);
        let mut pose = sk.rest_pose();
        apply_look_at(&sk, &mut pose, head, target, cfg);
        let once = pose.world[head].rotation();
        apply_look_at(&sk, &mut pose, head, target, cfg);
        assert!(
            (once - pose.world[head].rotation()).abs() < 1e-5,
            "look-at drifted on the second application"
        );

        // Same answer starting from a head the clips had already turned.
        let mut posed = sk.rest_pose();
        posed.offsets[head].rot = 0.9;
        posed.resolve(&sk);
        apply_look_at(&sk, &mut posed, head, target, cfg);
        assert!((once - posed.world[head].rotation()).abs() < 1e-5);
    }

    #[test]
    fn apply_two_bone_lands_the_real_chain_on_the_target() {
        let sk = neck_skeleton();
        let mut pose = sk.rest_pose();
        let (r, m, e) = (
            sk.index_of("neck").unwrap(),
            sk.index_of("head").unwrap(),
            sk.index_of("tip").unwrap(),
        );
        // The chain reaches 35px from the neck at (100, 80); stay inside that.
        for target in [
            Vec2::new(118.0, 62.0),
            Vec2::new(80.0, 55.0),
            Vec2::new(100.0, 50.0),
            Vec2::new(125.0, 85.0),
        ] {
            let mut p = sk.rest_pose();
            let sol = apply_two_bone(&sk, &mut p, r, m, e, target, true, 1.0);
            assert!(sol.reachable, "{target:?} should be reachable");
            let tip = p.world_pos(e);
            assert!(tip.dist(target) < 0.5, "tip landed {tip:?}, wanted {target:?}");
        }
        let _ = &mut pose;
    }

    #[test]
    fn two_bone_ik_works_on_a_chain_not_laid_out_along_its_own_x_axis() {
        // The neck rig points its bones down -y with rest.rot = 0, which is
        // the layout the in-app editor produces. IK must measure the chain by
        // where the joints are, not by assuming +x.
        let sk = neck_skeleton();
        let (r, m, e) = (
            sk.index_of("neck").unwrap(),
            sk.index_of("head").unwrap(),
            sk.index_of("tip").unwrap(),
        );
        let mut pose = sk.rest_pose();
        // Rest target: the chain should barely move.
        let rest_tip = pose.world_pos(e);
        apply_two_bone(&sk, &mut pose, r, m, e, rest_tip, true, 1.0);
        assert!(pose.world_pos(e).dist(rest_tip) < 0.5);
    }

    #[test]
    fn apply_two_bone_with_an_unreachable_target_still_produces_a_finite_pose() {
        let sk = neck_skeleton();
        let mut pose = sk.rest_pose();
        let (r, m, e) = (
            sk.index_of("neck").unwrap(),
            sk.index_of("head").unwrap(),
            sk.index_of("tip").unwrap(),
        );
        let sol = apply_two_bone(&sk, &mut pose, r, m, e, Vec2::new(9000.0, -9000.0), true, 1.0);
        assert!(!sol.reachable);
        assert!(pose.world_pos(e).is_finite());
    }

    #[test]
    fn eye_offset_saturates_at_the_rim() {
        let radius = Vec2::new(3.0, 2.0);
        let near = eye_offset(Vec2::ZERO, Vec2::new(10.0, 0.0), radius, 40.0);
        let far = eye_offset(Vec2::ZERO, Vec2::new(4000.0, 0.0), radius, 40.0);
        assert!(near.x < far.x);
        assert!(far.x <= radius.x + 1e-4, "escaped the rim: {far:?}");
        assert!((far.x - radius.x).abs() < 1e-3, "should pin at the rim: {far:?}");
    }

    #[test]
    fn eye_offset_respects_an_elliptical_rim() {
        let o = eye_offset(Vec2::ZERO, Vec2::new(0.0, 9000.0), Vec2::new(3.0, 2.0), 40.0);
        assert!((o.y - 2.0).abs() < 1e-3, "{o:?}");
        assert!(o.x.abs() < 1e-6);
    }

    #[test]
    fn eye_offset_on_the_eye_itself_is_zero() {
        assert_eq!(
            eye_offset(Vec2::new(4.0, 4.0), Vec2::new(4.0, 4.0), Vec2::ONE, 10.0),
            Vec2::ZERO
        );
    }
}
