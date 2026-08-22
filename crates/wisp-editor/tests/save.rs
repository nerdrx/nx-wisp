//! Saving: comments preserved, losses reported, bytes stable.
//!
//! The comment tests are the ones that justify this crate carrying a
//! `toml_edit` dependency. The shipped skin is 2453 lines and 651 of them are
//! comments recording two failed character designs; a save that dropped them
//! would delete the reason the third design exists.

mod support;

use support::{canonical, isolate, shipped_doc, shipped_source};

use wisp_editor::cmd::Command;
use wisp_editor::editor::Editor;
use wisp_editor::save;
use wisp_rig::skin::doc::{ColorDoc, SkinDoc};

fn comment_lines(s: &str) -> usize {
    s.lines().filter(|l| l.trim_start().starts_with('#')).count()
}

#[test]
fn the_shipped_skin_loads_edits_and_re_saves_with_no_validation_errors() {
    isolate();
    let mut doc = shipped_doc();
    wisp_rig::Skin::compile(doc.clone()).expect("it compiles as loaded");

    // A representative edit in every part of the format.
    let bone = doc.bones[1].name.clone();
    let cmds = vec![
        wisp_editor::canvas::move_points(&doc, 0, &[0, 1], wisp_rig::math::Vec2::new(1.0, -1.0))
            .unwrap(),
        wisp_editor::bones::paint_weight(&doc, 1, 0, &bone, 0.5).unwrap(),
        wisp_editor::timeline::set_key(&doc, 0, "root", "rot", 120.0, 4.0, Some("soft")).unwrap(),
        wisp_editor::swatch::add_stop_at(&doc, 0, 0.5).unwrap(),
    ];
    Command::Batch { label: "an editing session", cmds }.apply(&mut doc).unwrap();

    let (text, report) = save::to_toml_preserving(&doc, shipped_source()).expect("it writes");
    assert!(report.had_source);
    let back: SkinDoc = toml::from_str(&text).expect("what we wrote parses");
    wisp_rig::Skin::compile(back).expect("what we wrote is a valid skin");
}

#[test]
fn a_plain_save_loses_the_comments_and_says_so_by_being_plain() {
    isolate();
    let doc = shipped_doc();
    let plain = save::to_toml(&doc).unwrap();
    assert_eq!(
        comment_lines(&plain),
        0,
        "serde has nowhere to keep a comment — this is the behaviour we are working around"
    );
}

#[test]
fn a_preserving_save_keeps_the_comments() {
    isolate();
    let doc = shipped_doc();
    let (text, report) = save::to_toml_preserving(&doc, shipped_source()).unwrap();
    let before = comment_lines(shipped_source());
    let after = comment_lines(&text);
    assert!(before > 600, "the fixture really is comment-heavy: {before}");
    assert!(
        after >= before,
        "an unedited round trip must keep every comment: {before} in, {after} out"
    );
    assert!(report.dropped.is_empty(), "nothing was edited, so nothing may be dropped");
    // `carried` counts comment *blocks*, not lines: 76 blocks carrying 651
    // lines is what the shipped skin looks like.
    assert!(report.carried > 50, "and it should say how many it carried: {}", report.carried);
}

#[test]
fn the_failed_design_notes_survive_verbatim() {
    isolate();
    let doc = shipped_doc();
    let (text, _) = save::to_toml_preserving(&doc, shipped_source()).unwrap();
    // The three lines that are the actual reason this feature preserves
    // comments: they record designs that were tried and rejected.
    for needle in [
        "uncanny",
        "Attempt 1 — a faceted dark crystal",
        "Attempt 2 — a chibi humanoid",
        "Do not re-angularise her",
    ] {
        assert!(text.contains(needle), "the note {needle:?} did not survive the save");
    }
}

