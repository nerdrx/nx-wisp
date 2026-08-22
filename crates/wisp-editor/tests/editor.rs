//! The editor end to end: tools, puppet mode, the state-machine view, and the
//! chrome — all driven with no GPU and no compositor.

mod support;

use support::{canonical, isolate, shipped_editor, shipped_doc};

use wisp_editor::editor::{Editor, Key};
use wisp_editor::graph::{self, MoodGraph, StateNode};
use wisp_editor::panels::{self, Action, PanelState};
use wisp_editor::puppet::{grip_for, Grip, Puppet};
use wisp_editor::select::{SelectMode, Target, Tool};
use wisp_editor::text::Dry;
use wisp_paint::geom::{Point, Rect};
use wisp_paint::scene::Scene;
use wisp_paint::text::TextEngine;
use wisp_rig::math::Vec2;

const BOUNDS: Rect = Rect { x: 0.0, y: 0.0, w: 1440.0, h: 900.0 };

fn ready() -> (Editor, TextEngine) {
    let mut ed = shipped_editor();
    let mut text = TextEngine::new();
    let _ = ed.build_panels(BOUNDS, &mut text);
    ed.fit();
    ed.repose();
    (ed, text)
}

// ---------------------------------------------------------------- the shell

#[test]
fn the_shipped_skin_opens_valid() {
    let ed = shipped_editor();
    assert!(ed.validation().ok(), "{:?}", ed.validation().problems);
    assert!(ed.skin().is_some());
    assert!(!ed.dirty());
    assert_eq!(ed.doc().clips.len(), 16);
    assert_eq!(ed.doc().expressions.len(), 8);
}

#[test]
fn a_blank_skin_opens_and_can_be_built_up() {
    isolate();
    let mut ed = Editor::blank("Probe");
    assert!(ed.validation().ok(), "{:?}", ed.validation().problems);
    let mut text = TextEngine::new();
    let _ = ed.build_panels(BOUNDS, &mut text);
    ed.perform(Action::AddShape);
    ed.perform(Action::AddBone);
    assert_eq!(ed.doc().shapes.len(), 1);
    assert_eq!(ed.doc().bones.len(), 2);
    assert!(ed.validation().ok(), "{:?}", ed.validation().problems);
}

#[test]
fn the_regions_tile_the_window_without_overlapping() {
    let f = panels::frames(BOUNDS);
    assert_eq!(f.toolbar.y, BOUNDS.y);
    assert!((f.status.bottom() - BOUNDS.bottom()).abs() < 1e-3);
    assert!(f.canvas.y >= f.toolbar.bottom() - 1e-3);
    assert!(f.canvas.bottom() <= f.timeline.y + 1e-3);
    assert!(f.timeline.bottom() <= f.status.y + 1e-3);
    assert!(f.left.right() <= f.canvas.x + 1e-3);
    assert!(f.canvas.right() <= f.right.x + 1e-3);
    assert!(f.canvas.w > 0.0 && f.canvas.h > 0.0);
}

#[test]
fn a_narrow_window_drops_the_side_panels_rather_than_going_negative() {
    let f = panels::frames(Rect::new(0.0, 0.0, 400.0, 300.0));
    assert_eq!(f.left.w, 0.0);
    assert_eq!(f.right.w, 0.0);
    assert!(f.canvas.w > 0.0);
    assert!(f.timeline.h >= 0.0);
}

// -------------------------------------------------------------------- tools

#[test]
fn the_select_tool_picks_a_point_and_a_drag_moves_it() {
    let (mut ed, _text) = ready();
    let shape = ed.doc().shapes.iter().position(|s| s.name == "body").unwrap();
    let p = wisp_editor::canvas::points_of(ed.doc(), shape).unwrap()[0];
    ed.view.centre_on(p);
    let at = ed.view.to_screen(p);

    ed.tool = Tool::Select;
    ed.pointer_down(at, SelectMode::Replace);
    assert!(
        ed.selection.iter().any(|t| matches!(t, Target::Point { .. } | Target::Bone(_))),
        "a press on a handle selects something"
    );
    let moved = Point::new(at.x + 40.0, at.y + 20.0);
    ed.pointer_move(moved);
    ed.pointer_up(moved);

    assert!(ed.dirty());
    let after = wisp_editor::canvas::points_of(ed.doc(), shape).unwrap()[0];
    if ed.selection.iter().any(|t| matches!(t, Target::Point { .. })) {
        assert!(after.dist(p) > 1.0, "the drag should have moved the point");
    }
    // One undo puts the whole drag back.
    let depth_before = ed.can_undo();
    assert!(depth_before);
    ed.undo().unwrap();
    let back = wisp_editor::canvas::points_of(ed.doc(), shape).unwrap()[0];
    assert!(back.dist(p) < 1e-3, "one undo must restore the pre-drag position");
}

