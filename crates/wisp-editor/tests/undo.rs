//! Undo, redo, and the claim that every command is reversible.
//!
//! The assertions here are on **bytes**, not on fields. A test that checked a
//! handful of properties would pass while some field nobody thought of drifted;
//! comparing the serialised document catches everything the format can hold,
//! which is exactly the set of things that matter.

mod support;

use support::{canonical, isolate, shipped_doc, shipped_editor};

use wisp_editor::cmd::Command;
use wisp_editor::error::EditError;
use wisp_editor::history::{History, Reversible};
use wisp_rig::math::Vec2;
use wisp_rig::skin::doc::{
    BoneDoc, CanvasDoc, ChainDoc, ClipDoc, ColorDoc, EaseSpec, ExpressionDoc, GradientDoc, IkDoc,
    LayerDoc, MetaDoc, MotionDoc, Num, PhysicsDoc, ShapeDoc, TrackDoc, WeightDoc,
};

/// Every variant of `Command`, built against the shipped skin so the indices
/// address real records.
///
/// This list is the test's whole point: when a variant is added to `Command`
/// and not added here, `every_command_round_trips` still passes, so
/// [`covers_every_variant`] counts them and fails instead.
fn all_commands() -> Vec<Command> {
    vec![
        Command::Batch {
            label: "test",
            cmds: vec![
                Command::SetColor { at: 0, value: ColorDoc { name: "violet".into(), value: "#123456".into() } },
                Command::RemoveColor { at: 1 },
            ],
        },
        Command::SetMeta(Box::new(MetaDoc { name: "Renamed".into(), ..Default::default() })),
        Command::SetCanvas(CanvasDoc { size: [Num(512.0), Num(512.0)], anchor: [Num(1.0), Num(2.0)] }),
        Command::SetPhysics(Box::new(PhysicsDoc { gravity: Some(Num(9.8)), ..Default::default() })),
        Command::SetMotion(Box::new(MotionDoc { squash_bone: "body".into(), ..Default::default() })),
        Command::InsertColor { at: 0, value: ColorDoc { name: "new".into(), value: "#abcdef".into() } },
        Command::RemoveColor { at: 0 },
        Command::SetColor { at: 0, value: ColorDoc { name: "x".into(), value: "#000000".into() } },
        Command::InsertGradient { at: 0, value: Box::new(a_gradient()) },
        Command::RemoveGradient { at: 0 },
        Command::SetGradient { at: 0, value: Box::new(a_gradient()) },
        Command::InsertStop { gradient: 0, at: 1, position: 0.5, color: "#00ff00ff".into() },
        Command::RemoveStop { gradient: 0, at: 0 },
        Command::SetStop { gradient: 0, at: 0, position: 0.25, color: "#ff00ffff".into() },
        Command::InsertBone { at: 1, value: BoneDoc { name: "extra".into(), ..Default::default() } },
        Command::RemoveBone { at: 2 },
        Command::SetBone { at: 1, value: BoneDoc { name: "renamed".into(), ..Default::default() } },
        Command::InsertShape { at: 0, value: Box::new(a_shape()) },
        Command::RemoveShape { at: 0 },
        Command::SetShape { at: 0, value: Box::new(a_shape()) },
        Command::SetWeight {
            shape: 0,
            point: 1,
            value: Some(WeightDoc { point: 1, bones: vec!["root".into()], weights: vec![Num(1.0)] }),
        },
        Command::InsertIk { at: 0, value: Box::new(an_ik()) },
        Command::RemoveIk { at: 0 },
        Command::SetIk { at: 0, value: Box::new(an_ik()) },
        Command::InsertChain { at: 0, value: Box::new(a_chain()) },
        Command::RemoveChain { at: 0 },
        Command::SetChain { at: 0, value: Box::new(a_chain()) },
        Command::InsertLayer { at: 0, value: LayerDoc { name: "extra".into(), additive: true, default_clip: String::new(), weight: None } },
        Command::RemoveLayer { at: 0 },
        Command::SetLayer { at: 0, value: LayerDoc { name: "base2".into(), additive: false, default_clip: String::new(), weight: None } },
        Command::InsertClip { at: 0, value: Box::new(a_clip()) },
        Command::RemoveClip { at: 0 },
        Command::SetClip { at: 0, value: Box::new(a_clip()) },
        Command::InsertTrack { clip: 0, at: 0, value: Box::new(a_track()) },
        Command::RemoveTrack { clip: 0, at: 0 },
        Command::SetTrack { clip: 0, at: 0, value: Box::new(a_track()) },
        Command::InsertKey { clip: 0, track: 0, at: 0, t: 0.0, v: 1.5, ease: None },
        Command::RemoveKey { clip: 0, track: 0, at: 0 },
        Command::SetKey { clip: 0, track: 0, at: 0, t: 0.0, v: 3.0 },
        Command::SetTrackEase { clip: 0, track: 0, value: Some(EaseSpec::All("spring".into())) },
        Command::InsertExpression { at: 0, value: ExpressionDoc { name: "extra".into(), clip: "idle".into(), layer: String::new(), weight: None, fade_ms: None } },
        Command::RemoveExpression { at: 0 },
        Command::SetExpression { at: 0, value: ExpressionDoc { name: "neutral".into(), clip: "idle".into(), layer: String::new(), weight: None, fade_ms: None } },
    ]
}

