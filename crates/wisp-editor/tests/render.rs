//! Offscreen render tests. Every one draws into a texture and asserts on
//! read-back pixels — SPEC §4, and the pattern `wisp-paint`'s own suite proved.
//! **Nothing here opens a window**: the operator is using this machine.
//!
//! Set `WISP_PAINT_DUMP=/some/dir` to have every case write a PNG as well,
//! which is how DESIGN.md §11's "review by looking, not by reading code" gets
//! done for the editor's chrome.
//!
//! These are the only tests in this crate that need a GPU. Everything else —
//! the state machine, every tool, the graph, the save path — runs headless.

mod support;

use support::{isolate, shipped_editor};

use wisp_editor::editor::Editor;
use wisp_editor::panels::Action;
use wisp_editor::select::{Target, Tool};
use wisp_editor::text::Live;
use wisp_paint::adapter::AdapterPreference;
use wisp_paint::geom::Rect;
use wisp_paint::scene::Scene;
use wisp_paint::text::TextEngine;
use wisp_paint::{Image, Painter};
use wisp_theme::Color;

/// One painter for the whole file: bringing up Vulkan costs more than every
/// case in here put together.
fn painter() -> std::sync::MutexGuard<'static, Painter> {
    use std::sync::{Mutex, OnceLock};
    static P: OnceLock<Mutex<Painter>> = OnceLock::new();
    P.get_or_init(|| {
        isolate();
        Mutex::new(
            Painter::new(AdapterPreference::HighPerformance)
                .expect("a Vulkan adapter — this tree is Vulkan-only by SPEC §1"),
        )
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

fn dump(img: &Image, name: &str) {
    let Some(dir) = std::env::var_os("WISP_PAINT_DUMP") else { return };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).ok();
    let file = std::fs::File::create(dir.join(format!("{name}.png"))).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), img.w, img.h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    e.write_header().unwrap().write_image_data(&img.data).unwrap();
}

/// Render the whole editor — chrome and canvas — into one texture.
fn shoot(ed: &mut Editor, w: u32, h: u32, name: &str) -> Image {
    let mut p = painter();
    let bounds = Rect::from_size(w as f32, h as f32);
    let mut engine = TextEngine::new();
    // Build once so the panel geometry exists, frame the canvas against it,
    // then build for real. `fit` needs to know where the canvas is.
    let _ = ed.build_panels(bounds, &mut engine);
    ed.fit();
    let panels = ed.build_panels(bounds, &mut engine);
    ed.repose();

    let mut scene = Scene::new();
    panels.ui.paint(&p, &mut engine, &mut scene);
    {
        let mut sink = Live::new(&p, &mut engine);
        ed.draw_canvas(&mut sink, &mut scene);
    }

    let target = p.offscreen(w, h).unwrap();
    p.render(&target, &scene).unwrap();
    let img = p.read(&target).unwrap();
    dump(&img, name);
    img
}

fn opaque_fraction(img: &Image) -> f32 {
    let total = (img.w * img.h) as f32;
    let mut n = 0u32;
    for y in 0..img.h {
        for x in 0..img.w {
            if img.alpha(x, y) > 250 {
                n += 1;
            }
        }
    }
    n as f32 / total
}

/// How many distinct-ish colours appear. A panel that failed to draw is one
/// flat colour; a panel that drew is not.
fn variety(img: &Image, rect: Rect) -> usize {
    let mut seen = std::collections::HashSet::new();
    let x0 = rect.x.max(0.0) as u32;
    let y0 = rect.y.max(0.0) as u32;
    let x1 = (rect.right() as u32).min(img.w);
    let y1 = (rect.bottom() as u32).min(img.h);
    for y in y0..y1 {
        for x in x0..x1 {
            let [r, g, b, _] = img.pixel(x, y);
            seen.insert((r / 8, g / 8, b / 8));
        }
    }
    seen.len()
}

fn brightest(img: &Image, rect: Rect) -> Color {
    let mut best = Color::rgba(0, 0, 0, 0);
    let x1 = (rect.right() as u32).min(img.w);
    let y1 = (rect.bottom() as u32).min(img.h);
    for y in rect.y.max(0.0) as u32..y1 {
        for x in rect.x.max(0.0) as u32..x1 {
            let c = img.color(x, y);
            if c.luminance() > best.luminance() {
                best = c;
            }
        }
    }
    best
}

// ---------------------------------------------------------------- the editor