#[test]
fn a_click_on_empty_canvas_clears_the_selection() {
    let (mut ed, _text) = ready();
    ed.selection.set(Target::Shape(0));
    ed.tool = Tool::Select;
    // Far outside the canvas contents but inside the viewport rect.
    ed.view.origin = Vec2::new(-100_000.0, -100_000.0);
    let at = ed.frames().canvas.centre();
    ed.pointer_down(at, SelectMode::Replace);
    ed.pointer_up(at);
    assert!(ed.selection.is_empty());
}

#[test]
fn the_pen_tool_appends_a_point_to_the_selected_shape() {
    let (mut ed, _text) = ready();
    ed.perform(Action::AddShape);
    let shape = ed.doc().shapes.len() - 1;
    let before = wisp_editor::canvas::points_of(ed.doc(), shape).unwrap().len();
    ed.tool = Tool::Pen;
    let at = Point::new(ed.frames().canvas.x + 60.0, ed.frames().canvas.y + 60.0);
    ed.pointer_down(at, SelectMode::Replace);
    ed.pointer_up(at);
    let after = wisp_editor::canvas::points_of(ed.doc(), shape).unwrap().len();
    assert_eq!(after, before + 1);
    assert!(ed.validation().ok(), "{:?}", ed.validation().problems);
}

#[test]
fn the_erase_tool_deletes_the_point_under_the_pointer() {
    let (mut ed, _text) = ready();
    ed.perform(Action::AddShape);
    let shape = ed.doc().shapes.len() - 1;
    let pts = wisp_editor::canvas::points_of(ed.doc(), shape).unwrap();
    let before = pts.len();
    ed.selection.set(Target::Shape(shape));
    ed.view.centre_on(pts[0]);
    let at = ed.view.to_screen(pts[0]);
    ed.tool = Tool::Erase;
    ed.pointer_down(at, SelectMode::Replace);
    ed.pointer_up(at);
    let after = wisp_editor::canvas::points_of(ed.doc(), shape).unwrap().len();
    assert!(after < before, "{before} -> {after}");
}

#[test]
fn the_bone_tool_drags_out_a_child() {
    let (mut ed, _text) = ready();
    let before = ed.doc().bones.len();
    let skin = ed.skin().unwrap().clone();
    let head = ed.preview().unwrap().pose().world_pos(0);
    ed.view.centre_on(head);
    let from = ed.view.to_screen(head);
    let to = Point::new(from.x + 80.0, from.y - 40.0);

    ed.tool = Tool::Bone;
    ed.pointer_down(from, SelectMode::Replace);
    ed.pointer_move(to);
    ed.pointer_up(to);

    assert_eq!(ed.doc().bones.len(), before + 1, "status: {}", ed.status());
    let child = ed.doc().bones.last().unwrap();
    assert_eq!(child.parent, skin.skeleton.bone(0).name.to_string());
    assert!(ed.validation().ok(), "{:?}", ed.validation().problems);
}

