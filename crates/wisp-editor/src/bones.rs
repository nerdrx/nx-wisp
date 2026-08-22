//! The bone tree: create, reparent, bind, and place IK chains.
//!
//! # A cycle is refused before it is a document
//!
//! `wisp-rig`'s `Skeleton::build` rejects a cycle, which means a skin with one
//! never loads — the operator would drag a bone onto its own grandchild and
//! the *whole rig* would go dark with a validation error. That is a terrible
//! way to learn what you just did. So [`reparent`] walks the ancestors first
//! and refuses with the loop spelled out (`tail2 -> tail1 -> tail2`), and the
//! document is never invalid for even one frame.
//!
//! The rig's validator stays the authority — this is a *second* check in front
//! of it, not a replacement, and `tests/bones.rs` asserts the two agree.
//!
//! # Rest space
//!
//! A bone's `pos` is in its **parent's** space. Reparenting therefore has to
//! decide what the operator meant: keep the number, or keep the bone where it
//! is on screen. [`reparent`] keeps it **where it is** — it recomputes `pos`
//! in the new parent's space — because the operator is looking at the canvas,
//! not at the number.

use wisp_rig::math::{rad_to_deg, Affine, Vec2};
use wisp_rig::skeleton::{BoneRest, BoneSpec, Skeleton};
use wisp_rig::skin::doc::{pt, to_pt, BoneDoc, ChainDoc, IkDoc, Num, ShapeDoc, SkinDoc, WeightDoc};

use crate::cmd::Command;
use crate::error::EditError;

/// Build the rig's own skeleton from the working document, so gizmos are drawn
/// from the same rest transforms the runtime uses.
pub fn skeleton_of(doc: &SkinDoc) -> Result<Skeleton, EditError> {
    let specs: Vec<BoneSpec> = doc
        .bones
        .iter()
        .map(|b| BoneSpec {
            name: b.name.clone(),
            parent: if b.parent.is_empty() { None } else { Some(b.parent.clone()) },
            rest: BoneRest {
                pos: pt(b.pos),
                rot: wisp_rig::math::deg_to_rad(b.rot.map(|n| n.0).unwrap_or(0.0)),
                scale: b.scale.map(pt).unwrap_or(Vec2::ONE),
            },
            length: b.length.map(|n| n.0).unwrap_or(0.0),
        })
        .collect();
    Skeleton::build(&specs).map_err(|e| match e {
        wisp_rig::skeleton::SkeletonError::Cycle(chain) => EditError::BoneCycle {
            child: chain.first().cloned().unwrap_or_default(),
            parent: chain.get(1).cloned().unwrap_or_default(),
            chain,
        },
        wisp_rig::skeleton::SkeletonError::DuplicateName(name) => {
            EditError::DuplicateName { kind: "bone", name }
        }
        wisp_rig::skeleton::SkeletonError::UnknownParent(bone, parent) => {
            let _ = bone;
            EditError::NoSuchName { kind: "bone", name: parent }
        }
        wisp_rig::skeleton::SkeletonError::Empty => {
            EditError::NoSuchIndex { kind: "bone", at: 0, len: 0 }
        }
    })
}

pub fn index_of(doc: &SkinDoc, name: &str) -> Option<usize> {
    doc.bones.iter().position(|b| b.name == name)
}

fn require(doc: &SkinDoc, name: &str) -> Result<usize, EditError> {
    index_of(doc, name).ok_or(EditError::NoSuchName { kind: "bone", name: name.to_string() })
}