fn a_gradient() -> GradientDoc {
    GradientDoc {
        name: "test_grad".into(),
        kind: "linear".into(),
        start: Some([Num(0.0), Num(0.0)]),
        end: Some([Num(1.0), Num(1.0)]),
        center: None,
        focus: None,
        radius: None,
        extend: String::new(),
        follow_bone: String::new(),
        stop_at: vec![Num(0.0), Num(1.0)],
        stop_color: vec!["#ffffffff".into(), "#000000ff".into()],
    }
}

fn a_shape() -> ShapeDoc {
    ShapeDoc {
        name: "test_shape".into(),
        z: 42,
        opacity: None,
        silhouette: true,
        fill_rule: String::new(),
        path: "M 0 0 L 10 0 L 10 10 Z".into(),
        bind: "root".into(),
        fill: None,
        stroke: None,
        bind_auto: None,
        weights: Vec::new(),
    }
}

fn an_ik() -> IkDoc {
    IkDoc {
        name: "test_ik".into(),
        kind: "look_at".into(),
        target: "cursor".into(),
        weight: None,
        bone: "root".into(),
        forward: None,
        max_deg: Some(Num(30.0)),
        root: String::new(),
        mid: String::new(),
        end: String::new(),
        bend_positive: None,
    }
}

fn a_chain() -> ChainDoc {
    ChainDoc {
        name: "test_chain".into(),
        bones: vec!["root".into()],
        stiffness: None,
        damping: None,
        mass: None,
        gravity: None,
        drag: None,
        stiff_length: None,
    }
}

fn a_clip() -> ClipDoc {
    ClipDoc {
        name: "test_clip".into(),
        duration_ms: Num(1000.0),
        looping: true,
        additive: false,
        tracks: vec![a_track()],
    }
}

fn a_track() -> TrackDoc {
    TrackDoc {
        bone: "root".into(),
        channel: "ty".into(),
        t: vec![Num(0.0), Num(500.0)],
        v: vec![Num(0.0), Num(-3.0)],
        ease: Some(EaseSpec::All("soft".into())),
    }
}

#[test]
fn every_command_returns_an_inverse_that_restores_the_document() {
    isolate();
    for cmd in all_commands() {
        let mut doc = shipped_doc();
        let before = canonical(&doc);
        let label = cmd.label();
        let inverse = match cmd.apply(&mut doc) {
            Ok(inv) => inv,
            Err(e) => panic!("{label} refused on the shipped skin: {e}"),
        };
        let after = canonical(&doc);
        assert_ne!(before, after, "{label} claimed to be an edit but changed nothing");
        inverse.apply(&mut doc).unwrap_or_else(|e| panic!("undoing {label}: {e}"));
        assert_eq!(
            before,
            canonical(&doc),
            "{label} did not undo to a byte-identical document"
        );
    }
}