#[test]
fn the_whole_editor_renders() {
    let mut ed = shipped_editor();
    ed.fit();
    let shape = ed.doc().shapes.iter().position(|s| s.name == "body").unwrap();
    ed.selection.set(Target::Shape(shape));
    let img = shoot(&mut ed, 1440, 900, "editor_full");

    // Nothing is see-through: this is chrome, and DESIGN.md v1.5 §4 makes
    // structural surfaces opaque.
    assert!(
        opaque_fraction(&img) > 0.99,
        "the editor must be fully opaque, got {}",
        opaque_fraction(&img)
    );
    let f = ed.frames();
    assert!(variety(&img, f.toolbar) > 8, "the toolbar drew nothing");
    assert!(variety(&img, f.left) > 8, "the bone and shape lists drew nothing");
    assert!(variety(&img, f.right) > 8, "the palette drew nothing");
    assert!(variety(&img, f.canvas) > 40, "she is not on the canvas");
    assert!(variety(&img, f.timeline) > 8, "the timeline drew nothing");
    assert!(variety(&img, f.status) > 4, "the status strip drew nothing");
}

#[test]
fn the_canvas_shows_her_with_handles_and_bone_gizmos() {
    let mut ed = shipped_editor();
    ed.fit();
    let shape = ed.doc().shapes.iter().position(|s| s.name == "body").unwrap();
    ed.selection.set(Target::Shape(shape));
    ed.tool = Tool::Select;
    let img = shoot(&mut ed, 1000, 760, "editor_canvas");

    let f = ed.frames();
    // Cyan appears: the bone gizmos and the control handles are cyan, and
    // nothing else on the canvas is.
    let mut cyan = 0u32;
    let x1 = (f.canvas.right() as u32).min(img.w);
    let y1 = (f.canvas.bottom() as u32).min(img.h);
    for y in f.canvas.y as u32..y1 {
        for x in f.canvas.x as u32..x1 {
            let c = img.color(x, y);
            if c.b > 180 && c.g > 140 && c.r < 140 {
                cyan += 1;
            }
        }
    }
    assert!(cyan > 200, "the bone gizmos should be visible: {cyan} cyan pixels");
    // And she is genuinely lit — DESIGN.md's rim is the brightest thing on the
    // canvas.
    assert!(brightest(&img, f.canvas).luminance() > 0.6);
}

#[test]
fn the_timeline_draws_keyframes_and_a_playhead() {
    let mut ed = shipped_editor();
    ed.fit();
    ed.timeline.playhead_ms = 900.0;
    let img = shoot(&mut ed, 1280, 800, "editor_timeline");
    let t = ed.frames().timeline;

    let mut amber = 0u32;
    let mut cyan = 0u32;
    let x1 = (t.right() as u32).min(img.w);
    let y1 = (t.bottom() as u32).min(img.h);
    for y in t.y as u32..y1 {
        for x in t.x as u32..x1 {
            let c = img.color(x, y);
            // The playhead is a 1px hairline resolved out of a 2x
            // supersampled buffer, so it arrives as amber blended towards the
            // well beneath it: test the *hue* rather than the exact token.
            if c.r > 110 && c.r > c.g + 25 && c.g > c.b + 25 {
                amber += 1;
            }
            if c.b > 180 && c.g > 150 && c.r < 120 {
                cyan += 1;
            }
        }
    }
    assert!(amber > 20, "the playhead is amber and should be drawn: {amber}");
    assert!(cyan > 40, "keyframes are cyan boxes: {cyan}");
}

#[test]
fn the_state_machine_view_renders_its_graph() {
    let mut ed = shipped_editor();
    ed.fit();
    ed.perform(Action::ToggleGraph);
    ed.selection.set(Target::State(0));
    let img = shoot(&mut ed, 1280, 800, "editor_graph");
    let c = ed.frames().canvas;
    assert!(variety(&img, c) > 12, "the graph drew nothing");
    assert!(opaque_fraction(&img) > 0.99);
}