#[test]
fn the_ik_tool_needs_three_bones_and_then_places_a_chain() {
    let (mut ed, _text) = ready();
    let skin = ed.skin().unwrap().clone();
    // A real root -> mid -> end chain out of the shipped skeleton.
    let (root, mid, end) = (0..skin.skeleton.len())
        .find_map(|r| {
            let m = (0..skin.skeleton.len()).find(|i| skin.skeleton.bone(*i).parent == Some(r))?;
            let e = (0..skin.skeleton.len()).find(|i| skin.skeleton.bone(*i).parent == Some(m))?;
            Some((r, m, e))
        })
        .expect("a three-deep chain");

    let before = ed.doc().iks.len();
    ed.tool = Tool::Ik;
    for b in [root, mid, end] {
        let p = ed.preview().unwrap().pose().world_pos(b);
        ed.view.centre_on(p);
        let at = ed.view.to_screen(p);
        ed.pointer_down(at, SelectMode::Replace);
        ed.pointer_up(at);
    }
    assert_eq!(ed.doc().iks.len(), before + 1, "status: {}", ed.status());
    assert!(ed.validation().ok(), "{:?}", ed.validation().problems);
}

#[test]
fn the_weight_tool_needs_a_bone_and_says_so() {
    let (mut ed, _text) = ready();
    ed.tool = Tool::Weight;
    ed.selection.set(Target::Shape(0));
    let at = ed.frames().canvas.centre();
    ed.pointer_down(at, SelectMode::Replace);
    ed.pointer_up(at);
    assert!(ed.status().contains("bone"), "{}", ed.status());
}

#[test]
fn the_weight_tool_paints_where_it_is_dragged() {
    let (mut ed, _text) = ready();
    let shape = ed.doc().shapes.iter().position(|s| s.name == "body").unwrap();
    let bone = 2usize;
    ed.selection.set(Target::Shape(shape));
    ed.selection.apply(Target::Bone(bone), SelectMode::Add);
    ed.tool = Tool::Weight;

    let p = wisp_editor::canvas::points_of(ed.doc(), shape).unwrap()[0];
    ed.view.centre_on(p);
    let at = ed.view.to_screen(p);
    ed.pointer_down(at, SelectMode::Replace);
    ed.pointer_up(at);

    assert!(!ed.doc().shapes[shape].weights.is_empty(), "status: {}", ed.status());
    assert!(ed.validation().ok(), "{:?}", ed.validation().problems);
}

#[test]
fn panning_and_zooming_stay_inside_the_limits() {
    let (mut ed, _text) = ready();
    let at = ed.frames().canvas.centre();
    for _ in 0..500 {
        ed.wheel(at, 1.0);
    }
    assert!(ed.view.zoom <= wisp_editor::view::MAX_ZOOM);
    ed.tool = Tool::Pan;
    let before = ed.view.origin;
    ed.pointer_down(at, SelectMode::Replace);
    ed.pointer_move(Point::new(at.x + 100.0, at.y));
    ed.pointer_up(Point::new(at.x + 100.0, at.y));
    assert_ne!(before.x, ed.view.origin.x);
}

// ------------------------------------------------------------- puppet mode

#[test]
fn a_grip_is_chosen_from_what_the_skin_already_declares() {
    isolate();
    let skin = wisp_rig::Skin::compile(shipped_doc()).unwrap();
    // Every bone that is part of a two-bone IK chain grips as that chain.
    for def in &skin.iks {
        if let wisp_rig::skin::IkKind::TwoBone { end, .. } = def.kind {
            assert!(matches!(grip_for(&skin, end), Grip::TwoBone { .. }));
        }
    }
    // A bone with a length and a parent aims; a length-less root translates.
    let aiming = (0..skin.skeleton.len())
        .find(|i| skin.skeleton.bone(*i).length > 0.0 && skin.skeleton.bone(*i).parent.is_some());
    if let Some(i) = aiming {
        assert!(matches!(
            grip_for(&skin, i),
            Grip::Aim { .. } | Grip::TwoBone { .. }
        ));
    }
    assert!(matches!(grip_for(&skin, 0), Grip::Translate { .. } | Grip::Aim { .. }));
}