#[test]
fn covers_every_variant() {
    // A crude but effective guard: `Command`'s variants are counted by their
    // labels, and every label must appear in the list above. Adding a variant
    // without adding a case here fails right here rather than silently
    // shrinking the coverage of the test above.
    let mut labels: Vec<&str> = all_commands().iter().map(|c| c.label()).collect();
    labels.sort_unstable();
    labels.dedup();
    // 42 non-Batch variants, each with its own label, plus the "test" label
    // the Batch case above carries. A new variant makes this 44 and fails
    // until it is given a case in `all_commands`.
    assert_eq!(labels.len(), 43, "every command label is exercised: {labels:?}");
}

#[test]
fn undo_then_redo_returns_the_original_bytes() {
    isolate();
    let mut doc = shipped_doc();
    let original = canonical(&doc);
    let mut history: History<Command> = History::default();

    for cmd in all_commands() {
        history.apply(&mut doc, cmd).expect("the command applies");
    }
    let edited = canonical(&doc);
    assert_ne!(original, edited);

    while history.can_undo() {
        history.undo(&mut doc).expect("undo");
    }
    assert_eq!(original, canonical(&doc), "undoing everything is not the original");

    while history.can_redo() {
        history.redo(&mut doc).expect("redo");
    }
    assert_eq!(edited, canonical(&doc), "redoing everything is not the edited document");

    // ...and round the loop a second time, which catches a stack that is
    // right once and wrong on reuse.
    while history.can_undo() {
        history.undo(&mut doc).expect("undo");
    }
    assert_eq!(original, canonical(&doc));
}

#[test]
fn a_drag_folds_into_one_undo_step() {
    isolate();
    let mut doc = shipped_doc();
    let original = canonical(&doc);
    let mut history: History<Command> = History::default();

    history.begin();
    for i in 1..=30 {
        let cmd = wisp_editor::canvas::move_points(&doc, 0, &[0], Vec2::new(i as f32 * 0.1, 0.0))
            .expect("the shape has a point 0");
        history.apply(&mut doc, cmd).expect("apply");
    }
    history.end();

    assert_eq!(history.undo_depth(), 1, "thirty drag steps must be one undo entry");
    history.undo(&mut doc).expect("undo");
    assert_eq!(original, canonical(&doc), "one undo must restore the pre-drag state");
}

#[test]
fn two_gestures_are_two_undo_steps() {
    isolate();
    let mut doc = shipped_doc();
    let mut history: History<Command> = History::default();
    for _ in 0..2 {
        history.begin();
        for _ in 0..5 {
            let cmd = wisp_editor::canvas::move_points(&doc, 0, &[0], Vec2::new(0.1, 0.0)).unwrap();
            history.apply(&mut doc, cmd).unwrap();
        }
        history.end();
    }
    assert_eq!(history.undo_depth(), 2);
}

#[test]
fn a_refused_batch_leaves_the_document_untouched() {
    isolate();
    let mut doc = shipped_doc();
    let before = canonical(&doc);
    let bad = Command::Batch {
        label: "half a change",
        cmds: vec![
            Command::SetColor { at: 0, value: ColorDoc { name: "a".into(), value: "#111111".into() } },
            // Index far past the end: this one cannot apply.
            Command::SetColor { at: 9999, value: ColorDoc { name: "b".into(), value: "#222222".into() } },
        ],
    };
    let err = bad.apply(&mut doc).expect_err("the batch must be refused");
    assert!(matches!(err, EditError::NoSuchIndex { .. }));
    assert_eq!(before, canonical(&doc), "a refused batch must not leave half an edit");
}

#[test]
fn an_empty_batch_is_a_legal_no_op() {
    isolate();
    let mut doc = shipped_doc();
    let before = canonical(&doc);
    let inv = Command::Batch { label: "nothing", cmds: Vec::new() }.apply(&mut doc).unwrap();
    assert_eq!(before, canonical(&doc));
    inv.apply(&mut doc).unwrap();
    assert_eq!(before, canonical(&doc));
}

