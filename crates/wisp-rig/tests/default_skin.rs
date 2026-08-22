//! The shipped default skin, "Wisp" (F73).
//!
//! These tests are what proves the skin format by use: she is an ordinary skin
//! file, loaded through the ordinary parser, with no privileged path. If the
//! format regresses, she stops loading and this file says so.

use wisp_rig::math::Vec2;
use wisp_rig::paint::{nx, Paint, Rgba};
use wisp_rig::skin::{IkKind, IkTarget, PaintRef, Skin};
use wisp_rig::{path::Verb, REQUIRED_CLIPS, REQUIRED_EXPRESSIONS};

mod support;

fn wisp() -> Skin {
    support::isolate_config_dir();
    match wisp_rig::default_skin() {
        Ok(s) => s,
        Err(e) => panic!("the shipped default skin does not compile:\n{e}"),
    }
}

#[test]
fn she_compiles() {
    let s = wisp();
    assert_eq!(&*s.meta.name, "Wisp");
    assert_eq!(&*s.meta.author, "nerdrx");
    assert_eq!(&*s.meta.license, "MIT");
    assert!(!s.meta.summary.is_empty());
}

#[test]
fn she_loads_from_a_file_exactly_as_she_does_from_the_embedded_copy() {
    let dir = support::isolate_config_dir();
    let path = dir.path().join("wisp.skin.toml");
    std::fs::write(&path, wisp_rig::WISP_SKIN_TOML).unwrap();
    let from_disk = Skin::load(&path).unwrap();
    assert_eq!(from_disk.doc(), wisp().doc());
}

#[test]
fn her_size_range_is_the_one_f75_asks_for() {
    let s = wisp();
    assert_eq!(s.meta.min_size_px, 48.0);
    assert_eq!(s.meta.max_size_px, 512.0);
    assert_eq!(s.meta.default_size_px, 128.0);
}

#[test]
fn she_has_every_clip_the_plan_names() {
    let s = wisp();
    assert!(
        s.missing_required_clips().is_empty(),
        "missing clips: {:?}",
        s.missing_required_clips()
    );
    for name in REQUIRED_CLIPS {
        assert!(s.clip_index(name).is_some(), "{name}");
    }
}

#[test]
fn she_has_all_eight_expressions_of_f74() {
    let s = wisp();
    assert!(
        s.missing_required_expressions().is_empty(),
        "missing expressions: {:?}",
        s.missing_required_expressions()
    );
    for name in REQUIRED_EXPRESSIONS {
        let i = s.expression_index(name).unwrap_or_else(|| panic!("{name}"));
        let e = &s.expressions[i];
        assert!(
            s.layers[e.layer].additive,
            "{name} should ride on an additive layer, not replace the base"
        );
        assert!(s.clips[e.clip].additive, "{name}'s clip should be additive");
    }
}

#[test]
fn her_breathing_and_blinking_run_on_top_of_whatever_else_she_is_doing() {
    // F70: she never freezes. Both must be additive layers with a default
    // clip, so they play from the moment the rig starts.
    let s = wisp();
    for layer in ["breathe", "blink"] {
        let i = s.layer_index(layer).unwrap_or_else(|| panic!("no {layer} layer"));
        assert!(s.layers[i].additive, "{layer} must be additive");
        assert!(s.layers[i].default_clip.is_some(), "{layer} needs a default clip");
    }
    assert_eq!(s.layer_index("base"), Some(0), "the base layer must evaluate first");
    assert!(!s.layers[0].additive);
}

#[test]
fn the_hop_is_the_only_clip_that_does_not_loop() {
    let s = wisp();
    for c in &s.clips {
        let expect_loop = &*c.name != "hop";
        assert_eq!(c.looping, expect_loop, "{} looping = {}", c.name, c.looping);
    }
}

#[test]
fn nothing_she_does_is_a_linear_tween() {
    // F67: "never a linear tween". `linear` exists in the format for genuinely
    // constant motion, and she does not have any.
    let s = wisp();
    for c in &s.clips {
        for t in &c.tracks {
            for (i, e) in t.eases.iter().enumerate() {
                assert_ne!(
                    *e,
                    wisp_rig::Ease::Linear,
                    "clip {:?} key {i} on channel {} uses a linear ease",
                    c.name,
                    t.channel.name()
                );
            }
        }
    }
}

#[test]
fn her_skeleton_has_the_parts_the_brief_calls_for() {
    let s = wisp();
    for bone in ["root", "body", "head", "gaze", "eye_l", "eye_r", "light", "tail1", "tail2", "tail3"]
    {
        assert!(s.bone_index(bone).is_some(), "missing bone {bone}");
    }
    // Parents precede children, so a pose resolves in one forward pass.
    for (i, b) in s.skeleton.bones().iter().enumerate() {
        if let Some(p) = b.parent {
            assert!(p < i, "{} is ordered before its parent", b.name);
        }
    }
}