#[test]
fn dragging_a_limb_changes_the_pose_and_keyframes_it() {
    let (mut ed, _text) = ready();
    let skin = ed.skin().unwrap().clone();
    // Pick a bone that aims, so the drag is a rotation with a visible effect.
    let bone = (0..skin.skeleton.len())
        .find(|i| matches!(grip_for(&skin, *i), Grip::Aim { .. }))
        .expect("some bone aims");

    let tip = ed.preview().unwrap().pose().world_tip(&skin.skeleton, bone);
    ed.view.centre_on(tip);
    let from = ed.view.to_screen(tip);
    let to = Point::new(from.x + 60.0, from.y + 60.0);

    ed.tool = Tool::Puppet;
    ed.timeline.playhead_ms = 400.0;
    ed.pointer_down(from, SelectMode::Replace);
    ed.pointer_move(to);
    ed.pointer_up(to);
    assert!(ed.status().contains("keyframe"), "{}", ed.status());

    let before = ed.doc().clips[ed.timeline.clip].tracks.len();
    ed.key(Key::Keyframe);
    let after = ed.doc().clips[ed.timeline.clip].tracks.len();
    let keyed = after > before
        || ed.doc().clips[ed.timeline.clip]
            .tracks
            .iter()
            .any(|t| t.t.iter().any(|k| (k.0 - 400.0).abs() < 20.0));
    assert!(keyed, "puppeting then pressing K must leave a key: {}", ed.status());
    assert!(ed.validation().ok(), "{:?}", ed.validation().problems);
}

#[test]
fn keyframing_without_a_pose_says_so_instead_of_writing_nothing_quietly() {
    let (mut ed, _text) = ready();
    ed.key(Key::Keyframe);
    assert!(
        ed.status().contains("pose") || ed.status().contains("no posed frame"),
        "{}",
        ed.status()
    );
    assert!(!ed.dirty());
}

#[test]
fn a_puppeted_rotation_is_written_to_the_file_in_degrees() {
    isolate();
    let doc = shipped_doc();
    let skin = wisp_rig::Skin::compile(doc.clone()).unwrap();
    let mut preview = wisp_editor::preview::Preview::new(skin.clone());
    preview.seek(0, 0, 0.0);
    let mut puppet = Puppet::new(preview.pose().clone());

    let bone = (0..skin.skeleton.len())
        .find(|i| matches!(grip_for(&skin, *i), Grip::Aim { .. }))
        .unwrap();
    let tip = puppet.pose().world_tip(&skin.skeleton, bone);
    puppet.begin(&skin, bone, tip).unwrap();
    let grip = puppet.drag_to(&skin, tip + Vec2::new(40.0, 40.0)).unwrap();
    assert!(puppet.is_posed());

    let cmd = puppet.keys(&doc, &skin, 0, 500.0, &[grip]).unwrap();
    let mut edited = doc.clone();
    cmd.apply(&mut edited).unwrap();

    let name = skin.skeleton.bone(bone).name.to_string();
    let track = edited.clips[0]
        .tracks
        .iter()
        .find(|t| t.bone == name && t.channel == "rot")
        .expect("a rot track for the bone we posed");
    let key = track
        .t
        .iter()
        .position(|k| (k.0 - 500.0).abs() < 20.0)
        .expect("a key at the playhead");
    let written = track.v[key].0;
    let radians = puppet.pose().offsets[bone].rot;
    assert!(
        (written - wisp_rig::math::rad_to_deg(radians)).abs() < 1e-2,
        "the file must hold degrees: wrote {written}, pose holds {radians} rad"
    );
    wisp_rig::Skin::compile(edited).expect("still a valid skin");
}

// ------------------------------------------------------------- the graph

#[test]
fn a_graph_is_derived_from_the_skins_expressions() {
    isolate();
    let doc = shipped_doc();
    let g = MoodGraph::from_skin(&doc);
    assert_eq!(g.states.len(), doc.expressions.len());
    assert!(g.states.iter().filter(|s| s.initial).count() <= 1);
    // The example F76 gives by name is drawn, not coded.
    assert!(
        g.transitions.iter().any(|t| t.from == "bored"),
        "'bored -> ...' should exist as data: {:?}",
        g.transitions
    );
    let bored = g.states.iter().find(|s| s.name == "bored").expect("a bored state");
    assert_eq!(bored.behaviour, "wander", "bored wanders — that is the plan's own example");
}

