//! The bone hierarchy and pose resolution.
//!
//! Bones are stored in **topological order** — a bone's parent always has a
//! lower index — so resolving a pose is one forward pass with no recursion and
//! no sorting per frame. The topological sort happens once, at skin compile
//! time, and it is where bone-tree cycles are detected.
//!
//! Animation values are *offsets from rest*, never absolute transforms:
//! `tx`/`ty`/`rot` add, `sx`/`sy`/`alpha` multiply. That makes additive layers
//! (F70's breathing and blinking on top of walking) well defined and makes a
//! cross-fade a plain lerp of offsets.

use std::collections::HashMap;

use crate::math::{Affine, Vec2};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoneRest {
    /// Rest position in the parent's space.
    pub pos: Vec2,
    /// Rest rotation in radians.
    pub rot: f32,
    pub scale: Vec2,
}

impl Default for BoneRest {
    fn default() -> Self {
        BoneRest { pos: Vec2::ZERO, rot: 0.0, scale: Vec2::ONE }
    }
}

#[derive(Debug, Clone)]
pub struct Bone {
    pub name: Box<str>,
    /// Always `< self`'s own index, or `None` for a root.
    pub parent: Option<usize>,
    pub rest: BoneRest,
    /// Length along the bone's local +x axis. Used by IK and by auto-binding;
    /// a zero-length bone is a pure locator.
    pub length: f32,
}

impl Bone {
    /// Tip of the bone in its own local space.
    #[inline]
    pub fn tip_local(&self) -> Vec2 {
        Vec2::new(self.length, 0.0)
    }
}

/// The animation channels a clip may key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Additive x offset, canvas units.
    Tx,
    /// Additive y offset, canvas units.
    Ty,
    /// Additive rotation. Authored in degrees, stored in radians.
    Rot,
    /// Multiplicative x scale.
    Sx,
    /// Multiplicative y scale.
    Sy,
    /// Multiplicative opacity, inherited down the bone tree. Shapes bound to
    /// the bone are faded by it — this is how a blink and a fading tail work
    /// without a per-shape animation system.
    Alpha,
}

impl Channel {
    pub fn name(self) -> &'static str {
        match self {
            Channel::Tx => "tx",
            Channel::Ty => "ty",
            Channel::Rot => "rot",
            Channel::Sx => "sx",
            Channel::Sy => "sy",
            Channel::Alpha => "alpha",
        }
    }
    pub fn from_name(s: &str) -> Option<Channel> {
        Some(match s {
            "tx" => Channel::Tx,
            "ty" => Channel::Ty,
            "rot" => Channel::Rot,
            "sx" => Channel::Sx,
            "sy" => Channel::Sy,
            "alpha" => Channel::Alpha,
            _ => return None,
        })
    }
    /// Additive channels sum across layers; the rest multiply.
    #[inline]
    pub fn is_additive_channel(self) -> bool {
        matches!(self, Channel::Tx | Channel::Ty | Channel::Rot)
    }
    /// The value that means "no change".
    #[inline]
    pub fn identity(self) -> f32 {
        if self.is_additive_channel() {
            0.0
        } else {
            1.0
        }
    }
    pub const ALL: [Channel; 6] = [
        Channel::Tx,
        Channel::Ty,
        Channel::Rot,
        Channel::Sx,
        Channel::Sy,
        Channel::Alpha,
    ];
}

/// Per-bone animation state, as offsets from rest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoneOffsets {
    pub tx: f32,
    pub ty: f32,
    pub rot: f32,
    pub sx: f32,
    pub sy: f32,
    pub alpha: f32,
}

impl Default for BoneOffsets {
    fn default() -> Self {
        BoneOffsets::IDENTITY
    }
}

impl BoneOffsets {
    pub const IDENTITY: BoneOffsets =
        BoneOffsets { tx: 0.0, ty: 0.0, rot: 0.0, sx: 1.0, sy: 1.0, alpha: 1.0 };