/// Would making `child` a child of `parent` close a loop? Returns the loop.
///
/// Walks upward from the proposed parent. If the walk reaches `child`, the
/// chain from `child` back around to itself is the loop, and it is returned in
/// reading order so the error message is a path, not a set.
pub fn cycle_from(doc: &SkinDoc, child: usize, parent: &str) -> Option<Vec<String>> {
    if parent.is_empty() {
        return None;
    }
    let child_name = doc.bones[child].name.clone();
    if parent == child_name {
        return Some(vec![child_name.clone(), child_name]);
    }
    let mut chain = vec![child_name.clone()];
    let mut cursor = parent.to_string();
    // The document is finite; the guard stops a loop that already exists in
    // the file from spinning here.
    for _ in 0..=doc.bones.len() {
        chain.push(cursor.clone());
        if cursor == child_name {
            return Some(chain);
        }
        match doc.bones.iter().find(|b| b.name == cursor) {
            Some(b) if !b.parent.is_empty() => cursor = b.parent.clone(),
            _ => return None,
        }
    }
    Some(chain)
}

/// Rest world transform of every bone, or the refusal that says why not.
pub fn rest_world(doc: &SkinDoc) -> Result<Vec<Affine>, EditError> {
    Ok(skeleton_of(doc)?.rest_world().to_vec())
}

/// Where a bone's head sits in canvas units at rest.
pub fn head_of(doc: &SkinDoc, bone: usize) -> Result<Vec2, EditError> {
    Ok(rest_world(doc)?
        .get(bone)
        .copied()
        .ok_or(EditError::NoSuchIndex { kind: "bone", at: bone, len: doc.bones.len() })?
        .origin())
}

/// Where a bone's tip sits — head plus `length` along its own +x.
pub fn tip_of(doc: &SkinDoc, bone: usize) -> Result<Vec2, EditError> {
    let sk = skeleton_of(doc)?;
    let w = sk
        .rest_world()
        .get(bone)
        .copied()
        .ok_or(EditError::NoSuchIndex { kind: "bone", at: bone, len: doc.bones.len() })?;
    Ok(w.apply(sk.bone(bone).tip_local()))
}

// ------------------------------------------------------------------ creating

/// Add a bone whose head lands at `at` in **canvas** units, parented to
/// `parent` (empty for a root).
pub fn add_bone(
    doc: &SkinDoc,
    name: &str,
    parent: &str,
    at: Vec2,
) -> Result<Command, EditError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(EditError::EmptyName { kind: "bone", name: name.to_string() });
    }
    if doc.bones.iter().any(|b| b.name == trimmed) {
        return Err(EditError::DuplicateName { kind: "bone", name: trimmed.to_string() });
    }
    let local = if parent.is_empty() {
        at
    } else {
        let pi = require(doc, parent)?;
        let world = rest_world(doc)?[pi];
        world.inverse().apply(at)
    };
    Ok(Command::InsertBone {
        at: doc.bones.len(),
        value: BoneDoc {
            name: trimmed.to_string(),
            parent: parent.to_string(),
            pos: to_pt(local),
            rot: None,
            scale: None,
            length: None,
        },
    })
}

/// A child bone created by dragging from `parent` out to `to`.
///
/// The child starts at the parent's **tip** — where a limb actually continues
/// from — and is rotated and lengthened so that its own tip lands exactly
/// where the drag ended. That is what makes "drag out a limb" produce a bone
/// you can immediately IK against instead of a zero-length stub sitting on top
/// of its parent.
pub fn drag_out_child(
    doc: &SkinDoc,
    parent: &str,
    name: &str,
    to: Vec2,
) -> Result<Command, EditError> {
    let pi = require(doc, parent)?;
    let sk = skeleton_of(doc)?;
    let world = sk.rest_world()[pi];
    // Where the child's head goes, in the parent's space.
    let head_local = sk.bone(pi).tip_local();
    let target_local = world.inverse().apply(to);
    let d = target_local - head_local;
    let len = d.len();
    let rot = if len > 1e-4 { rad_to_deg(d.y.atan2(d.x)) } else { 0.0 };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(EditError::EmptyName { kind: "bone", name: name.to_string() });
    }
    if doc.bones.iter().any(|b| b.name == trimmed) {
        return Err(EditError::DuplicateName { kind: "bone", name: trimmed.to_string() });
    }
    Ok(Command::InsertBone {
        at: doc.bones.len(),
        value: BoneDoc {
            name: trimmed.to_string(),
            parent: parent.to_string(),
            pos: to_pt(head_local),
            rot: (rot.abs() > 1e-4).then_some(Num(rot)),
            scale: None,
            length: (len > 1e-4).then_some(Num(len)),
        },
    })
}