#[test]
fn her_eyes_track_the_cursor_through_a_dedicated_gaze_bone() {
    // F69. The look-at owns the rotation channel it writes, so it gets a bone
    // of its own and the head stays free for authored motion.
    let s = wisp();
    let gaze = s
        .iks
        .iter()
        .find(|k| k.target == IkTarget::Cursor)
        .expect("no cursor-driven constraint");
    match gaze.kind {
        IkKind::LookAt { bone, cfg } => {
            assert_eq!(bone, s.bone_index("gaze").unwrap());
            // A narrow cone: a mote of glass that swivels 90 degrees reads as
            // a turret, not as attention.
            assert!(cfg.max_angle < 0.4, "the gaze cone is too wide: {}", cfg.max_angle);
            assert!(cfg.max_angle > 0.1);
        }
        other => panic!("expected a look_at, got {other:?}"),
    }
    // No clip may key the gaze bone, or the constraint and the clip would
    // fight over the same channel every frame.
    let gaze_bone = s.bone_index("gaze").unwrap();
    for c in &s.clips {
        assert!(
            !c.touched_bones().contains(&gaze_bone),
            "clip {:?} keys the gaze bone, which the look-at owns",
            c.name
        );
    }
}

#[test]
fn her_tail_is_a_secondary_motion_chain_down_the_bone_tree() {
    let s = wisp();
    let tail = s.chains.iter().find(|c| &*c.name == "tail").expect("no tail chain");
    assert_eq!(tail.bones.len(), 3);
    for w in 1..tail.bones.len() {
        assert_eq!(
            s.skeleton.bone(tail.bones[w]).parent,
            Some(tail.bones[w - 1]),
            "the tail chain must follow the bone tree"
        );
    }
    // Nearly weightless, and draggy: it is a wisp-trail, not a rope.
    assert!(tail.params.gravity > 0.0 && tail.params.gravity < 120.0);
    assert!(tail.params.drag > 0.0);
}

#[test]
fn the_light_moves_inside_her() {
    // F73: cyan light moving *inside* the violet core, riding her motion
    // rather than flashing (DESIGN.md §1).
    let s = wisp();
    let light = s.motion.light_bone.expect("no light bone wired up");
    assert_eq!(Some(light), s.bone_index("light"));
    assert!(s.motion.light_gain > 0.0);
    assert!(s.motion.light_range > 0.0);

    let following: Vec<&str> = s
        .gradients
        .iter()
        .filter(|g| g.follow_bone == Some(light))
        .map(|g| &*g.name)
        .collect();
    assert!(
        following.contains(&"core") && following.contains(&"spark"),
        "the core and the spark must ride the light bone, got {following:?}"
    );
    assert!(following.contains(&"sheen"), "the specular band must ride it too");
}

#[test]
fn violet_leads_and_cyan_is_only_light_inside_the_material() {
    // DESIGN.md §1. Cyan may appear in a gradient — as light — but never as
    // the flat fill of a surface.
    let s = wisp();
    for shape in &s.shapes {
        if let Some(PaintRef::Solid(c)) = shape.fill {
            assert!(
                (c.r - nx::CYAN.r).abs() > 0.01
                    || (c.g - nx::CYAN.g).abs() > 0.01
                    || (c.b - nx::CYAN.b).abs() > 0.01,
                "shape {:?} uses cyan as a surface colour",
                shape.name
            );
        }
    }
    // ...and the brand violet really is the brand violet.
    let core = s
        .gradients
        .iter()
        .find(|g| &*g.name == "core")
        .expect("no core gradient");
    assert!(
        core.stops.iter().any(|st| {
            (st.color.r - nx::VIOLET.r).abs() < 0.02
                && (st.color.g - nx::VIOLET.g).abs() < 0.02
                && (st.color.b - nx::VIOLET.b).abs() < 0.02
        }),
        "the core is not NX violet: {:?}",
        core.stops
    );
    let spark = s.gradients.iter().find(|g| &*g.name == "spark").unwrap();
    assert!(
        spark.stops.iter().any(|st| {
            (st.color.g - nx::CYAN.g).abs() < 0.02 && (st.color.b - nx::CYAN.b).abs() < 0.02
        }),
        "the internal light is not NX cyan"
    );
}

