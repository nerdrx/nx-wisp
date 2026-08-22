//! The bone tree: cycles refused, binding, IK placement.

mod support;

use support::{canonical, isolate, shipped_doc, shipped_editor};

use wisp_editor::bones;
use wisp_editor::cmd::Command;
use wisp_editor::error::EditError;
use wisp_rig::math::Vec2;
use wisp_rig::skeleton::{BoneRest, BoneSpec, Skeleton};
use wisp_rig::skin::doc::SkinDoc;

fn index(doc: &SkinDoc, name: &str) -> usize {
    bones::index_of(doc, name).unwrap_or_else(|| panic!("the shipped skin has a bone {name:?}"))
}

/// The first bone that has a parent — a real child to try to loop.
fn a_child(doc: &SkinDoc) -> usize {
    doc.bones.iter().position(|b| !b.parent.is_empty()).expect("the skin has a child bone")
}

#[test]
fn reparenting_a_bone_under_its_own_child_is_refused() {
    isolate();
    let doc = shipped_doc();
    // Find a bone with a grandchild, so there is a real loop to close.
    let (parent, descendant) = doc
        .bones
        .iter()
        .enumerate()
        .find_map(|(i, b)| {
            let child = doc.bones.iter().find(|c| c.parent == b.name)?;
            let grandchild = doc.bones.iter().find(|g| g.parent == child.name)?;
            Some((i, grandchild.name.clone()))
        })
        .expect("the shipped skin has a three-deep chain");

    let err = bones::reparent(&doc, parent, &descendant).expect_err("this closes a loop");
    match err {
        EditError::BoneCycle { child, parent: p, chain } => {
            assert_eq!(p, descendant);
            assert_eq!(chain.first().map(String::as_str), Some(child.as_str()));
            assert_eq!(chain.last().map(String::as_str), Some(child.as_str()),
                "the reported chain must come back round to where it started");
            let msg = EditError::BoneCycle { child, parent: p, chain }.to_string();
            assert!(msg.contains("->"), "the message draws the loop: {msg}");
            assert!(!msg.contains('!'), "DESIGN.md §9: {msg}");
            assert!(msg.contains("pick a parent"), "it says what to do next: {msg}");
        }
        other => panic!("expected a cycle refusal, got {other}"),
    }
}

#[test]
fn a_bone_cannot_be_its_own_parent() {
    isolate();
    let doc = shipped_doc();
    let i = a_child(&doc);
    let name = doc.bones[i].name.clone();
    assert!(matches!(
        bones::reparent(&doc, i, &name),
        Err(EditError::SelfParent { .. })
    ));
}

#[test]
fn the_refusal_agrees_with_the_rigs_own_validator() {
    isolate();
    let doc = shipped_doc();
    // For every (bone, candidate parent) pair the editor refuses as a cycle,
    // building the skeleton anyway must also fail. The editor is a second
    // check in front of the rig, never a different opinion from it.
    let mut checked = 0usize;
    for i in 0..doc.bones.len() {
        for j in 0..doc.bones.len() {
            let parent = doc.bones[j].name.clone();
            let refused = bones::cycle_from(&doc, i, &parent).is_some();
            if !refused {
                continue;
            }
            checked += 1;
            let mut trial = doc.clone();
            trial.bones[i].parent = parent.clone();
            let specs: Vec<BoneSpec> = trial
                .bones
                .iter()
                .map(|b| BoneSpec {
                    name: b.name.clone(),
                    parent: (!b.parent.is_empty()).then(|| b.parent.clone()),
                    rest: BoneRest::default(),
                    length: 0.0,
                })
                .collect();
            assert!(
                Skeleton::build(&specs).is_err(),
                "the editor refused {:?} -> {parent:?} but the rig accepts it",
                doc.bones[i].name
            );
        }
    }
    assert!(checked > 0, "the shipped skin must offer at least one cycle to refuse");
}

#[test]
fn a_legal_reparent_keeps_the_bone_where_it_was_on_screen() {
    isolate();
    let mut doc = shipped_doc();
    let bone = a_child(&doc);
    let before = bones::head_of(&doc, bone).expect("the tree resolves");

    // Move it to the root: a different parent space, the same world position.
    let cmd = bones::reparent(&doc, bone, "").expect("reparenting to a root is legal");
    cmd.apply(&mut doc).unwrap();

    let after = bones::head_of(&doc, bone).expect("the tree still resolves");
    assert!(
        before.dist(after) < 1e-3,
        "reparenting moved the bone from {before:?} to {after:?}"
    );
    assert!(doc.bones[bone].parent.is_empty());
}

#[test]
fn deleting_a_bone_something_still_uses_is_refused_by_name() {
    isolate();
    let doc = shipped_doc();
    let root = index(&doc, "root");
    let err = bones::delete_bone(&doc, root).expect_err("root is used everywhere");
    let msg = err.to_string();
    assert!(msg.contains("root"), "{msg}");
    assert!(msg.contains("repoint it first"), "it says what to do next: {msg}");
}