/// Reparent a bone, keeping it where it is on the canvas.
pub fn reparent(doc: &SkinDoc, bone: usize, parent: &str) -> Result<Command, EditError> {
    let b = doc
        .bones
        .get(bone)
        .ok_or(EditError::NoSuchIndex { kind: "bone", at: bone, len: doc.bones.len() })?;
    if parent == b.name {
        return Err(EditError::SelfParent { name: b.name.clone() });
    }
    if let Some(chain) = cycle_from(doc, bone, parent) {
        return Err(EditError::BoneCycle {
            child: b.name.clone(),
            parent: parent.to_string(),
            chain,
        });
    }
    if !parent.is_empty() {
        require(doc, parent)?;
    }
    // Keep the head where it is: express the old world position in the new
    // parent's space.
    let world = rest_world(doc)?;
    let head = world[bone].origin();
    let local = if parent.is_empty() {
        head
    } else {
        let pi = require(doc, parent)?;
        world[pi].inverse().apply(head)
    };
    Ok(Command::SetBone {
        at: bone,
        value: BoneDoc { parent: parent.to_string(), pos: to_pt(local), ..b.clone() },
    })
}

/// Move a bone's rest head to `at`, in canvas units.
pub fn move_bone(doc: &SkinDoc, bone: usize, at: Vec2) -> Result<Command, EditError> {
    let b = doc
        .bones
        .get(bone)
        .ok_or(EditError::NoSuchIndex { kind: "bone", at: bone, len: doc.bones.len() })?;
    let local = if b.parent.is_empty() {
        at
    } else {
        let pi = require(doc, &b.parent)?;
        rest_world(doc)?[pi].inverse().apply(at)
    };
    Ok(Command::SetBone { at: bone, value: BoneDoc { pos: to_pt(local), ..b.clone() } })
}

/// Set a bone's rest rotation, in degrees.
pub fn set_bone_rotation(doc: &SkinDoc, bone: usize, deg: f32) -> Result<Command, EditError> {
    let b = doc
        .bones
        .get(bone)
        .ok_or(EditError::NoSuchIndex { kind: "bone", at: bone, len: doc.bones.len() })?;
    Ok(Command::SetBone {
        at: bone,
        value: BoneDoc { rot: (deg.abs() > 1e-6).then_some(Num(deg)), ..b.clone() },
    })
}

/// Set a bone's length, which is what IK and auto-binding measure against.
pub fn set_bone_length(doc: &SkinDoc, bone: usize, length: f32) -> Result<Command, EditError> {
    let b = doc
        .bones
        .get(bone)
        .ok_or(EditError::NoSuchIndex { kind: "bone", at: bone, len: doc.bones.len() })?;
    Ok(Command::SetBone {
        at: bone,
        value: BoneDoc { length: (length > 1e-6).then_some(Num(length)), ..b.clone() },
    })
}