    #[inline]
    pub fn get(&self, c: Channel) -> f32 {
        match c {
            Channel::Tx => self.tx,
            Channel::Ty => self.ty,
            Channel::Rot => self.rot,
            Channel::Sx => self.sx,
            Channel::Sy => self.sy,
            Channel::Alpha => self.alpha,
        }
    }
    #[inline]
    pub fn set(&mut self, c: Channel, v: f32) {
        match c {
            Channel::Tx => self.tx = v,
            Channel::Ty => self.ty = v,
            Channel::Rot => self.rot = v,
            Channel::Sx => self.sx = v,
            Channel::Sy => self.sy = v,
            Channel::Alpha => self.alpha = v,
        }
    }
    /// Fold another set in as an *additive layer*.
    #[inline]
    pub fn accumulate(&mut self, o: &BoneOffsets, weight: f32) {
        self.tx += o.tx * weight;
        self.ty += o.ty * weight;
        self.rot += o.rot * weight;
        self.sx *= 1.0 + (o.sx - 1.0) * weight;
        self.sy *= 1.0 + (o.sy - 1.0) * weight;
        self.alpha *= 1.0 + (o.alpha - 1.0) * weight;
    }
    /// Blend towards another set — the cross-fade and layer-weight operation.
    #[inline]
    pub fn blend(&mut self, o: &BoneOffsets, t: f32) {
        use crate::math::lerp;
        self.tx = lerp(self.tx, o.tx, t);
        self.ty = lerp(self.ty, o.ty, t);
        self.rot = lerp(self.rot, o.rot, t);
        self.sx = lerp(self.sx, o.sx, t);
        self.sy = lerp(self.sy, o.sy, t);
        self.alpha = lerp(self.alpha, o.alpha, t);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkeletonError {
    #[error("bone '{0}' names a parent '{1}' that does not exist")]
    UnknownParent(String, String),
    #[error("duplicate bone name '{0}'")]
    DuplicateName(String),
    #[error("the bone tree has a cycle through: {}", .0.join(" -> "))]
    Cycle(Vec<String>),
    #[error("a skin needs at least one bone")]
    Empty,
}

/// A bone as it comes out of a skin file, before topological ordering.
#[derive(Debug, Clone)]
pub struct BoneSpec {
    pub name: String,
    pub parent: Option<String>,
    pub rest: BoneRest,
    pub length: f32,
}

#[derive(Debug, Clone)]
pub struct Skeleton {
    bones: Vec<Bone>,
    by_name: HashMap<Box<str>, usize>,
    rest_world: Vec<Affine>,
    rest_world_inv: Vec<Affine>,
}

impl Skeleton {
    /// Build from unordered specs. Detects duplicate names, dangling parents
    /// and cycles, and reorders so parents precede children.
    pub fn build(specs: &[BoneSpec]) -> Result<Skeleton, SkeletonError> {
        if specs.is_empty() {
            return Err(SkeletonError::Empty);
        }
        let mut index: HashMap<&str, usize> = HashMap::with_capacity(specs.len());
        for (i, s) in specs.iter().enumerate() {
            if index.insert(s.name.as_str(), i).is_some() {
                return Err(SkeletonError::DuplicateName(s.name.clone()));
            }
        }
        // Resolve parent links up front so a dangling parent is reported as
        // itself rather than as a phantom cycle.
        let mut parent_of: Vec<Option<usize>> = Vec::with_capacity(specs.len());
        for s in specs {
            match &s.parent {
                None => parent_of.push(None),
                Some(p) if p.is_empty() => parent_of.push(None),
                Some(p) => match index.get(p.as_str()) {
                    Some(&pi) => parent_of.push(Some(pi)),
                    None => {
                        return Err(SkeletonError::UnknownParent(s.name.clone(), p.clone()))
                    }
                },
            }
        }

        let order = topo_order(specs, &parent_of)?;

        // old index -> new index
        let mut remap = vec![usize::MAX; specs.len()];
        for (new_i, &old_i) in order.iter().enumerate() {
            remap[old_i] = new_i;
        }

        let mut bones = Vec::with_capacity(specs.len());
        let mut by_name = HashMap::with_capacity(specs.len());
        for &old_i in &order {
            let s = &specs[old_i];
            let name: Box<str> = s.name.clone().into_boxed_str();
            by_name.insert(name.clone(), bones.len());
            bones.push(Bone {
                name,
                parent: parent_of[old_i].map(|p| remap[p]),
                rest: s.rest,
                length: s.length,
            });
        }

        let mut sk = Skeleton {
            bones,
            by_name,
            rest_world: Vec::new(),
            rest_world_inv: Vec::new(),
        };
        sk.recompute_rest();
        Ok(sk)
    }

    fn recompute_rest(&mut self) {
        let n = self.bones.len();
        self.rest_world.clear();
        self.rest_world.resize(n, Affine::IDENTITY);
        for i in 0..n {
            let b = &self.bones[i];
            let local = Affine::from_trs(b.rest.pos, b.rest.rot, b.rest.scale);
            self.rest_world[i] = match b.parent {
                Some(p) => self.rest_world[p].mul(local),
                None => local,
            };
        }
        self.rest_world_inv = self.rest_world.iter().map(|m| m.inverse()).collect();
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.bones.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bones.is_empty()
    }
    #[inline]
    pub fn bones(&self) -> &[Bone] {
        &self.bones
    }
    #[inline]
    pub fn bone(&self, i: usize) -> &Bone {
        &self.bones[i]
    }
    #[inline]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).copied()
    }
    #[inline]
    pub fn rest_world(&self) -> &[Affine] {
        &self.rest_world
    }
    #[inline]
    pub fn rest_world_inv(&self) -> &[Affine] {
        &self.rest_world_inv
    }

    /// Is `ancestor` on the parent chain of `bone`?
    pub fn is_ancestor_of(&self, ancestor: usize, bone: usize) -> bool {
        let mut cur = self.bones[bone].parent;
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            cur = self.bones[c].parent;
        }
        false
    }

