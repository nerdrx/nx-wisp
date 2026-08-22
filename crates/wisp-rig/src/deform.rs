//! Mesh binding and linear blend skinning.
//!
//! Every point of a path — anchors and Bézier control points alike — is a
//! skinnable vertex. Each carries up to [`MAX_INFLUENCES`] bone weights that
//! sum to 1. Deforming is then a weighted sum of the bones' skinning matrices
//! applied to the rest point, which is the textbook LBS formulation.
//!
//! Weights live in one flat array shared by every point, with a `(start, len)`
//! span per point. That keeps the hot loop over contiguous memory and means a
//! deform costs one pass with no allocation and no hashing.

use crate::math::{clamp, Affine, Vec2};
use crate::skeleton::Skeleton;

/// Bones per vertex. Four is the standard budget: it covers a joint plus its
/// neighbours, and it keeps the weight array small enough to stay in cache.
pub const MAX_INFLUENCES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Influence {
    pub bone: u32,
    pub weight: f32,
}

/// Which slice of the shared weight array belongs to a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: u32,
    pub len: u32,
}

/// A point-to-bone binding for one shape.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Binding {
    /// One span per point, in path point order.
    pub spans: Vec<Span>,
    pub influences: Vec<Influence>,
}

impl Binding {
    /// Bind every point rigidly to one bone. The common case: a shape that is
    /// a solid piece of her, carried by a single bone.
    pub fn rigid(bone: usize, point_count: usize) -> Binding {
        Binding {
            spans: (0..point_count)
                .map(|i| Span { start: i as u32, len: 1 })
                .collect(),
            influences: vec![
                Influence { bone: bone as u32, weight: 1.0 };
                point_count
            ],
        }
    }

    pub fn point_count(&self) -> usize {
        self.spans.len()
    }

    pub fn influences_of(&self, point: usize) -> &[Influence] {
        match self.spans.get(point) {
            Some(s) => {
                let a = s.start as usize;
                let b = (a + s.len as usize).min(self.influences.len());
                &self.influences[a.min(b)..b]
            }
            None => &[],
        }
    }