/// Everything in the document that names this bone. Used to refuse a delete
/// that would leave a dangling reference, and to explain the refusal.
pub fn references_to(doc: &SkinDoc, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for b in &doc.bones {
        if b.parent == name {
            out.push(format!("bone {:?}", b.name));
        }
    }
    for s in &doc.shapes {
        if s.bind == name {
            out.push(format!("shape {:?}", s.name));
        }
        if let Some(a) = &s.bind_auto {
            if a.bones.iter().any(|n| n == name) {
                out.push(format!("shape {:?}'s auto-bind", s.name));
            }
        }
        for w in &s.weights {
            if w.bones.iter().any(|n| n == name) {
                out.push(format!("shape {:?}, point {}", s.name, w.point));
            }
        }
    }
    for g in &doc.gradients {
        if g.follow_bone == name {
            out.push(format!("gradient {:?}", g.name));
        }
    }
    for i in &doc.iks {
        if [&i.bone, &i.root, &i.mid, &i.end].iter().any(|n| n.as_str() == name) {
            out.push(format!("ik {:?}", i.name));
        }
    }
    for c in &doc.chains {
        if c.bones.iter().any(|n| n == name) {
            out.push(format!("chain {:?}", c.name));
        }
    }
    for c in &doc.clips {
        for t in &c.tracks {
            if t.bone == name {
                out.push(format!("clip {:?}, track {:?}", c.name, t.channel));
                break;
            }
        }
    }
    for m in [&doc.motion.squash_bone, &doc.motion.lean_bone, &doc.motion.light_bone] {
        if m == name {
            out.push("the procedural motion table".to_string());
        }
    }
    out
}

/// Delete a bone, refusing while anything still names it.
pub fn delete_bone(doc: &SkinDoc, bone: usize) -> Result<Command, EditError> {
    let b = doc
        .bones
        .get(bone)
        .ok_or(EditError::NoSuchIndex { kind: "bone", at: bone, len: doc.bones.len() })?;
    let refs = references_to(doc, &b.name);
    if let Some(first) = refs.first() {
        return Err(EditError::BoneStillUsed {
            name: b.name.clone(),
            referenced_by: if refs.len() == 1 {
                first.clone()
            } else {
                format!("{first} and {} other places", refs.len() - 1)
            },
        });
    }
    Ok(Command::RemoveBone { at: bone })
}

// ------------------------------------------------------------------- binding

/// Bind every point of a shape rigidly to one bone, and drop any per-point
/// overrides — the simple case, and the one most shapes want.
pub fn bind_rigid(doc: &SkinDoc, shape: usize, bone: &str) -> Result<Command, EditError> {
    require(doc, bone)?;
    let s = doc
        .shapes
        .get(shape)
        .ok_or(EditError::NoSuchIndex { kind: "shape", at: shape, len: doc.shapes.len() })?;
    Ok(Command::SetShape {
        at: shape,
        value: Box::new(ShapeDoc {
            bind: bone.to_string(),
            bind_auto: None,
            weights: Vec::new(),
            ..s.clone()
        }),
    })
}

/// Weight a shape against a set of bones by distance.
pub fn bind_auto(
    doc: &SkinDoc,
    shape: usize,
    bones: &[String],
    falloff: Option<f32>,
    power: Option<f32>,
) -> Result<Command, EditError> {
    for b in bones {
        require(doc, b)?;
    }
    let s = doc
        .shapes
        .get(shape)
        .ok_or(EditError::NoSuchIndex { kind: "shape", at: shape, len: doc.shapes.len() })?;
    Ok(Command::SetShape {
        at: shape,
        value: Box::new(ShapeDoc {
            bind: String::new(),
            bind_auto: Some(wisp_rig::skin::doc::AutoBindDoc {
                bones: bones.to_vec(),
                falloff: falloff.map(Num),
                power: power.map(Num),
            }),
            ..s.clone()
        }),
    })
}