#[test]
fn she_is_round_because_she_is_a_creature_and_not_chrome() {
    // SPEC.md §3.5b. This test used to assert the opposite — that every edge of
    // her was straight, because DESIGN.md §1 says "angular, never rounded".
    //
    // That rule governs *chrome*: bubbles, panels, menus, the rig editor. It was
    // applied to the character by mistake, and the result was a faceted crystal
    // that at 96 px — the size she actually is on a desktop — read as a dark
    // smudge. F73's revised brief makes her a chibi, and a chibi is round by
    // definition. So the assertion is inverted rather than deleted: her artwork
    // must *keep* its curves, and the next person to "fix" her to comply with
    // the geometry rule gets a failing test and this comment.
    let s = wisp();
    let curved = |sh: &wisp_rig::skin::ShapeDef| {
        sh.path
            .verbs
            .iter()
            .any(|v| matches!(v, Verb::Quad | Verb::Cubic))
    };
    for name in ["shell", "hair_back", "hair_front", "body", "eye_l", "eye_r"] {
        let sh = s
            .shapes
            .iter()
            .find(|sh| &*sh.name == name)
            .unwrap_or_else(|| panic!("no {name} shape"));
        assert!(curved(sh), "shape {name:?} has been re-angularised — see SPEC.md §3.5b");
    }
    // ...and she is round nearly everywhere, not just in those six places.
    let round = s.shapes.iter().filter(|sh| curved(sh)).count();
    assert!(
        round * 4 >= s.shapes.len() * 3,
        "only {round} of {} shapes carry a curve",
        s.shapes.len()
    );
}

#[test]
fn her_lit_edge_runs_from_the_upper_left() {
    // One light source, upper-left, in every gradient and edge.
    let s = wisp();
    for name in ["glass_body", "edge_lit", "facet_light"] {
        let g = s
            .gradients
            .iter()
            .find(|g| &*g.name == name)
            .unwrap_or_else(|| panic!("no {name} gradient"));
        match g.geom {
            wisp_rig::skin::GradientGeom::Linear { start, end } => {
                assert!(
                    end.x >= start.x && end.y >= start.y,
                    "{name} runs from the wrong corner: {start:?} -> {end:?}"
                );
            }
            other => panic!("{name} should be linear, got {other:?}"),
        }
    }
    // Radial gradients put their focus above and left of their centre, which
    // is what gives glass an off-axis highlight instead of a bullseye.
    for name in ["core", "spark", "aura"] {
        let g = s.gradients.iter().find(|g| &*g.name == name).unwrap();
        match g.geom {
            wisp_rig::skin::GradientGeom::Radial { center, focus, .. } => {
                assert!(
                    focus.x <= center.x && focus.y <= center.y,
                    "{name}'s highlight is on the wrong side: focus {focus:?}, centre {center:?}"
                );
            }
            other => panic!("{name} should be radial, got {other:?}"),
        }
    }
}

#[test]
fn the_edge_stroke_is_a_hairline_at_every_size() {
    // Stroke width is in canvas units, so the lit edge keeps its proportion
    // when the size slider moves (F75).
    let s = wisp();
    let edge = s
        .shapes
        .iter()
        .find(|sh| &*sh.name == "edge")
        .expect("no lit edge shape");
    let stroke = edge.stroke.expect("the edge shape has no stroke");
    assert!(edge.fill.is_none(), "the edge is a stroke, not a fill");
    // ~1px at her 128px default, and it scales from there.
    let px_at_default = stroke.width * s.scale_for(128.0);
    assert!(
        (0.7..=1.6).contains(&px_at_default),
        "the lit edge is {px_at_default}px at the default size"
    );
}

#[test]
fn the_glow_and_the_faint_tail_tip_are_click_through() {
    // F2: only actual body pixels take clicks.
    let s = wisp();
    let by_name = |n: &str| s.shapes.iter().find(|sh| &*sh.name == n).unwrap();
    assert!(!by_name("aura").silhouette, "the bloom must be click-through");
    assert!(!by_name("tail_c").silhouette, "the faintest wisp must be click-through");
    assert!(by_name("shell").silhouette, "her body must take clicks");
    assert!(by_name("tail_a").silhouette);
}

#[test]
fn every_shape_is_bound_to_a_bone_with_valid_weights() {
    let s = wisp();
    for shape in &s.shapes {
        assert!(
            shape.binding.is_valid(s.skeleton.len()),
            "shape {:?} has an invalid binding",
            shape.name
        );
        assert_eq!(
            shape.binding.point_count(),
            shape.path.point_count(),
            "shape {:?} binds a different number of points than it has",
            shape.name
        );
        for i in 0..shape.binding.point_count() {
            let sum: f32 = shape.binding.influences_of(i).iter().map(|f| f.weight).sum();
            assert!(
                (sum - 1.0).abs() < 1e-4,
                "shape {:?} point {i} has weights summing to {sum}",
                shape.name
            );
        }
    }
}