#[test]
fn a_graph_round_trips_through_its_own_file() {
    isolate();
    let doc = shipped_doc();
    let g = MoodGraph::from_skin(&doc);
    let text = g.to_toml().unwrap();
    let back = MoodGraph::parse(&text).unwrap();
    assert_eq!(g, back);
    assert_eq!(text, back.to_toml().unwrap(), "byte stable");
    // Data only: no field is an expression, and an unknown one is refused.
    assert!(MoodGraph::parse("format = \"nx-wisp-moods\"\nversion = 1\nscript = \"run()\"").is_err());
}

#[test]
fn the_graph_reports_what_is_wrong_without_refusing_to_open() {
    isolate();
    let doc = shipped_doc();
    let mut g = MoodGraph::from_skin(&doc);
    g.states.push(StateNode {
        name: "ghost".into(),
        clip: "no_such_clip".into(),
        expression: String::new(),
        behaviour: "teleport".into(),
        x: 0.0,
        y: 0.0,
        initial: false,
    });
    let problems = g.problems(&doc);
    assert!(problems.iter().any(|p| p.contains("no_such_clip")), "{problems:?}");
    assert!(problems.iter().any(|p| p.contains("teleport")), "{problems:?}");
    assert!(problems.iter().any(|p| p.contains("nothing leads to")), "{problems:?}");
    assert!(problems.iter().all(|p| !p.contains('!')), "DESIGN.md §9: {problems:?}");
}

#[test]
fn graph_edits_are_reversible_too() {
    let (mut ed, _text) = ready();
    let before = ed.graph.clone();
    ed.apply_graph(graph::add_state(&ed.graph, "wander", "wander", (10.0, 20.0)).unwrap())
        .unwrap();
    ed.apply_graph(graph::connect(&ed.graph, "bored", "wander", "", Some(3000.0)).unwrap())
        .unwrap();
    assert_ne!(before, ed.graph);
    // Deleting the state takes its edge with it.
    let i = ed.graph.state_index("wander").unwrap();
    ed.apply_graph(graph::delete_state(&ed.graph, i).unwrap()).unwrap();
    assert!(ed.graph.state_index("wander").is_none());
    assert!(!ed.graph.transitions.iter().any(|t| t.to == "wander" || t.from == "wander"));
}

#[test]
fn a_transition_to_a_state_that_does_not_exist_is_refused() {
    isolate();
    let g = MoodGraph::from_skin(&shipped_doc());
    assert!(graph::connect(&g, "bored", "nowhere", "", Some(100.0)).is_err());
    assert!(graph::add_state(&g, "bored", "idle", (0.0, 0.0)).is_err(), "duplicate name");
    assert!(graph::set_state_behaviour(&g, 0, "levitate").is_err(), "closed list");
}

#[test]
fn only_one_state_can_be_the_entry_point() {
    isolate();
    let mut g = MoodGraph::from_skin(&shipped_doc());
    graph::set_initial(&g, 3).unwrap().apply(&mut g).unwrap();
    assert_eq!(g.states.iter().filter(|s| s.initial).count(), 1);
    assert!(g.states[3].initial);
}

#[test]
fn edge_ends_stop_at_the_node_boxes() {
    isolate();
    let g = MoodGraph::from_skin(&shipped_doc());
    for t in &g.transitions {
        let (a, b) = graph::edge_ends(&g, t).expect("both ends exist");
        let from = &g.states[g.state_index(&t.from).unwrap()];
        let to = &g.states[g.state_index(&t.to).unwrap()];
        // Each end sits on its own node's box, not at its centre.
        let da = ((a.0 - from.x).abs(), (a.1 - from.y).abs());
        assert!(da.0 > 1.0 || da.1 > 1.0, "the edge starts at the node's centre");
        assert!(da.0 <= graph::NODE_W * 0.5 + 1e-3 && da.1 <= graph::NODE_H * 0.5 + 1e-3);
        let db = ((b.0 - to.x).abs(), (b.1 - to.y).abs());
        assert!(db.0 <= graph::NODE_W * 0.5 + 1e-3 && db.1 <= graph::NODE_H * 0.5 + 1e-3);
    }
}

// ------------------------------------------------------------------- chrome