    /// A fresh pose sized for this skeleton, sitting at rest.
    pub fn rest_pose(&self) -> Pose {
        let mut p = Pose::new(self.len());
        p.resolve(self);
        p
    }
}

fn topo_order(
    specs: &[BoneSpec],
    parent_of: &[Option<usize>],
) -> Result<Vec<usize>, SkeletonError> {
    let n = specs.len();
    // 0 = unvisited, 1 = on stack, 2 = done.
    let mut state = vec![0u8; n];
    let mut order = Vec::with_capacity(n);
    // Iterative DFS so a pathological skin cannot blow the stack.
    for start in 0..n {
        if state[start] != 0 {
            continue;
        }
        let mut stack = vec![start];
        while let Some(&node) = stack.last() {
            match state[node] {
                0 => {
                    state[node] = 1;
                    if let Some(p) = parent_of[node] {
                        match state[p] {
                            0 => stack.push(p),
                            1 => {
                                // p is on the current stack: cycle. Report the
                                // loop, starting from p.
                                let at = stack.iter().position(|&x| x == p).unwrap_or(0);
                                let mut chain: Vec<String> =
                                    stack[at..].iter().map(|&i| specs[i].name.clone()).collect();
                                chain.push(specs[p].name.clone());
                                return Err(SkeletonError::Cycle(chain));
                            }
                            _ => {}
                        }
                    }
                }
                1 => {
                    // Parent (if any) is resolved; emit.
                    state[node] = 2;
                    order.push(node);
                    stack.pop();
                }
                _ => {
                    stack.pop();
                }
            }
        }
    }
    debug_assert_eq!(order.len(), n);
    Ok(order)
}

/// A resolved pose: offsets in, world transforms out.
#[derive(Debug, Clone, PartialEq)]
pub struct Pose {
    pub offsets: Vec<BoneOffsets>,
    pub local: Vec<Affine>,
    pub world: Vec<Affine>,
    /// `world * rest_world⁻¹` — what linear blend skinning multiplies a rest
    /// vertex by.
    pub skin_mat: Vec<Affine>,
    /// Alpha accumulated down the hierarchy.
    pub alpha: Vec<f32>,
}

impl Pose {
    pub fn new(n: usize) -> Pose {
        Pose {
            offsets: vec![BoneOffsets::IDENTITY; n],
            local: vec![Affine::IDENTITY; n],
            world: vec![Affine::IDENTITY; n],
            skin_mat: vec![Affine::IDENTITY; n],
            alpha: vec![1.0; n],
        }
    }

    pub fn reset_offsets(&mut self) {
        self.offsets.fill(BoneOffsets::IDENTITY);
    }

    /// Propagate local transforms to world, then compute skinning matrices.
    /// One forward pass — no allocation, no recursion.
    pub fn resolve(&mut self, sk: &Skeleton) {
        let n = sk.len();
        debug_assert_eq!(self.offsets.len(), n);
        for i in 0..n {
            let b = sk.bone(i);
            let o = &self.offsets[i];
            let local = Affine::from_trs(
                Vec2::new(b.rest.pos.x + o.tx, b.rest.pos.y + o.ty),
                b.rest.rot + o.rot,
                Vec2::new(b.rest.scale.x * o.sx, b.rest.scale.y * o.sy),
            );
            self.local[i] = local;
            match b.parent {
                Some(p) => {
                    self.world[i] = self.world[p].mul(local);
                    self.alpha[i] = self.alpha[p] * o.alpha;
                }
                None => {
                    self.world[i] = local;
                    self.alpha[i] = o.alpha;
                }
            }
            self.skin_mat[i] = self.world[i].mul(sk.rest_world_inv()[i]);
        }
    }