/// Compute the weight one bone should have over a point, given a falloff
/// radius. This is the weight brush: 1 at the bone, 0 at `falloff`, smooth in
/// between (`smoothstep`, so the brush has no hard edge to see in the deform).
pub fn falloff_weight(distance: f32, falloff: f32) -> f32 {
    if falloff <= 1e-6 {
        return if distance <= 1e-6 { 1.0 } else { 0.0 };
    }
    let t = (1.0 - (distance / falloff)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Paint one point's weight towards `bone` by `amount`, renormalising the rest
/// so the influences still sum to one.
///
/// `amount` of 1 makes the point rigid to `bone`; 0 removes its influence.
pub fn paint_weight(
    doc: &SkinDoc,
    shape: usize,
    point: usize,
    bone: &str,
    amount: f32,
) -> Result<Command, EditError> {
    require(doc, bone)?;
    let s = doc
        .shapes
        .get(shape)
        .ok_or(EditError::NoSuchIndex { kind: "shape", at: shape, len: doc.shapes.len() })?;
    let amount = amount.clamp(0.0, 1.0);

    // Start from whatever this point already had: an explicit weight, or the
    // shape's rigid bind, or nothing.
    let mut names: Vec<String> = Vec::new();
    let mut values: Vec<f32> = Vec::new();
    if let Some(w) = s.weights.iter().find(|w| w.point == point) {
        names = w.bones.clone();
        values = w.weights.iter().map(|n| n.0).collect();
    } else if !s.bind.is_empty() {
        names.push(s.bind.clone());
        values.push(1.0);
    }

    let idx = names.iter().position(|n| n == bone);
    let rest: f32 = values
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != idx)
        .map(|(_, v)| *v)
        .sum();
    let scale = if rest > 1e-6 { (1.0 - amount) / rest } else { 0.0 };
    for (i, v) in values.iter_mut().enumerate() {
        if Some(i) != idx {
            *v *= scale;
        }
    }
    match idx {
        Some(i) => values[i] = amount,
        None => {
            names.push(bone.to_string());
            values.push(amount);
        }
    }
    // Drop influences that rounded away, and refuse to leave the point
    // weighted to nothing — the rig rejects that, so the editor never writes
    // it.
    let mut keep: Vec<(String, f32)> = names
        .into_iter()
        .zip(values)
        .filter(|(_, v)| *v > 1e-4)
        .collect();
    if keep.is_empty() {
        keep.push((bone.to_string(), 1.0));
    }
    keep.sort_by(|a, b| b.1.total_cmp(&a.1));
    keep.truncate(wisp_rig::deform::MAX_INFLUENCES);
    let total: f32 = keep.iter().map(|(_, v)| *v).sum();
    let value = WeightDoc {
        point,
        bones: keep.iter().map(|(n, _)| n.clone()).collect(),
        weights: keep.iter().map(|(_, v)| Num(v / total)).collect(),
    };
    Ok(Command::SetWeight { shape, point, value: Some(value) })
}

/// Clear one point's explicit weight, letting `bind` / `bind_auto` decide.
pub fn clear_weight(shape: usize, point: usize) -> Command {
    Command::SetWeight { shape, point, value: None }
}

// ------------------------------------------------------------------------ IK

/// Is `child` a direct child of `parent`? A chain must follow the bone tree,
/// and the rig refuses one that does not.
pub fn is_child_of(doc: &SkinDoc, child: &str, parent: &str) -> bool {
    doc.bones.iter().any(|b| b.name == child && b.parent == parent)
}

/// Place a look-at constraint on one bone.
pub fn add_look_at(
    doc: &SkinDoc,
    name: &str,
    bone: &str,
    target: &str,
    max_deg: Option<f32>,
) -> Result<Command, EditError> {
    require(doc, bone)?;
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(EditError::EmptyName { kind: "IK chain", name: name.to_string() });
    }
    if doc.iks.iter().any(|i| i.name == trimmed) {
        return Err(EditError::DuplicateName { kind: "IK chain", name: trimmed.to_string() });
    }
    Ok(Command::InsertIk {
        at: doc.iks.len(),
        value: Box::new(IkDoc {
            name: trimmed.to_string(),
            kind: "look_at".into(),
            target: target.to_string(),
            weight: None,
            bone: bone.to_string(),
            forward: None,
            max_deg: max_deg.map(Num),
            root: String::new(),
            mid: String::new(),
            end: String::new(),
            bend_positive: None,
        }),
    })
}