#[test]
fn the_toolbar_selects_every_tool() {
    let (mut ed, mut text) = ready();
    for tool in Tool::ALL {
        let mut panels = ed.build_panels(BOUNDS, &mut text);
        let node = panels
            .node_for(&Action::SelectTool(tool))
            .unwrap_or_else(|| panic!("the toolbar offers {}", tool.name()));
        let at = panels.ui.node(node).layout.centre();
        assert_eq!(panels.click(at, true), None, "a press is not yet a click");
        assert_eq!(panels.click(at, false), Some(Action::SelectTool(tool)));
        ed.perform(Action::SelectTool(tool));
        assert_eq!(ed.tool, tool);
    }
}

#[test]
fn clicking_a_bone_row_selects_that_bone() {
    let (mut ed, mut text) = ready();
    let mut panels = ed.build_panels(BOUNDS, &mut text);
    let target = Target::Bone(3);
    let node = panels.node_for(&Action::Select(target)).expect("a row for bone 3");
    let at = panels.ui.node(node).layout.centre();
    panels.click(at, true);
    let action = panels.click(at, false).expect("a click");
    ed.perform(action);
    assert!(ed.selection.contains(target));
}

#[test]
fn the_swatch_row_paints_the_selected_shape_with_a_named_colour() {
    let (mut ed, mut text) = ready();
    let shape = ed.doc().shapes.iter().position(|s| s.name == "body").unwrap();
    ed.selection.set(Target::Shape(shape));
    let mut panels = ed.build_panels(BOUNDS, &mut text);
    let node = panels.node_for(&Action::PickSwatch(2)).expect("a cyan swatch");
    let at = panels.ui.node(node).layout.centre();
    panels.click(at, true);
    let action = panels.click(at, false).unwrap();
    ed.perform(action);
    let fill = ed.doc().shapes[shape].fill.as_ref().unwrap();
    assert_eq!(fill.color, "cyan", "the skin already names it, so the file stays readable");
    assert!(ed.status().contains("light inside materials"), "{}", ed.status());
    assert!(ed.validation().ok());
}

#[test]
fn the_lit_edge_button_adds_a_stroke_and_a_gradient_lit_from_the_upper_left() {
    let (mut ed, _text) = ready();
    let shape = ed.doc().shapes.iter().position(|s| s.name == "body").unwrap();
    ed.selection.set(Target::Shape(shape));
    let gradients = ed.doc().gradients.len();
    ed.perform(Action::AddLitEdge);
    assert_eq!(ed.doc().gradients.len(), gradients + 1, "{}", ed.status());
    let stroke = ed.doc().shapes[shape].stroke.as_ref().expect("a stroke");
    assert_eq!(stroke.gradient, "body_edge");
    let g = ed.doc().gradients.iter().find(|g| g.name == "body_edge").unwrap();
    assert_eq!(
        wisp_editor::swatch::light_from_upper_left(g),
        Some(true),
        "DESIGN.md §1: one light source, upper-left"
    );
    assert!(ed.validation().ok(), "{:?}", ed.validation().problems);
    // ...and one undo removes both halves.
    let before = ed.doc().gradients.len();
    ed.undo().unwrap();
    assert_eq!(ed.doc().gradients.len(), before - 1);
    assert!(ed.doc().shapes[shape].stroke.as_ref().map(|s| &s.gradient) != Some(&"body_edge".to_string()));
}

#[test]
fn the_properties_well_describes_whatever_is_selected() {
    let (mut ed, _text) = ready();
    let doc = ed.doc().clone();
    fn mk(ed: &Editor) -> PanelState<'_> {
        PanelState {
            doc: ed.doc(),
            selection: &ed.selection,
            tool: ed.tool,
            timeline: &ed.timeline,
            can_undo: false,
            can_redo: false,
            dirty: false,
            status: "",
            problems: 0,
            show_graph: false,
            graph: None,
        }
    }
    assert_eq!(panels::describe_selection(&mk(&ed)), vec!["nothing selected".to_string()]);

    let shape = doc.shapes.iter().position(|s| s.name == "body").unwrap();
    ed.selection.set(Target::Shape(shape));
    let lines = panels::describe_selection(&mk(&ed));
    assert!(lines.iter().any(|l| l.contains("body")), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("points")), "{lines:?}");

    ed.selection.set(Target::Bone(2));
    let lines = panels::describe_selection(&mk(&ed));
    assert!(lines.iter().any(|l| l.contains(&doc.bones[2].name)), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("parent")), "{lines:?}");
}