#[test]
fn a_comment_follows_the_thing_it_is_about_when_the_order_changes() {
    isolate();
    let src = r#"
format = "nx-wisp-skin"
version = 1

[meta]
name = "T"

[canvas]

# the root of everything
[[bone]]
name = "root"

# this bone drives the tail's second joint
[[bone]]
name = "tail2"
parent = "root"
"#;
    let mut doc: SkinDoc = toml::from_str(src).unwrap();
    // Insert a bone *between* the two, which is exactly the edit that would
    // shift a position-matched comment onto the wrong stanza.
    Command::InsertBone {
        at: 1,
        value: wisp_rig::skin::doc::BoneDoc {
            name: "tail1".into(),
            parent: "root".into(),
            ..Default::default()
        },
    }
    .apply(&mut doc)
    .unwrap();

    let (text, report) = save::to_toml_preserving(&doc, src).unwrap();
    assert!(report.dropped.is_empty(), "nothing was deleted: {:?}", report.dropped);
    let tail2_at = text.find("name = \"tail2\"").expect("tail2 is in the output");
    let note_at = text.find("drives the tail's second joint").expect("the note survived");
    let tail1_at = text.find("name = \"tail1\"").expect("tail1 is in the output");
    assert!(note_at < tail2_at, "the note must sit above tail2");
    assert!(tail1_at < note_at, "and below the bone that was inserted before it");
}

#[test]
fn deleting_the_thing_a_comment_describes_is_reported_by_name() {
    isolate();
    let src = r#"
format = "nx-wisp-skin"
version = 1

[meta]
name = "T"

[canvas]

[[bone]]
name = "root"

# tail3 exists because the two-joint tail read as a stick
[[bone]]
name = "tail3"
parent = "root"
"#;
    let mut doc: SkinDoc = toml::from_str(src).unwrap();
    Command::RemoveBone { at: 1 }.apply(&mut doc).unwrap();

    let (text, report) = save::to_toml_preserving(&doc, src).unwrap();
    assert!(!text.contains("read as a stick"), "the note went with its bone");
    assert_eq!(report.dropped, vec!["bone \"tail3\"".to_string()]);
    let warning = report.warning().expect("a warning");
    assert!(warning.contains("tail3"), "{warning}");
    assert!(!warning.contains('!'), "DESIGN.md §9: {warning}");
    assert!(report.lost_anything());
}

#[test]
fn save_load_save_is_byte_stable_on_the_shipped_skin() {
    isolate();
    let doc = shipped_doc();
    let (once, _) = save::to_toml_preserving(&doc, shipped_source()).unwrap();
    let reloaded: SkinDoc = toml::from_str(&once).unwrap();
    let (twice, _) = save::to_toml_preserving(&reloaded, &once).unwrap();
    assert_eq!(once, twice, "the second save must be byte-identical to the first");
    // ...and a third, because an off-by-one in the decor merge could alternate.
    let third_doc: SkinDoc = toml::from_str(&twice).unwrap();
    let (thrice, _) = save::to_toml_preserving(&third_doc, &twice).unwrap();
    assert_eq!(twice, thrice);
}

#[test]
fn save_load_save_is_byte_stable_after_an_edit() {
    isolate();
    let mut doc = shipped_doc();
    Command::InsertColor {
        at: 0,
        value: ColorDoc { name: "probe".into(), value: "#123456".into() },
    }
    .apply(&mut doc)
    .unwrap();
    let (once, _) = save::to_toml_preserving(&doc, shipped_source()).unwrap();
    let reloaded: SkinDoc = toml::from_str(&once).unwrap();
    let (twice, _) = save::to_toml_preserving(&reloaded, &once).unwrap();
    assert_eq!(once, twice);
}

#[test]
fn a_plain_save_is_byte_stable_too() {
    isolate();
    let doc = shipped_doc();
    let once = save::to_toml(&doc).unwrap();
    let reloaded: SkinDoc = toml::from_str(&once).unwrap();
    assert_eq!(once, save::to_toml(&reloaded).unwrap());
}

#[test]
fn writing_to_disk_round_trips_through_the_ordinary_parser() {
    isolate();
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("probe.skin.toml");
    let doc = shipped_doc();
    let report = save::write(&path, &doc, Some(shipped_source())).unwrap();
    assert!(report.carried > 0);

    let (back, text) = save::read(&path).unwrap();
    assert_eq!(canonical(&doc), canonical(&back), "what came back is what went in");
    assert!(comment_lines(&text) > 600);
    // The skin the editor wrote is loadable by the ordinary runtime path —
    // F49's "the editor gets no privileged path", asserted rather than
    // asserted-in-a-comment.
    wisp_rig::Skin::load(&path).expect("the runtime loads what the editor wrote");
    // No leftover part file.
    assert!(!path.with_extension("toml.part").exists());
}