    /// Resolve only `from..` — used after IK writes a rotation partway down
    /// the tree, since parents cannot have changed.
    pub fn resolve_from(&mut self, sk: &Skeleton, from: usize) {
        for i in from..sk.len() {
            let b = sk.bone(i);
            let o = &self.offsets[i];
            let local = Affine::from_trs(
                Vec2::new(b.rest.pos.x + o.tx, b.rest.pos.y + o.ty),
                b.rest.rot + o.rot,
                Vec2::new(b.rest.scale.x * o.sx, b.rest.scale.y * o.sy),
            );
            self.local[i] = local;
            match b.parent {
                Some(p) => {
                    self.world[i] = self.world[p].mul(local);
                    self.alpha[i] = self.alpha[p] * o.alpha;
                }
                None => {
                    self.world[i] = local;
                    self.alpha[i] = o.alpha;
                }
            }
            self.skin_mat[i] = self.world[i].mul(sk.rest_world_inv()[i]);
        }
    }

    #[inline]
    pub fn world_pos(&self, i: usize) -> Vec2 {
        self.world[i].origin()
    }

    /// World position of a bone's tip.
    #[inline]
    pub fn world_tip(&self, sk: &Skeleton, i: usize) -> Vec2 {
        self.world[i].apply(sk.bone(i).tip_local())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, parent: Option<&str>, pos: Vec2, rot_deg: f32, len: f32) -> BoneSpec {
        BoneSpec {
            name: name.into(),
            parent: parent.map(str::to_string),
            rest: BoneRest {
                pos,
                rot: crate::math::deg_to_rad(rot_deg),
                scale: Vec2::ONE,
            },
            length: len,
        }
    }

    fn chain() -> Skeleton {
        Skeleton::build(&[
            // Deliberately out of order: the builder must sort them.
            spec("hand", Some("arm"), Vec2::new(20.0, 0.0), 0.0, 10.0),
            spec("root", None, Vec2::new(100.0, 100.0), 0.0, 0.0),
            spec("arm", Some("root"), Vec2::new(10.0, 0.0), 0.0, 20.0),
        ])
        .unwrap()
    }

    #[test]
    fn build_orders_parents_before_children() {
        let sk = chain();
        for (i, b) in sk.bones().iter().enumerate() {
            if let Some(p) = b.parent {
                assert!(p < i, "bone {} has parent at {p} >= {i}", b.name);
            }
        }
    }

    #[test]
    fn rest_world_composes_down_the_chain() {
        let sk = chain();
        let hand = sk.index_of("hand").unwrap();
        assert_eq!(sk.rest_world()[hand].origin(), Vec2::new(130.0, 100.0));
    }

    #[test]
    fn rejects_duplicate_bone_names() {
        let e = Skeleton::build(&[
            spec("a", None, Vec2::ZERO, 0.0, 1.0),
            spec("a", None, Vec2::ZERO, 0.0, 1.0),
        ])
        .unwrap_err();
        assert_eq!(e, SkeletonError::DuplicateName("a".into()));
    }

    #[test]
    fn rejects_unknown_parent() {
        let e = Skeleton::build(&[spec("a", Some("ghost"), Vec2::ZERO, 0.0, 1.0)]).unwrap_err();
        assert_eq!(e, SkeletonError::UnknownParent("a".into(), "ghost".into()));
    }

    #[test]
    fn rejects_a_two_bone_cycle() {
        let e = Skeleton::build(&[
            spec("a", Some("b"), Vec2::ZERO, 0.0, 1.0),
            spec("b", Some("a"), Vec2::ZERO, 0.0, 1.0),
        ])
        .unwrap_err();
        match e {
            SkeletonError::Cycle(c) => assert!(c.contains(&"a".to_string()) && c.contains(&"b".to_string())),
            other => panic!("expected a cycle, got {other:?}"),
        }
    }

    #[test]
    fn rejects_a_self_parented_bone() {
        let e = Skeleton::build(&[spec("a", Some("a"), Vec2::ZERO, 0.0, 1.0)]).unwrap_err();
        assert!(matches!(e, SkeletonError::Cycle(_)));
    }

    #[test]
    fn rejects_a_long_cycle() {
        let e = Skeleton::build(&[
            spec("a", Some("c"), Vec2::ZERO, 0.0, 1.0),
            spec("b", Some("a"), Vec2::ZERO, 0.0, 1.0),
            spec("c", Some("b"), Vec2::ZERO, 0.0, 1.0),
        ])
        .unwrap_err();
        assert!(matches!(e, SkeletonError::Cycle(_)));
    }