#[test]
fn the_timeline_draws_the_clips_keys_and_hands_back_their_boxes() {
    let (ed, mut engine) = ready();
    let mut scene = Scene::new();
    let mut sink = Dry::new(&mut engine);
    let hits = panels::draw_timeline(
        ed.doc(),
        &ed.timeline,
        ed.frames().timeline,
        &mut sink,
        &mut scene,
    );
    assert!(!hits.is_empty(), "the idle clip has keys to click");
    assert!(hits.iter().all(|(_, t)| matches!(t, Target::Key { .. })));
    assert!(sink.drew_containing("idle"), "the clip's name is on screen: {:?}", sink.drawn);
    assert!(!scene.is_empty());
    // Every keyframe box is inside the timeline panel.
    for (r, _) in &hits {
        assert!(r.x >= ed.frames().timeline.x - 8.0);
        assert!(r.bottom() <= ed.frames().timeline.bottom() + 8.0);
    }
}

#[test]
fn clicking_a_keyframe_selects_it_and_moves_the_playhead_there() {
    let (mut ed, mut engine) = ready();
    let mut scene = Scene::new();
    {
        let mut sink = Dry::new(&mut engine);
        ed.draw_canvas(&mut sink, &mut scene);
    }
    // Re-derive the hits the same way the editor did, then click the first.
    let mut scene2 = Scene::new();
    let mut sink = Dry::new(&mut engine);
    let hits = panels::draw_timeline(
        ed.doc(),
        &ed.timeline,
        ed.frames().timeline,
        &mut sink,
        &mut scene2,
    );
    let (rect, target) = hits[3];
    let at = rect.centre();
    ed.pointer_down(at, SelectMode::Replace);
    ed.pointer_up(at);
    assert!(ed.selection.contains(target), "clicking a key selects it");
    if let Target::Key { clip, track, key } = target {
        let expected = ed.doc().clips[clip].tracks[track].t[key].0;
        assert!((ed.timeline.playhead_ms - expected).abs() < 1e-3);
    }
}

#[test]
fn the_graph_view_draws_a_node_per_state_and_labels_its_edges() {
    let (ed, mut engine) = ready();
    let mut scene = Scene::new();
    let mut sink = Dry::new(&mut engine);
    let hits = panels::draw_graph(
        &ed.graph,
        ed.frames().canvas,
        (0.0, 0.0),
        1.0,
        &ed.selection,
        &mut sink,
        &mut scene,
    );
    assert_eq!(hits.len(), ed.graph.states.len());
    for s in &ed.graph.states {
        assert!(sink.drew(&s.name), "state {:?} is not drawn: {:?}", s.name, sink.drawn);
    }
    assert!(sink.drew_containing("after"), "a timed edge is labelled: {:?}", sink.drawn);
}

#[test]
fn the_whole_canvas_builds_with_no_gpu() {
    let (mut ed, mut engine) = ready();
    ed.selection.set(Target::Shape(ed.doc().shapes.iter().position(|s| s.name == "body").unwrap()));
    ed.timeline.onion.enabled = true;
    let mut scene = Scene::new();
    let mut sink = Dry::new(&mut engine);
    ed.draw_canvas(&mut sink, &mut scene);
    assert!(scene.cmds().len() > 100, "{} commands", scene.cmds().len());
    assert_eq!(scene.blur_count(), 0, "the editor is chrome and spends no blur");
}

// ---------------------------------------------------------------------- keys

#[test]
fn the_keyboard_drives_undo_redo_and_the_playhead() {
    let (mut ed, _text) = ready();
    let original = canonical(ed.doc());
    ed.perform(Action::AddShape);
    assert_ne!(original, canonical(ed.doc()));
    ed.key(Key::Undo);
    assert_eq!(original, canonical(ed.doc()));
    ed.key(Key::Redo);
    assert_ne!(original, canonical(ed.doc()));

    ed.timeline.playhead_ms = 0.0;
    ed.key(Key::NextFrame);
    assert!(ed.timeline.playhead_ms > 0.0);
    ed.key(Key::PrevFrame);
    assert!((ed.timeline.playhead_ms).abs() < 1e-3);

    assert!(!ed.timeline.playing);
    ed.key(Key::PlayPause);
    assert!(ed.timeline.playing);
    ed.tick(100.0);
    assert!(ed.timeline.playhead_ms > 0.0);

    assert!(!ed.timeline.onion.enabled);
    ed.key(Key::ToggleOnion);
    assert!(ed.timeline.onion.enabled);
    ed.key(Key::ToggleGraph);
    assert!(ed.show_graph);
}