#[test]
fn the_editor_reports_what_a_save_would_cost_before_it_happens() {
    isolate();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wisp.skin.toml");
    std::fs::write(&path, shipped_source()).unwrap();

    let mut ed = Editor::open(&path).expect("it opens");
    // Nothing lost yet.
    assert!(ed.save_preview().unwrap().dropped.is_empty());

    // Delete a shape that carries a comment, and ask again *before* saving.
    let commented = ed
        .doc()
        .shapes
        .iter()
        .position(|s| s.name == "halo")
        .expect("the shipped skin has a shape called halo");
    ed.apply(Command::RemoveShape { at: commented }).unwrap();
    let preview = ed.save_preview().unwrap();
    assert!(preview.lost_anything(), "removing a documented shape costs its comment");
    assert!(preview.warning().unwrap().contains("halo"));

    // Undo, and the cost goes away again — the warning is a live fact, not a
    // sticky flag.
    ed.undo().unwrap();
    assert!(ed.save_preview().unwrap().dropped.is_empty());
}

#[test]
fn the_editor_saves_and_then_reads_back_the_same_bytes() {
    isolate();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wisp.skin.toml");
    std::fs::write(&path, shipped_source()).unwrap();

    let mut ed = Editor::open(&path).unwrap();
    assert!(!ed.dirty());
    ed.apply(Command::InsertColor {
        at: 0,
        value: ColorDoc { name: "probe".into(), value: "#7700ff".into() },
    })
    .unwrap();
    assert!(ed.dirty());
    ed.save().unwrap();
    assert!(!ed.dirty());
    let first = std::fs::read_to_string(&path).unwrap();

    // Saving again with no further edits must not change a byte.
    ed.save().unwrap();
    assert_eq!(first, std::fs::read_to_string(&path).unwrap());

    // ...and reopening gives the same document.
    let ed2 = Editor::open(&path).unwrap();
    assert_eq!(canonical(ed.doc()), canonical(ed2.doc()));
}

#[test]
fn the_mood_graph_is_written_beside_the_skin() {
    isolate();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wisp.skin.toml");
    std::fs::write(&path, shipped_source()).unwrap();

    let mut ed = Editor::open(&path).unwrap();
    ed.save().unwrap();
    let graph_path = wisp_editor::graph::MoodGraph::path_for(&path);
    assert_eq!(graph_path.file_name().unwrap(), "wisp.moods.toml");
    let text = std::fs::read_to_string(&graph_path).expect("the graph was written");
    let g = wisp_editor::graph::MoodGraph::parse(&text).expect("it parses");
    assert_eq!(g.states.len(), ed.doc().expressions.len());

    // Reopening picks it back up.
    let ed2 = Editor::open(&path).unwrap();
    assert_eq!(ed2.graph.states.len(), g.states.len());
}

#[test]
fn a_skin_created_from_nothing_has_no_comments_to_lose() {
    isolate();
    let ed = Editor::blank("Test");
    let report = ed.save_preview().unwrap();
    assert!(!report.had_source);
    assert!(report.warning().is_none());
}

#[test]
fn an_unsaved_skin_refuses_to_save_without_a_path() {
    isolate();
    let mut ed = Editor::blank("Test");
    let err = ed.save().expect_err("there is nowhere to write it");
    assert!(err.to_string().contains("choose a file"), "{err}");
}

#[test]
fn every_shipped_clip_and_expression_survives_a_round_trip() {
    isolate();
    let doc = shipped_doc();
    assert_eq!(doc.clips.len(), 16, "F76 says all sixteen clips are editable");
    assert_eq!(doc.expressions.len(), 8, "and all eight expressions (F74)");

    let (text, _) = save::to_toml_preserving(&doc, shipped_source()).unwrap();
    let back: SkinDoc = toml::from_str(&text).unwrap();
    assert_eq!(back.clips.len(), 16);
    assert_eq!(back.expressions.len(), 8);
    for (a, b) in doc.clips.iter().zip(&back.clips) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.tracks.len(), b.tracks.len(), "clip {:?} lost a track", a.name);
        for (ta, tb) in a.tracks.iter().zip(&b.tracks) {
            assert_eq!(ta.t, tb.t, "clip {:?} track {:?} lost keys", a.name, ta.channel);
            assert_eq!(ta.v, tb.v);
            assert_eq!(ta.ease, tb.ease);
        }
    }
}