/// Place a two-bone chain, checking that root → mid → end really is a parent
/// chain **before** the document is changed.
pub fn add_two_bone(
    doc: &SkinDoc,
    name: &str,
    root: &str,
    mid: &str,
    end: &str,
    target: &str,
) -> Result<Command, EditError> {
    require(doc, root)?;
    require(doc, mid)?;
    require(doc, end)?;
    if !is_child_of(doc, mid, root) {
        return Err(EditError::NoSuchName {
            kind: "child of the chain's root",
            name: mid.to_string(),
        });
    }
    if !is_child_of(doc, end, mid) {
        return Err(EditError::NoSuchName {
            kind: "child of the chain's middle bone",
            name: end.to_string(),
        });
    }
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(EditError::EmptyName { kind: "IK chain", name: name.to_string() });
    }
    if doc.iks.iter().any(|i| i.name == trimmed) {
        return Err(EditError::DuplicateName { kind: "IK chain", name: trimmed.to_string() });
    }
    Ok(Command::InsertIk {
        at: doc.iks.len(),
        value: Box::new(IkDoc {
            name: trimmed.to_string(),
            kind: "two_bone".into(),
            target: target.to_string(),
            weight: None,
            bone: String::new(),
            forward: None,
            max_deg: None,
            root: root.to_string(),
            mid: mid.to_string(),
            end: end.to_string(),
            bend_positive: None,
        }),
    })
}

/// Place a secondary-motion chain over a contiguous run of bones.
pub fn add_spring_chain(
    doc: &SkinDoc,
    name: &str,
    bones: &[String],
) -> Result<Command, EditError> {
    if bones.len() < 2 {
        return Err(EditError::NoSuchIndex { kind: "bone in the chain", at: bones.len(), len: 2 });
    }
    for b in bones {
        require(doc, b)?;
    }
    for pair in bones.windows(2) {
        if !is_child_of(doc, &pair[1], &pair[0]) {
            return Err(EditError::NoSuchName {
                kind: "child of the bone before it",
                name: pair[1].clone(),
            });
        }
    }
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(EditError::EmptyName { kind: "spring chain", name: name.to_string() });
    }
    if doc.chains.iter().any(|c| c.name == trimmed) {
        return Err(EditError::DuplicateName { kind: "spring chain", name: trimmed.to_string() });
    }
    Ok(Command::InsertChain {
        at: doc.chains.len(),
        value: Box::new(ChainDoc {
            name: trimmed.to_string(),
            bones: bones.to_vec(),
            stiffness: None,
            damping: None,
            mass: None,
            gravity: None,
            drag: None,
            stiff_length: None,
        }),
    })
}

/// The bone tree as rows the panel draws: index, depth, and whether it has
/// children. Roots come first, each followed by its subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeRow {
    pub bone: usize,
    pub depth: usize,
    pub has_children: bool,
}

pub fn tree_rows(doc: &SkinDoc) -> Vec<TreeRow> {
    let mut rows = Vec::with_capacity(doc.bones.len());
    let mut seen = vec![false; doc.bones.len()];
    fn walk(
        doc: &SkinDoc,
        parent: &str,
        depth: usize,
        rows: &mut Vec<TreeRow>,
        seen: &mut [bool],
    ) {
        for (i, b) in doc.bones.iter().enumerate() {
            if b.parent != parent || seen[i] {
                continue;
            }
            seen[i] = true;
            let has_children = doc.bones.iter().any(|c| c.parent == b.name);
            rows.push(TreeRow { bone: i, depth, has_children });
            walk(doc, &b.name, depth + 1, rows, seen);
        }
    }
    walk(doc, "", 0, &mut rows, &mut seen);
    // Anything left is inside a cycle or points at a missing parent. Show it
    // rather than hiding it: the tree panel is where the operator fixes it.
    for (i, _) in doc.bones.iter().enumerate() {
        if !seen[i] {
            rows.push(TreeRow { bone: i, depth: 0, has_children: false });
        }
    }
    rows
}