#[test]
fn escape_abandons_a_gesture_without_leaving_a_half_edit() {
    let (mut ed, _text) = ready();
    ed.tool = Tool::Ik;
    let p = ed.preview().unwrap().pose().world_pos(1);
    ed.view.centre_on(p);
    let at = ed.view.to_screen(p);
    ed.pointer_down(at, SelectMode::Replace);
    ed.pointer_up(at);
    assert!(ed.status().contains("picked"), "{}", ed.status());
    ed.key(Key::Escape);
    assert_eq!(ed.status(), "cancelled");
    // Clicking two more bones must not now complete a chain from stale picks.
    let iks = ed.doc().iks.len();
    let q = ed.preview().unwrap().pose().world_pos(2);
    ed.view.centre_on(q);
    let at = ed.view.to_screen(q);
    ed.pointer_down(at, SelectMode::Replace);
    ed.pointer_up(at);
    assert_eq!(ed.doc().iks.len(), iks);
}

#[test]
fn deleting_the_selection_removes_every_kind_of_thing_in_it() {
    let (mut ed, _text) = ready();
    ed.perform(Action::AddShape);
    let shape = ed.doc().shapes.len() - 1;
    let shapes = ed.doc().shapes.len();
    ed.selection.set(Target::Shape(shape));
    ed.key(Key::Delete);
    assert_eq!(ed.doc().shapes.len(), shapes - 1);
    assert!(ed.selection.is_empty());
    assert!(ed.validation().ok(), "{:?}", ed.validation().problems);
}

#[test]
fn the_selection_never_points_into_a_hole_after_a_delete() {
    let (mut ed, _text) = ready();
    let last = ed.doc().shapes.len() - 1;
    ed.selection.set(Target::Point { shape: last, point: 0 });
    ed.apply(wisp_editor::cmd::Command::RemoveShape { at: last }).unwrap();
    assert!(
        ed.selection.iter().all(|t| match t {
            Target::Point { shape, .. } | Target::Shape(shape) => shape < ed.doc().shapes.len(),
            _ => true,
        }),
        "the selection still names a shape that is gone"
    );
}

#[test]
fn an_edit_that_leaves_the_document_invalid_is_kept_and_reported() {
    let (mut ed, _text) = ready();
    // Point an expression at a clip that does not exist: legal to type, not
    // legal to run.
    let e = ed.doc().expressions[0].clone();
    ed.apply(wisp_editor::cmd::Command::SetExpression {
        at: 0,
        value: wisp_rig::skin::doc::ExpressionDoc { clip: "nope".into(), ..e },
    })
    .expect("the editor holds a half-finished state");
    assert!(!ed.validation().ok(), "it must know the document is broken");
    assert!(ed.status().contains("nope"), "and say so: {}", ed.status());
    assert!(ed.skin().is_some(), "the last good compile is still on screen");
    ed.undo().unwrap();
    assert!(ed.validation().ok());
}

#[test]
fn a_render_of_the_panels_lays_out_inside_the_window() {
    let (mut ed, mut text) = ready();
    let panels = ed.build_panels(BOUNDS, &mut text);
    let root = panels.ui.root().expect("a root");
    assert_eq!(panels.ui.node(root).layout, BOUNDS);
    for (node, action) in panels.actions() {
        let r = panels.ui.node(node).layout;
        assert!(r.w >= 0.0 && r.h >= 0.0, "{action:?} has a negative box: {r:?}");
        assert!(
            r.x >= BOUNDS.x - 1.0 && r.right() <= BOUNDS.right() + 1.0,
            "{action:?} at {r:?} is outside the window"
        );
    }
}