#[test]
fn a_bone_nothing_references_can_be_deleted() {
    isolate();
    let mut doc = shipped_doc();
    let cmd = bones::add_bone(&doc, "spare", "root", Vec2::new(10.0, 10.0)).unwrap();
    cmd.apply(&mut doc).unwrap();
    let i = index(&doc, "spare");
    let cmd = bones::delete_bone(&doc, i).expect("nothing uses it yet");
    cmd.apply(&mut doc).unwrap();
    assert!(bones::index_of(&doc, "spare").is_none());
}

#[test]
fn a_duplicate_bone_name_is_refused() {
    isolate();
    let doc = shipped_doc();
    assert!(matches!(
        bones::add_bone(&doc, "root", "", Vec2::ZERO),
        Err(EditError::DuplicateName { kind: "bone", .. })
    ));
    assert!(matches!(
        bones::add_bone(&doc, "   ", "", Vec2::ZERO),
        Err(EditError::EmptyName { .. })
    ));
}

#[test]
fn dragging_out_a_child_gives_it_a_length_and_a_direction() {
    isolate();
    let mut doc = shipped_doc();
    let head = bones::head_of(&doc, index(&doc, "root")).unwrap();
    let target = head + Vec2::new(30.0, 0.0);
    let cmd = bones::drag_out_child(&doc, "root", "limb", target).unwrap();
    cmd.apply(&mut doc).unwrap();
    let i = index(&doc, "limb");
    assert!(doc.bones[i].length.expect("a dragged bone has a length").0 > 29.0);
    let tip = bones::tip_of(&doc, i).unwrap();
    assert!(tip.dist(target) < 0.5, "the tip should land where the drag ended: {tip:?}");
}

#[test]
fn a_new_bones_position_is_expressed_in_its_parents_space() {
    isolate();
    let mut doc = shipped_doc();
    // Pick a parent that is not at the origin and is rotated, so a naive
    // "just store the canvas coordinate" would be visibly wrong.
    let parent = doc
        .bones
        .iter()
        .position(|b| b.rot.map(|r| r.0.abs() > 1.0).unwrap_or(false) || b.pos[0].0.abs() > 1.0)
        .expect("some bone is offset or rotated");
    let name = doc.bones[parent].name.clone();
    let at = Vec2::new(40.0, 60.0);
    let cmd = bones::add_bone(&doc, "probe", &name, at).unwrap();
    cmd.apply(&mut doc).unwrap();
    let i = bones::index_of(&doc, "probe").unwrap();
    let landed = bones::head_of(&doc, i).unwrap();
    assert!(landed.dist(at) < 1e-3, "wanted {at:?}, landed at {landed:?}");
}

// ------------------------------------------------------------------ binding

#[test]
fn painting_a_weight_keeps_the_influences_summing_to_one() {
    isolate();
    let mut doc = shipped_doc();
    let shape = 0usize;
    let bone = doc.bones[1].name.clone();
    let cmd = bones::paint_weight(&doc, shape, 2, &bone, 0.4).unwrap();
    cmd.apply(&mut doc).unwrap();
    let w = doc.shapes[shape]
        .weights
        .iter()
        .find(|w| w.point == 2)
        .expect("the point now has an explicit weight");
    let total: f32 = w.weights.iter().map(|n| n.0).sum();
    assert!((total - 1.0).abs() < 1e-3, "weights sum to {total}");
    assert!(w.bones.len() <= wisp_rig::deform::MAX_INFLUENCES);
    assert!(w.weights.iter().all(|n| (0.0..=1.0).contains(&n.0)));
}

#[test]
fn a_painted_weight_never_leaves_a_point_weighted_to_nothing() {
    isolate();
    let mut doc = shipped_doc();
    let bone = doc.bones[1].name.clone();
    // Paint zero: the rig rejects a point whose weights are all zero, so the
    // editor must not be able to author one.
    let cmd = bones::paint_weight(&doc, 0, 3, &bone, 0.0).unwrap();
    cmd.apply(&mut doc).unwrap();
    let w = doc.shapes[0].weights.iter().find(|w| w.point == 3).unwrap();
    assert!(w.weights.iter().any(|n| n.0 > 0.0));
    // ...and the document still compiles.
    wisp_rig::Skin::compile(doc).expect("still a valid skin");
}

#[test]
fn a_weighted_shipped_skin_still_compiles() {
    isolate();
    let mut doc = shipped_doc();
    let bone = doc.bones[2].name.clone();
    let mut cmds = Vec::new();
    for point in 0..6 {
        cmds.push(bones::paint_weight(&doc, 1, point, &bone, 0.6).unwrap());
    }
    Command::Batch { label: "paint", cmds }.apply(&mut doc).unwrap();
    wisp_rig::Skin::compile(doc).expect("a painted skin compiles");
}