#[test]
fn onion_skin_ghosts_her_previous_frames() {
    let mut ed = shipped_editor();
    ed.fit();
    ed.timeline.onion.enabled = true;
    ed.timeline.onion.before = 3;
    ed.timeline.onion.after = 2;
    ed.timeline.onion.spacing_ms = 220.0;
    // `walk` moves her far enough for a ghost to be worth drawing; `idle`
    // barely moves, which is exactly why it is the wrong clip for this check.
    ed.timeline.clip = ed.doc().clips.iter().position(|c| c.name == "walk").unwrap();
    ed.timeline.playhead_ms = 500.0;
    let with = shoot(&mut ed, 1000, 760, "editor_onion");

    ed.timeline.onion.enabled = false;
    let without = shoot(&mut ed, 1000, 760, "editor_no_onion");
    assert!(
        with.mean_abs_diff(&without) > 0.05,
        "ghosts should change the picture: {}",
        with.mean_abs_diff(&without)
    );
}

#[test]
fn the_weight_overlay_shows_which_points_a_bone_owns() {
    let mut ed = shipped_editor();
    ed.fit();
    let shape = ed.doc().shapes.iter().position(|s| s.name == "body").unwrap();
    ed.selection.set(Target::Shape(shape));
    ed.selection.apply(Target::Bone(2), wisp_editor::select::SelectMode::Add);
    ed.tool = Tool::Weight;
    let img = shoot(&mut ed, 1000, 760, "editor_weights");
    assert!(variety(&img, ed.frames().canvas) > 40);
}

// ------------------------------------------------------------------- the rig

#[test]
fn she_still_reads_at_96_px() {
    // F73 makes this the acceptance test, not 512px. The editor renders her
    // through exactly the path the shell does, so if this passes here it
    // passes on the desktop.
    let mut p = painter();
    let skin = wisp_rig::Skin::compile(support::shipped_doc()).unwrap();
    let mut preview = wisp_editor::preview::Preview::at_size(
        skin,
        96.0,
        wisp_rig::math::Vec2::new(64.0, 78.0),
    );
    preview.seek(0, 0, 0.0);

    let mut view = wisp_editor::view::Viewport::new(Rect::from_size(128.0, 128.0));
    view.zoom = 1.0;
    view.origin = wisp_rig::math::Vec2::ZERO;

    let mut scene = Scene::new();
    scene.fill_rect(
        Rect::from_size(128.0, 128.0),
        wisp_theme::Radius::XS,
        wisp_paint::paint::Paint::Solid(wisp_theme::palette::BG_TOP),
    );
    wisp_editor::overlay::draw_frame(preview.frame(), &view, 1.0, &mut scene);

    let target = p.offscreen(128, 128).unwrap();
    p.render(&target, &scene).unwrap();
    let img = p.read(&target).unwrap();
    dump(&img, "wisp_at_96px");

    // She occupies a real part of the frame and is not a smudge.
    let mut lit = 0u32;
    for y in 0..img.h {
        for x in 0..img.w {
            if img.color(x, y).luminance() > 0.10 {
                lit += 1;
            }
        }
    }
    let frac = lit as f32 / (img.w * img.h) as f32;
    assert!(frac > 0.08, "she covers only {frac:.3} of a 128px frame");
    assert!(brightest(&img, Rect::from_size(128.0, 128.0)).luminance() > 0.5, "no rim light");
}

#[test]
fn the_editor_draws_her_from_the_same_document_it_saves() {
    // The strongest form of "no privileged path": render, save, reload the
    // saved bytes through the ordinary parser, render again, compare pixels.
    let mut p = painter();
    isolate();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("probe.skin.toml");
    std::fs::write(&path, wisp_rig::skin::WISP_SKIN_TOML).unwrap();

    let render = |p: &mut Painter, skin: wisp_rig::Skin| -> Image {
        let mut preview = wisp_editor::preview::Preview::new(skin);
        preview.seek(0, 0, 640.0);
        let mut view = wisp_editor::view::Viewport::new(Rect::from_size(256.0, 256.0));
        view.zoom = 1.0;
        let mut scene = Scene::new();
        wisp_editor::overlay::draw_frame(preview.frame(), &view, 1.0, &mut scene);
        let target = p.offscreen(256, 256).unwrap();
        p.render(&target, &scene).unwrap();
        p.read(&target).unwrap()
    };

    let mut ed = Editor::open(&path).unwrap();
    let before = render(&mut p, ed.skin().unwrap().clone());
    dump(&before, "roundtrip_before");
    ed.save().unwrap();
    let after = render(&mut p, wisp_rig::Skin::load(&path).unwrap());
    dump(&after, "roundtrip_after");

    assert_eq!(
        before.max_abs_diff(&after),
        0,
        "saving and reloading changed how she looks"
    );
}