    /// Build from explicit per-point influence lists. Normalises each point's
    /// weights, drops zero-weight entries and keeps the strongest
    /// [`MAX_INFLUENCES`].
    pub fn from_lists(lists: &[Vec<Influence>]) -> Binding {
        let mut b = Binding {
            spans: Vec::with_capacity(lists.len()),
            influences: Vec::with_capacity(lists.len() * 2),
        };
        let mut scratch: Vec<Influence> = Vec::with_capacity(8);
        for list in lists {
            scratch.clear();
            scratch.extend(list.iter().copied().filter(|i| i.weight > 1e-5));
            scratch.sort_by(|a, c| {
                c.weight
                    .partial_cmp(&a.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            scratch.truncate(MAX_INFLUENCES);
            let sum: f32 = scratch.iter().map(|i| i.weight).sum();
            let start = b.influences.len() as u32;
            if sum <= 1e-6 {
                // A point nothing claims stays where it is: bind it to the
                // root with full weight rather than collapsing it to the
                // origin.
                b.influences.push(Influence { bone: 0, weight: 1.0 });
                b.spans.push(Span { start, len: 1 });
                continue;
            }
            for inf in &scratch {
                b.influences.push(Influence { bone: inf.bone, weight: inf.weight / sum });
            }
            b.spans.push(Span { start, len: scratch.len() as u32 });
        }
        b
    }

    /// True if every span points inside `influences` and every bone index is
    /// below `bone_count`. Skin validation calls this; the hot loop then does
    /// not have to.
    pub fn is_valid(&self, bone_count: usize) -> bool {
        self.spans.iter().all(|s| {
            (s.start as usize + s.len as usize) <= self.influences.len() && s.len > 0
        }) && self
            .influences
            .iter()
            .all(|i| (i.bone as usize) < bone_count && (0.0..=1.0).contains(&i.weight))
    }
}

/// How auto-binding falls off with distance from a bone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoBind {
    /// Distance in canvas units at which a bone's influence reaches zero.
    pub falloff: f32,
    /// Exponent on the normalised distance. 1 is linear, 2 is the smooth
    /// default, higher is tighter around the bone.
    pub power: f32,
}

impl Default for AutoBind {
    fn default() -> Self {
        AutoBind { falloff: 40.0, power: 2.0 }
    }
}

/// Weight points against a set of bones by distance to each bone's rest
/// segment.
///
/// This is what makes a skin authorable by hand: name the bones a shape should
/// follow and the falloff radius, and the weights come out smooth. Explicit
/// per-point weights in the skin file override whatever this produces.
pub fn auto_bind(
    sk: &Skeleton,
    rest_points: &[Vec2],
    bones: &[usize],
    cfg: AutoBind,
) -> Binding {
    if bones.is_empty() {
        return Binding::rigid(0, rest_points.len());
    }
    let falloff = cfg.falloff.max(1e-3);
    let power = cfg.power.max(0.01);

    // Bone rest segments in world space, once.
    let segs: Vec<(Vec2, Vec2)> = bones
        .iter()
        .map(|&b| {
            let m = sk.rest_world()[b];
            (m.origin(), m.apply(sk.bone(b).tip_local()))
        })
        .collect();

    let mut lists: Vec<Vec<Influence>> = Vec::with_capacity(rest_points.len());
    for p in rest_points {
        let mut list = Vec::with_capacity(bones.len());
        for (k, &b) in bones.iter().enumerate() {
            let (a, c) = segs[k];
            let (closest, _) = p.closest_on_segment(a, c);
            let d = p.dist(closest);
            let w = clamp(1.0 - d / falloff, 0.0, 1.0).powf(power);
            if w > 1e-5 {
                list.push(Influence { bone: b as u32, weight: w });
            }
        }
        if list.is_empty() {
            // Outside every falloff radius: fall back to the nearest bone so
            // the point still moves with her instead of being left behind.
            let mut best = (f32::MAX, bones[0]);
            for (k, &b) in bones.iter().enumerate() {
                let (a, c) = segs[k];
                let d = p.dist(p.closest_on_segment(a, c).0);
                if d < best.0 {
                    best = (d, b);
                }
            }
            list.push(Influence { bone: best.1 as u32, weight: 1.0 });
        }
        lists.push(list);
    }
    Binding::from_lists(&lists)
}

/// Linear blend skinning: `out[i] = Σ w_j · M_j · rest[i]`.
///
/// `out` is resized to match `rest` and then overwritten, so passing the same
/// buffer every frame allocates only on the first call.
pub fn skin_points(
    rest: &[Vec2],
    binding: &Binding,
    skin_mats: &[Affine],
    out: &mut Vec<Vec2>,
) {
    if out.len() != rest.len() {
        out.clear();
        out.resize(rest.len(), Vec2::ZERO);
    }
    for (i, r) in rest.iter().enumerate() {
        let infs = binding.influences_of(i);
        match infs {
            // Rigid: the overwhelmingly common case, and worth not looping for.
            [only] => {
                let m = skin_mats
                    .get(only.bone as usize)
                    .copied()
                    .unwrap_or(Affine::IDENTITY);
                out[i] = m.apply(*r);
            }
            [] => out[i] = *r,
            many => {
                let mut acc = Vec2::ZERO;
                for inf in many {
                    let m = skin_mats
                        .get(inf.bone as usize)
                        .copied()
                        .unwrap_or(Affine::IDENTITY);
                    acc += m.apply(*r) * inf.weight;
                }
                out[i] = acc;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{deg_to_rad, Vec2};
    use crate::skeleton::{BoneRest, BoneSpec, Skeleton};

    fn two_bone_skeleton() -> Skeleton {
        let b = |name: &str, parent: Option<&str>, pos: Vec2, len: f32| BoneSpec {
            name: name.into(),
            parent: parent.map(str::to_string),
            rest: BoneRest { pos, rot: 0.0, scale: Vec2::ONE },
            length: len,
        };
        Skeleton::build(&[
            b("a", None, Vec2::new(0.0, 0.0), 100.0),
            b("b", Some("a"), Vec2::new(100.0, 0.0), 100.0),
        ])
        .unwrap()
    }

    #[test]
    fn rigid_binding_moves_points_with_one_bone() {
        let sk = two_bone_skeleton();
        let mut pose = sk.rest_pose();
        pose.offsets[0].tx = 25.0;
        pose.resolve(&sk);
        let rest = vec![Vec2::new(10.0, 0.0), Vec2::new(50.0, 5.0)];
        let bind = Binding::rigid(0, rest.len());
        let mut out = Vec::new();
        skin_points(&rest, &bind, &pose.skin_mat, &mut out);
        assert_eq!(out[0], Vec2::new(35.0, 0.0));
        assert_eq!(out[1], Vec2::new(75.0, 5.0));
    }

    #[test]
    fn rest_pose_skinning_is_the_identity() {
        let sk = two_bone_skeleton();
        let pose = sk.rest_pose();
        let rest = vec![Vec2::new(37.0, -12.0), Vec2::new(150.0, 4.0)];
        let bind = auto_bind(&sk, &rest, &[0, 1], AutoBind { falloff: 60.0, power: 2.0 });
        let mut out = Vec::new();
        skin_points(&rest, &bind, &pose.skin_mat, &mut out);
        for (a, b) in rest.iter().zip(out.iter()) {
            assert!(a.dist(*b) < 1e-3, "{a:?} -> {b:?}");
        }
    }

    #[test]
    fn blended_weights_land_between_the_two_rigid_answers() {
        let sk = two_bone_skeleton();
        let mut pose = sk.rest_pose();
        pose.offsets[1].rot = deg_to_rad(60.0);
        pose.resolve(&sk);
        let p = Vec2::new(100.0, 0.0);
        let rest = vec![p];

        let a_only = {
            let mut o = Vec::new();
            skin_points(&rest, &Binding::rigid(0, 1), &pose.skin_mat, &mut o);
            o[0]
        };
        let b_only = {
            let mut o = Vec::new();
            skin_points(&rest, &Binding::rigid(1, 1), &pose.skin_mat, &mut o);
            o[0]
        };
        let half = Binding::from_lists(&[vec![
            Influence { bone: 0, weight: 0.5 },
            Influence { bone: 1, weight: 0.5 },
        ]]);
        let mut o = Vec::new();
        skin_points(&rest, &half, &pose.skin_mat, &mut o);
        let expected = (a_only + b_only) * 0.5;
        assert!(o[0].dist(expected) < 1e-3, "{:?} vs {expected:?}", o[0]);
    }

    #[test]
    fn from_lists_normalises_weights_to_one() {
        let b = Binding::from_lists(&[vec![
            Influence { bone: 0, weight: 3.0 },
            Influence { bone: 1, weight: 1.0 },
        ]]);
        let infs = b.influences_of(0);
        assert_eq!(infs.len(), 2);
        let sum: f32 = infs.iter().map(|i| i.weight).sum();
        assert!((sum - 1.0).abs() < 1e-5);
        assert!((infs[0].weight - 0.75).abs() < 1e-5);
    }

    #[test]
    fn from_lists_keeps_only_the_strongest_four() {
        let list: Vec<Influence> = (0..7)
            .map(|i| Influence { bone: i, weight: (i + 1) as f32 })
            .collect();
        let b = Binding::from_lists(&[list]);
        let infs = b.influences_of(0);
        assert_eq!(infs.len(), MAX_INFLUENCES);
        // Sorted strongest first: bones 6, 5, 4, 3.
        assert_eq!(infs[0].bone, 6);
        assert!(infs.iter().all(|i| i.bone >= 3));
        let sum: f32 = infs.iter().map(|i| i.weight).sum();
        assert!((sum - 1.0).abs() < 1e-5);
    }

    #[test]
    fn from_lists_drops_zero_weights() {
        let b = Binding::from_lists(&[vec![
            Influence { bone: 0, weight: 1.0 },
            Influence { bone: 1, weight: 0.0 },
        ]]);
        assert_eq!(b.influences_of(0).len(), 1);
    }

    #[test]
    fn a_point_with_no_influence_falls_back_to_the_root() {
        let b = Binding::from_lists(&[vec![]]);
        let infs = b.influences_of(0);
        assert_eq!(infs.len(), 1);
        assert_eq!(infs[0], Influence { bone: 0, weight: 1.0 });
    }

    #[test]
    fn auto_bind_weights_are_normalised_and_in_range() {
        let sk = two_bone_skeleton();
        let rest: Vec<Vec2> = (0..40)
            .map(|i| Vec2::new(i as f32 * 5.0, (i % 7) as f32 * 3.0 - 10.0))
            .collect();
        let bind = auto_bind(&sk, &rest, &[0, 1], AutoBind { falloff: 50.0, power: 2.0 });
        assert!(bind.is_valid(sk.len()));
        for i in 0..rest.len() {
            let infs = bind.influences_of(i);
            assert!(!infs.is_empty(), "point {i} unbound");
            let sum: f32 = infs.iter().map(|f| f.weight).sum();
            assert!((sum - 1.0).abs() < 1e-4, "point {i} sums to {sum}");
            assert!(infs.iter().all(|f| (0.0..=1.0).contains(&f.weight)));
        }
    }

    #[test]
    fn auto_bind_favours_the_nearer_bone() {
        let sk = two_bone_skeleton();
        // Sits on bone `a`, far from `b`.
        let rest = vec![Vec2::new(10.0, 0.0)];
        let bind = auto_bind(&sk, &rest, &[0, 1], AutoBind { falloff: 60.0, power: 2.0 });
        let infs = bind.influences_of(0);
        let wa: f32 = infs.iter().filter(|i| i.bone == 0).map(|i| i.weight).sum();
        assert!(wa > 0.9, "expected bone a to dominate, got {wa}");
    }

    #[test]
    fn auto_bind_falls_back_to_the_nearest_bone_when_out_of_range() {
        let sk = two_bone_skeleton();
        let rest = vec![Vec2::new(0.0, 5000.0)];
        let bind = auto_bind(&sk, &rest, &[0, 1], AutoBind { falloff: 10.0, power: 2.0 });
        let infs = bind.influences_of(0);
        assert_eq!(infs.len(), 1);
        assert!((infs[0].weight - 1.0).abs() < 1e-5);
    }

    #[test]
    fn auto_bind_with_no_bones_binds_rigidly_to_the_root() {
        let sk = two_bone_skeleton();
        let rest = vec![Vec2::new(1.0, 1.0), Vec2::new(2.0, 2.0)];
        let bind = auto_bind(&sk, &rest, &[], AutoBind::default());
        assert!(bind.is_valid(sk.len()));
        assert_eq!(bind.point_count(), 2);
    }

    #[test]
    fn validation_rejects_out_of_range_bones_and_weights() {
        let good = Binding::rigid(0, 2);
        assert!(good.is_valid(1));
        assert!(!good.is_valid(0));

        let bad_weight = Binding {
            spans: vec![Span { start: 0, len: 1 }],
            influences: vec![Influence { bone: 0, weight: 1.5 }],
        };
        assert!(!bad_weight.is_valid(1));

        let bad_span = Binding {
            spans: vec![Span { start: 5, len: 3 }],
            influences: vec![Influence { bone: 0, weight: 1.0 }],
        };
        assert!(!bad_span.is_valid(1));
    }

    #[test]
    fn skinning_reuses_its_output_buffer() {
        let sk = two_bone_skeleton();
        let pose = sk.rest_pose();
        let rest = vec![Vec2::new(1.0, 1.0); 64];
        let bind = Binding::rigid(0, rest.len());
        let mut out = Vec::new();
        skin_points(&rest, &bind, &pose.skin_mat, &mut out);
        let cap = out.capacity();
        for _ in 0..100 {
            skin_points(&rest, &bind, &pose.skin_mat, &mut out);
        }
        assert_eq!(cap, out.capacity(), "skinning reallocated");
    }

    #[test]
    fn a_binding_pointing_at_a_missing_bone_does_not_panic() {
        let rest = vec![Vec2::new(1.0, 1.0)];
        let bind = Binding::rigid(99, 1);
        let mut out = Vec::new();
        skin_points(&rest, &bind, &[Affine::IDENTITY], &mut out);
        assert_eq!(out[0], rest[0]);
    }
}