#[test]
fn the_falloff_brush_is_one_at_the_centre_and_zero_at_the_rim() {
    assert!((bones::falloff_weight(0.0, 10.0) - 1.0).abs() < 1e-6);
    assert!(bones::falloff_weight(10.0, 10.0).abs() < 1e-6);
    assert!(bones::falloff_weight(20.0, 10.0).abs() < 1e-6);
    // Smooth, so the brush has no visible edge in the deform.
    let a = bones::falloff_weight(4.0, 10.0);
    let b = bones::falloff_weight(5.0, 10.0);
    let c = bones::falloff_weight(6.0, 10.0);
    assert!(a > b && b > c);
}

// ----------------------------------------------------------------------- IK

#[test]
fn a_two_bone_chain_that_does_not_follow_the_tree_is_refused() {
    isolate();
    let doc = shipped_doc();
    // Two bones that are not parent and child.
    let roots: Vec<String> =
        doc.bones.iter().filter(|b| b.parent.is_empty()).map(|b| b.name.clone()).collect();
    let a = roots.first().cloned().unwrap_or_else(|| doc.bones[0].name.clone());
    let unrelated = doc
        .bones
        .iter()
        .find(|b| b.parent != a && b.name != a && !b.parent.is_empty())
        .expect("some unrelated bone")
        .name
        .clone();
    let err = bones::add_two_bone(&doc, "bad", &a, &unrelated, &unrelated, "none")
        .expect_err("a chain must follow the bone tree");
    assert!(err.to_string().contains("child"), "{err}");
}

#[test]
fn a_two_bone_chain_along_a_real_parent_chain_is_accepted_and_compiles() {
    isolate();
    let mut doc = shipped_doc();
    let (root, mid, end) = doc
        .bones
        .iter()
        .find_map(|b| {
            let child = doc.bones.iter().find(|c| c.parent == b.name)?;
            let grandchild = doc.bones.iter().find(|g| g.parent == child.name)?;
            Some((b.name.clone(), child.name.clone(), grandchild.name.clone()))
        })
        .expect("a three-deep chain");
    let cmd = bones::add_two_bone(&doc, "probe_ik", &root, &mid, &end, "cursor").unwrap();
    cmd.apply(&mut doc).unwrap();
    let skin = wisp_rig::Skin::compile(doc).expect("the rig accepts the chain we built");
    assert!(skin.iks.iter().any(|i| &*i.name == "probe_ik"));
}

#[test]
fn a_look_at_is_placed_on_one_bone_and_compiles() {
    isolate();
    let mut doc = shipped_doc();
    let bone = doc.bones[1].name.clone();
    let cmd = bones::add_look_at(&doc, "probe_look", &bone, "cursor", Some(24.0)).unwrap();
    cmd.apply(&mut doc).unwrap();
    let skin = wisp_rig::Skin::compile(doc).expect("the rig accepts the look-at");
    assert!(skin.iks.iter().any(|i| &*i.name == "probe_look"));
}

#[test]
fn a_spring_chain_must_be_contiguous() {
    isolate();
    let doc = shipped_doc();
    assert!(bones::add_spring_chain(&doc, "short", &["root".into()]).is_err());
    let bad: Vec<String> = vec!["root".into(), "root".into()];
    assert!(bones::add_spring_chain(&doc, "loop", &bad).is_err());
}

// ------------------------------------------------------------------ the tree

#[test]
fn the_tree_lists_every_bone_exactly_once_with_parents_before_children() {
    isolate();
    let doc = shipped_doc();
    let rows = bones::tree_rows(&doc);
    assert_eq!(rows.len(), doc.bones.len(), "every bone gets a row");
    let mut seen = std::collections::HashSet::new();
    for r in &rows {
        assert!(seen.insert(r.bone), "bone {} listed twice", r.bone);
    }
    let mut position = vec![usize::MAX; doc.bones.len()];
    for (i, r) in rows.iter().enumerate() {
        position[r.bone] = i;
    }
    for (i, b) in doc.bones.iter().enumerate() {
        if b.parent.is_empty() {
            continue;
        }
        let p = bones::index_of(&doc, &b.parent).expect("a real parent");
        assert!(position[p] < position[i], "{} listed before its parent", b.name);
        assert!(rows[position[i]].depth > rows[position[p]].depth);
    }
}

#[test]
fn the_editor_surfaces_a_refused_reparent_in_the_status_strip() {
    let mut ed = shipped_editor();
    let (parent, descendant) = ed
        .doc()
        .bones
        .iter()
        .enumerate()
        .find_map(|(i, b)| {
            let child = ed.doc().bones.iter().find(|c| c.parent == b.name)?;
            let g = ed.doc().bones.iter().find(|g| g.parent == child.name)?;
            Some((i, g.name.clone()))
        })
        .unwrap();
    let before = canonical(ed.doc());
    assert!(ed.reparent(parent, &descendant).is_err());
    assert_eq!(before, canonical(ed.doc()), "a refused edit changes nothing");
    assert!(ed.status().contains("loop"), "the status strip explains it: {}", ed.status());
    assert!(!ed.can_undo(), "a refused edit puts nothing on the undo stack");
}