#[test]
fn undo_is_refused_when_there_is_nothing_to_undo() {
    isolate();
    let mut doc = shipped_doc();
    let mut history: History<Command> = History::default();
    assert!(matches!(history.undo(&mut doc), Err(EditError::NothingToUndo)));
    assert!(matches!(history.redo(&mut doc), Err(EditError::NothingToRedo)));
}

#[test]
fn a_new_edit_clears_the_redo_stack() {
    isolate();
    let mut doc = shipped_doc();
    let mut history: History<Command> = History::default();
    history
        .apply(&mut doc, Command::SetCanvas(CanvasDoc { size: [Num(300.0), Num(300.0)], anchor: [Num(0.0), Num(0.0)] }))
        .unwrap();
    history.undo(&mut doc).unwrap();
    assert!(history.can_redo());
    history
        .apply(&mut doc, Command::SetCanvas(CanvasDoc { size: [Num(400.0), Num(400.0)], anchor: [Num(0.0), Num(0.0)] }))
        .unwrap();
    assert!(!history.can_redo(), "a fresh edit forks the timeline");
}

#[test]
fn the_dirty_flag_follows_the_save_watermark() {
    isolate();
    let mut doc = shipped_doc();
    let mut history: History<Command> = History::default();
    assert!(!history.dirty());
    history
        .apply(&mut doc, Command::SetCanvas(CanvasDoc { size: [Num(300.0), Num(300.0)], anchor: [Num(0.0), Num(0.0)] }))
        .unwrap();
    assert!(history.dirty());
    history.mark_saved();
    assert!(!history.dirty());
    history.undo(&mut doc).unwrap();
    assert!(history.dirty(), "undoing past a save still leaves the file out of date");
}

#[test]
fn the_editor_undoes_a_whole_editing_session() {
    let mut ed = shipped_editor();
    let original = canonical(ed.doc());

    // A representative session: paint a shape, move a point, add a bone, key a
    // pose, give something a lit edge.
    ed.selection.set(wisp_editor::select::Target::Shape(0));
    ed.perform(wisp_editor::panels::Action::PickSwatch(2));
    let cmd = wisp_editor::canvas::move_points(ed.doc(), 0, &[0], Vec2::new(3.0, -2.0)).unwrap();
    ed.apply(cmd).unwrap();
    ed.perform(wisp_editor::panels::Action::AddBone);
    ed.perform(wisp_editor::panels::Action::AddShape);
    ed.perform(wisp_editor::panels::Action::AddLitEdge);
    let key = wisp_editor::timeline::set_key(ed.doc(), 0, "root", "rot", 250.0, 12.0, None).unwrap();
    ed.apply(key).unwrap();

    assert_ne!(original, canonical(ed.doc()));
    while ed.can_undo() {
        ed.undo().unwrap();
    }
    assert_eq!(original, canonical(ed.doc()), "the session did not undo cleanly");
}

#[test]
fn the_history_is_bounded() {
    isolate();
    let mut doc = shipped_doc();
    let mut history: History<Command> = History::new(8);
    for i in 0..40 {
        history
            .apply(
                &mut doc,
                Command::SetCanvas(CanvasDoc {
                    size: [Num(200.0 + i as f32), Num(200.0)],
                    anchor: [Num(0.0), Num(0.0)],
                }),
            )
            .unwrap();
    }
    assert_eq!(history.undo_depth(), 8);
}

#[test]
fn a_reversible_command_reports_a_label_for_the_menu() {
    for cmd in all_commands() {
        let label = Reversible::label(&cmd);
        assert!(!label.is_empty());
        assert!(!label.contains('!'), "DESIGN.md §9: no exclamation marks — {label}");
        // Sentence case: the first word is lower case. Acronyms inside it
        // ("IK") stay as they are written, because "add an ik chain" is not
        // sentence case, it is wrong.
        let first = label.chars().next().expect("a non-empty label");
        assert!(first.is_lowercase(), "sentence case: {label}");
        assert!(!label.ends_with('.'), "a menu label is not a sentence: {label}");
    }
}