    #[test]
    fn rejects_an_empty_skeleton() {
        assert_eq!(Skeleton::build(&[]).unwrap_err(), SkeletonError::Empty);
    }

    #[test]
    fn deep_chains_do_not_blow_the_stack() {
        // Iterative DFS: 20k bones must sort without recursing.
        let mut specs = vec![spec("b0", None, Vec2::ZERO, 0.0, 1.0)];
        for i in 1..20_000 {
            specs.push(spec(
                &format!("b{i}"),
                Some(&format!("b{}", i - 1)),
                Vec2::new(1.0, 0.0),
                0.0,
                1.0,
            ));
        }
        let sk = Skeleton::build(&specs).unwrap();
        assert_eq!(sk.len(), 20_000);
    }

    #[test]
    fn rotation_offset_propagates_to_descendants() {
        let sk = chain();
        let mut pose = sk.rest_pose();
        let arm = sk.index_of("arm").unwrap();
        let hand = sk.index_of("hand").unwrap();
        pose.offsets[arm].rot = crate::math::deg_to_rad(90.0);
        pose.resolve(&sk);
        // arm sits at (110,100); rotating it 90 deg swings the hand to (110,120).
        let p = pose.world_pos(hand);
        assert!((p.x - 110.0).abs() < 1e-3, "{p:?}");
        assert!((p.y - 120.0).abs() < 1e-3, "{p:?}");
    }

    #[test]
    fn skin_matrix_is_identity_at_rest() {
        let sk = chain();
        let pose = sk.rest_pose();
        for m in &pose.skin_mat {
            let p = m.apply(Vec2::new(7.0, -3.0));
            assert!((p.x - 7.0).abs() < 1e-3 && (p.y + 3.0).abs() < 1e-3, "{m:?}");
        }
    }

    #[test]
    fn alpha_multiplies_down_the_hierarchy() {
        let sk = chain();
        let mut pose = sk.rest_pose();
        pose.offsets[sk.index_of("root").unwrap()].alpha = 0.5;
        pose.offsets[sk.index_of("arm").unwrap()].alpha = 0.5;
        pose.resolve(&sk);
        assert!((pose.alpha[sk.index_of("hand").unwrap()] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn resolve_from_matches_a_full_resolve() {
        let sk = chain();
        let arm = sk.index_of("arm").unwrap();
        let mut a = sk.rest_pose();
        a.offsets[arm].rot = 0.4;
        a.resolve(&sk);
        let mut b = sk.rest_pose();
        b.offsets[arm].rot = 0.4;
        b.resolve_from(&sk, arm);
        assert_eq!(a.world, b.world);
    }

    #[test]
    fn world_tip_follows_the_bone_length() {
        let sk = chain();
        let pose = sk.rest_pose();
        let arm = sk.index_of("arm").unwrap();
        assert_eq!(pose.world_tip(&sk, arm), Vec2::new(130.0, 100.0));
    }

    #[test]
    fn is_ancestor_of_walks_the_chain() {
        let sk = chain();
        let (r, a, h) = (
            sk.index_of("root").unwrap(),
            sk.index_of("arm").unwrap(),
            sk.index_of("hand").unwrap(),
        );
        assert!(sk.is_ancestor_of(r, h));
        assert!(sk.is_ancestor_of(a, h));
        assert!(!sk.is_ancestor_of(h, r));
        assert!(!sk.is_ancestor_of(h, h));
    }

    #[test]
    fn additive_accumulate_sums_translation_and_scales_multiplicatively() {
        let mut base = BoneOffsets::IDENTITY;
        let layer = BoneOffsets { tx: 4.0, ty: 0.0, rot: 0.5, sx: 1.2, sy: 0.8, alpha: 0.5 };
        base.accumulate(&layer, 1.0);
        assert_eq!(base.tx, 4.0);
        assert!((base.sx - 1.2).abs() < 1e-6);
        assert!((base.alpha - 0.5).abs() < 1e-6);
        base.accumulate(&layer, 0.5);
        assert_eq!(base.tx, 6.0);
        assert!((base.sx - 1.2 * 1.1).abs() < 1e-5);
    }

    #[test]
    fn channel_names_round_trip() {
        for c in Channel::ALL {
            assert_eq!(Channel::from_name(c.name()), Some(c));
        }
        assert_eq!(Channel::from_name("wobble"), None);
    }

    #[test]
    fn channel_identity_matches_additivity() {
        for c in Channel::ALL {
            let mut o = BoneOffsets::IDENTITY;
            o.set(c, c.identity());
            assert_eq!(o, BoneOffsets::IDENTITY, "{}", c.name());
        }
    }
}