#[test]
fn her_artwork_fits_the_canvas_she_declares() {
    let s = wisp();
    for shape in &s.shapes {
        let b = shape.path.bounds();
        assert!(
            b.min.x >= -4.0 && b.min.y >= -4.0,
            "shape {:?} starts off-canvas at {:?}",
            shape.name,
            b.min
        );
        assert!(
            b.max.x <= s.canvas.size.x + 4.0 && b.max.y <= s.canvas.size.y + 4.0,
            "shape {:?} runs off the canvas to {:?}",
            shape.name,
            b.max
        );
    }
    // The anchor is where she touches down, so it sits on the body, not in
    // the middle of the empty canvas.
    assert!(s.canvas.anchor.y > s.canvas.size.y * 0.5);
}

#[test]
fn she_round_trips_through_the_serialiser() {
    let a = wisp();
    let text = a.to_toml().unwrap();
    let b = Skin::parse(&text).unwrap_or_else(|e| panic!("re-serialised skin will not parse:\n{e}"));
    assert_eq!(a.doc(), b.doc());
    assert_eq!(a.clips, b.clips);
    assert_eq!(a.shapes, b.shapes);
    assert_eq!(a.gradients, b.gradients);
    assert_eq!(a.layers, b.layers);
    assert_eq!(a.expressions, b.expressions);
    assert_eq!(a.chains, b.chains);
    assert_eq!(a.iks, b.iks);
}

#[test]
fn the_shipped_file_carries_its_own_licence_and_explanation() {
    // A community format lives or dies on whether the reference pack is
    // readable. This is the one thing TOML gives us that JSON cannot.
    let src = wisp_rig::WISP_SKIN_TOML;
    assert!(src.starts_with("# Wisp"), "the file needs a header comment");
    assert!(src.contains("DESIGN.md"), "the design rules should be stated in the file");
    assert!(src.contains("SPEC.md §3.6"));
    let comment_lines = src.lines().filter(|l| l.trim_start().starts_with('#')).count();
    assert!(comment_lines > 40, "only {comment_lines} comment lines");
}

#[test]
fn her_physics_are_lighter_than_the_defaults() {
    // She floats. Falling like a brick would be wrong.
    let s = wisp();
    let d = wisp_rig::PhysicsParams::default();
    assert!(s.physics.gravity < d.gravity, "a mote should fall slowly");
    assert!(s.physics.drag > d.drag, "a mote should feel air resistance");
}

#[test]
fn nothing_in_the_frame_is_transparent_black_by_accident() {
    // A stop that failed to resolve would come out as fully transparent, and
    // the shape would silently vanish rather than erroring.
    let s = wisp();
    for shape in &s.shapes {
        let Some(fill) = shape.fill else { continue };
        let opaque_enough = match fill {
            PaintRef::Solid(c) => c.a > 0.05,
            PaintRef::Gradient { index, .. } => {
                s.gradients[index].stops.iter().any(|st| st.color.a > 0.05)
            }
        };
        assert!(opaque_enough, "shape {:?} would draw nothing", shape.name);
    }
}

#[test]
fn her_palette_is_the_nx_palette() {
    let s = wisp();
    let named: Vec<(&str, Rgba)> = s
        .doc()
        .colors
        .iter()
        .map(|c| (c.name.as_str(), Rgba::parse_hex(&c.value).unwrap()))
        .collect();
    let get = |n: &str| named.iter().find(|(k, _)| *k == n).map(|(_, v)| *v);
    assert_eq!(get("violet"), Some(nx::VIOLET));
    assert_eq!(get("cyan"), Some(nx::CYAN));
    assert_eq!(get("text"), Some(nx::TEXT));
}

#[test]
fn a_gradient_paint_carries_its_stops_into_the_frame() {
    // Sanity on the rig's paint construction: what the skin declares is what
    // the renderer is handed.
    let s = wisp();
    let rig = wisp_rig::Rig::new(s);
    let mut r = rig;
    r.update(1.0 / 60.0, &Default::default());
    let shell = r.frame().shape("shell").expect("no shell in the frame");
    match shell.fill.as_ref().expect("the shell has no fill") {
        Paint::Linear(g) => {
            assert_eq!(g.stops.len(), 3);
            assert!(g.start.is_finite() && g.end.is_finite());
            assert_ne!(g.start, g.end);
        }
        other => panic!("the shell should be a linear gradient, got {other:?}"),
    }
    assert_ne!(shell.points[0], Vec2::ZERO);
}
