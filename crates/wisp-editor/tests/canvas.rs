//! The vector canvas: hit-testing at every zoom, and path editing that keeps
//! the document valid.

mod support;

use support::{canonical, isolate, shipped_doc};

use wisp_editor::canvas::{self, Hit};
use wisp_editor::select::{Selection, Target};
use wisp_editor::view::{Viewport, GRAB_PX};
use wisp_paint::geom::{Point, Rect};
use wisp_rig::math::Vec2;
use wisp_rig::path::{Path, Verb};
use wisp_rig::skin::doc::SkinDoc;

fn view_at(zoom: f32) -> Viewport {
    let mut v = Viewport::new(Rect::new(0.0, 0.0, 800.0, 600.0));
    v.zoom = zoom;
    v.origin = Vec2::ZERO;
    v
}

fn shape_named(doc: &SkinDoc, name: &str) -> usize {
    doc.shapes
        .iter()
        .position(|s| s.name == name)
        .unwrap_or_else(|| panic!("the shipped skin has a shape {name:?}"))
}

// --------------------------------------------------------------- projection

#[test]
fn screen_and_canvas_are_exact_inverses_at_every_zoom() {
    for zoom in [0.05f32, 0.25, 1.0, 3.7, 16.0, 64.0] {
        let mut v = view_at(zoom);
        v.origin = Vec2::new(-13.5, 41.25);
        for p in [Vec2::ZERO, Vec2::new(128.0, 96.0), Vec2::new(-40.0, 250.0)] {
            let back = v.to_canvas(v.to_screen(p));
            assert!(back.dist(p) < 1e-2, "zoom {zoom}: {p:?} -> {back:?}");
        }
    }
}

#[test]
fn the_grab_radius_is_a_constant_number_of_pixels() {
    for zoom in [0.25f32, 1.0, 4.0, 32.0] {
        let v = view_at(zoom);
        let r = v.grab_radius();
        assert!(
            (r * zoom - GRAB_PX).abs() < 1e-3,
            "at zoom {zoom} the radius is {r} canvas units, which is {} px",
            r * zoom
        );
    }
}

#[test]
fn zooming_keeps_the_point_under_the_cursor_still() {
    let mut v = view_at(1.0);
    let cursor = Point::new(317.0, 208.0);
    let under = v.to_canvas(cursor);
    for _ in 0..6 {
        v.wheel(cursor, 1.0);
    }
    for _ in 0..3 {
        v.wheel(cursor, -1.0);
    }
    let still = v.to_canvas(cursor);
    assert!(still.dist(under) < 1e-2, "wanted {under:?}, got {still:?}");
}

#[test]
fn zoom_is_clamped_at_both_ends() {
    let mut v = view_at(1.0);
    for _ in 0..200 {
        v.wheel(Point::new(0.0, 0.0), 1.0);
    }
    assert!(v.zoom <= wisp_editor::view::MAX_ZOOM);
    for _ in 0..400 {
        v.wheel(Point::new(0.0, 0.0), -1.0);
    }
    assert!(v.zoom >= wisp_editor::view::MIN_ZOOM);
}

#[test]
fn fit_frames_the_whole_canvas_inside_the_viewport() {
    isolate();
    let doc = shipped_doc();
    let size = Vec2::new(doc.canvas.size[0].0, doc.canvas.size[1].0);
    let mut v = Viewport::new(Rect::new(20.0, 40.0, 640.0, 480.0));
    v.fit(size, 32.0);
    for corner in [Vec2::ZERO, size, Vec2::new(size.x, 0.0), Vec2::new(0.0, size.y)] {
        let p = v.to_screen(corner);
        assert!(
            p.x >= v.rect.x - 1.0
                && p.x <= v.rect.right() + 1.0
                && p.y >= v.rect.y - 1.0
                && p.y <= v.rect.bottom() + 1.0,
            "corner {corner:?} landed at {p:?}, outside {:?}",
            v.rect
        );
    }
}

// -------------------------------------------------------------- hit testing

#[test]
fn a_click_on_a_point_picks_that_point_at_every_zoom() {
    isolate();
    let doc = shipped_doc();
    let shape = shape_named(&doc, "body");
    let points = canvas::points_of(&doc, shape).unwrap();
    let sel = Selection::new();

    for zoom in [0.25f32, 0.5, 1.0, 2.0, 8.0, 32.0] {
        let mut v = view_at(zoom);
        for (i, p) in points.iter().enumerate() {
            v.centre_on(*p);
            let at = v.to_screen(*p);
            match canvas::hit_point(&doc, &v, at, &sel) {
                Hit::Point { shape: s, point, .. } => {
                    // Another point may legitimately sit closer at low zoom —
                    // what must never happen is picking a *different shape*
                    // or missing entirely.
                    assert_eq!(s, shape, "zoom {zoom}, point {i}: picked the wrong shape");
                    let picked = points[point];
                    assert!(
                        picked.dist(*p) <= v.grab_radius() + 1e-3,
                        "zoom {zoom}: picked point {point} at {picked:?}, wanted {p:?}"
                    );
                }
                other => panic!("zoom {zoom}, point {i} at {at:?}: {other:?}"),
            }
        }
    }
}

#[test]
fn a_click_well_away_from_every_point_picks_nothing() {
    isolate();
    let doc = shipped_doc();
    let sel = Selection::new();
    for zoom in [0.5f32, 1.0, 4.0] {
        let mut v = view_at(zoom);
        v.origin = Vec2::new(-10_000.0, -10_000.0);
        assert!(matches!(
            canvas::hit_point(&doc, &v, Point::new(400.0, 300.0), &sel),
            Hit::Nothing
        ));
    }
}

#[test]
fn a_click_just_outside_the_grab_radius_misses_and_just_inside_hits() {
    isolate();
    let doc = shipped_doc();
    let shape = shape_named(&doc, "body");
    let p = canvas::points_of(&doc, shape).unwrap()[0];
    let sel = Selection::new();
    // High zoom, so the neighbouring points are far away in pixels and the
    // only thing the test measures is the radius.
    let mut v = view_at(20.0);
    v.centre_on(p);
    let at = v.to_screen(p);

    let inside = Point::new(at.x + GRAB_PX - 1.0, at.y);
    assert!(matches!(canvas::hit_point(&doc, &v, inside, &sel), Hit::Point { .. }));

    let outside = Point::new(at.x + GRAB_PX + 2.0, at.y);
    assert!(
        matches!(canvas::hit_point(&doc, &v, outside, &sel), Hit::Nothing),
        "a click {} px away must miss",
        GRAB_PX + 2.0
    );
}

#[test]
fn the_selected_shape_wins_a_tie_with_an_overlapping_neighbour() {
    isolate();
    let mut doc = shipped_doc();
    // Two shapes with a point in exactly the same place, so neither is
    // closer and only the preference can break the tie.
    let a = shape_named(&doc, "body");
    let p = canvas::points_of(&doc, a).unwrap()[0];
    let mut path = Path::new();
    path.move_to(p);
    path.line_to(p + Vec2::new(30.0, 0.0));
    path.line_to(p + Vec2::new(30.0, 30.0));
    path.close();
    canvas::new_shape(&doc, "overlap", &path, "#ffffff").apply(&mut doc).unwrap();
    let b = shape_named(&doc, "overlap");

    let mut v = view_at(8.0);
    v.centre_on(p);
    let at = v.to_screen(p);

    let mut sel = Selection::new();
    sel.set(Target::Shape(a));
    match canvas::hit_point(&doc, &v, at, &sel) {
        Hit::Point { shape, .. } => assert_eq!(shape, a, "the selected shape should keep focus"),
        other => panic!("{other:?}"),
    }
    sel.set(Target::Shape(b));
    match canvas::hit_point(&doc, &v, at, &sel) {
        Hit::Point { shape, .. } => assert_eq!(shape, b),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_click_inside_a_filled_shape_picks_the_shape() {
    isolate();
    let doc = shipped_doc();
    let shape = shape_named(&doc, "body");
    let bounds = canvas::bounds_of(&doc, shape).unwrap();
    let centre = (bounds.min + bounds.max) * 0.5;
    let mut v = view_at(2.0);
    v.centre_on(centre);
    // The centre of her body is inside several shapes; the topmost wins, and
    // whichever it is, it must be one that really contains the point.
    match canvas::hit_shape(&doc, &v, v.to_screen(centre)) {
        Hit::Shape(i) => {
            let path = canvas::path_of(&doc, i).unwrap();
            assert!(canvas::contains(&path, centre), "shape {i} does not contain the point");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn even_odd_containment_treats_a_hole_as_a_hole() {
    let mut outer = canvas::blob(Vec2::new(0.0, 0.0), 50.0);
    let inner = canvas::blob(Vec2::new(0.0, 0.0), 20.0);
    outer.verbs.extend(inner.verbs);
    outer.points.extend(inner.points);
    assert!(canvas::contains(&outer, Vec2::new(35.0, 0.0)), "between the rings");
    assert!(!canvas::contains(&outer, Vec2::new(0.0, 0.0)), "in the hole");
    assert!(!canvas::contains(&outer, Vec2::new(80.0, 0.0)), "outside");
}

#[test]
fn a_click_on_a_segment_finds_where_the_pen_would_insert() {
    isolate();
    let mut doc = shipped_doc();
    let path = {
        let mut p = Path::new();
        p.move_to(Vec2::new(0.0, 0.0));
        p.line_to(Vec2::new(100.0, 0.0));
        p.line_to(Vec2::new(100.0, 100.0));
        p.close();
        p
    };
    canvas::new_shape(&doc, "probe", &path, "#ffffff").apply(&mut doc).unwrap();
    let shape = shape_named(&doc, "probe");
    let mut v = view_at(2.0);
    v.centre_on(Vec2::new(50.0, 0.0));
    match canvas::hit_segment(&doc, &v, v.to_screen(Vec2::new(50.0, 0.5))) {
        Hit::Segment { shape: s, after_point, at, .. } => {
            assert_eq!(s, shape);
            assert_eq!(after_point, 1, "the point after which a new one goes");
            assert!(at.dist(Vec2::new(50.0, 0.0)) < 1.0, "{at:?}");
        }
        other => panic!("{other:?}"),
    }
}

// ------------------------------------------------------------ path editing

#[test]
fn moving_points_moves_exactly_those_points() {
    isolate();
    let mut doc = shipped_doc();
    let shape = shape_named(&doc, "body");
    let before = canvas::points_of(&doc, shape).unwrap();
    let delta = Vec2::new(3.5, -2.25);
    canvas::move_points(&doc, shape, &[0, 2], delta).unwrap().apply(&mut doc).unwrap();
    let after = canvas::points_of(&doc, shape).unwrap();
    assert_eq!(before.len(), after.len());
    for (i, (a, b)) in before.iter().zip(&after).enumerate() {
        if i == 0 || i == 2 {
            assert!((*a + delta).dist(*b) < 1e-3, "point {i} did not move");
        } else {
            assert!(a.dist(*b) < 1e-3, "point {i} moved and should not have");
        }
    }
    wisp_rig::Skin::compile(doc).expect("still a valid skin");
}

#[test]
fn splitting_a_segment_adds_a_point_and_renumbers_the_weights() {
    isolate();
    let mut doc = shipped_doc();
    // A shape with explicit per-point weights, so the renumbering is testable.
    let shape = doc
        .shapes
        .iter()
        .position(|s| !s.weights.is_empty())
        .unwrap_or_else(|| {
            // The shipped skin may bind rigidly everywhere; make one.
            let s = shape_named(&doc, "body");
            wisp_editor::bones::paint_weight(&doc, s, 4, &doc.bones[1].name.clone(), 0.5)
                .unwrap()
                .apply(&mut doc)
                .unwrap();
            s
        });
    let before_points = canvas::points_of(&doc, shape).unwrap().len();
    let weighted: Vec<usize> = doc.shapes[shape].weights.iter().map(|w| w.point).collect();
    let split_after = weighted.iter().copied().min().unwrap_or(0);

    canvas::split_segment(&doc, shape, split_after, Vec2::new(1.0, 1.0))
        .unwrap()
        .apply(&mut doc)
        .unwrap();

    assert_eq!(canvas::points_of(&doc, shape).unwrap().len(), before_points + 1);
    let after: Vec<usize> = doc.shapes[shape].weights.iter().map(|w| w.point).collect();
    for (b, a) in weighted.iter().zip(&after) {
        let expected = if *b > split_after { b + 1 } else { *b };
        assert_eq!(*a, expected, "weight on point {b} should have become {expected}");
    }
    wisp_rig::Skin::compile(doc).expect("still valid");
}

#[test]
fn deleting_a_point_takes_its_verb_and_renumbers_the_weights() {
    isolate();
    let mut doc = shipped_doc();
    let shape = shape_named(&doc, "body");
    let bone = doc.bones[1].name.clone();
    // Weight a point after the one we will delete.
    wisp_editor::bones::paint_weight(&doc, shape, 8, &bone, 0.5)
        .unwrap()
        .apply(&mut doc)
        .unwrap();
    let path = canvas::path_of(&doc, shape).unwrap();
    // Delete an anchor in the middle of the path.
    let victim = 4usize;
    let (vi, _) = canvas::verb_of_point(&path, victim).unwrap();
    let verb_points = path.verbs[vi].point_count();
    let before = path.points.len();

    canvas::delete_point(&doc, shape, victim).unwrap().apply(&mut doc).unwrap();

    let after = canvas::points_of(&doc, shape).unwrap().len();
    assert_eq!(after, before - verb_points, "the whole verb goes, not one of its points");
    let w = doc.shapes[shape].weights.iter().find(|w| w.point == 8 - verb_points);
    assert!(w.is_some(), "the weight followed its point down: {:?}", doc.shapes[shape].weights);
    wisp_rig::Skin::compile(doc).expect("still valid");
}

#[test]
fn deleting_the_move_of_a_subpath_promotes_what_follows() {
    isolate();
    let mut doc = shipped_doc();
    let mut p = Path::new();
    p.move_to(Vec2::new(0.0, 0.0));
    p.line_to(Vec2::new(10.0, 0.0));
    p.line_to(Vec2::new(10.0, 10.0));
    p.close();
    canvas::new_shape(&doc, "probe", &p, "#ffffff").apply(&mut doc).unwrap();
    let shape = shape_named(&doc, "probe");

    canvas::delete_point(&doc, shape, 0).unwrap().apply(&mut doc).unwrap();
    let path = canvas::path_of(&doc, shape).unwrap();
    assert_eq!(path.verbs.first(), Some(&Verb::Move), "the subpath still starts with a move");
    assert_eq!(path.points.len(), 2);
    wisp_rig::Skin::compile(doc).expect("still valid");
}

#[test]
fn appending_with_the_pen_extends_the_path() {
    isolate();
    let mut doc = shipped_doc();
    let mut p = Path::new();
    p.move_to(Vec2::new(0.0, 0.0));
    canvas::new_shape(&doc, "probe", &p, "#ffffff").apply(&mut doc).unwrap();
    let shape = shape_named(&doc, "probe");

    canvas::append_point(&doc, shape, Vec2::new(20.0, 0.0)).unwrap().apply(&mut doc).unwrap();
    canvas::append_point(&doc, shape, Vec2::new(20.0, 20.0)).unwrap().apply(&mut doc).unwrap();
    let path = canvas::path_of(&doc, shape).unwrap();
    assert_eq!(path.verbs, vec![Verb::Move, Verb::Line, Verb::Line]);

    canvas::close_subpath(&doc, shape).unwrap().apply(&mut doc).unwrap();
    let path = canvas::path_of(&doc, shape).unwrap();
    assert_eq!(path.verbs.last(), Some(&Verb::Close));
    // Closing twice is refused rather than producing `Z Z`.
    assert!(canvas::close_subpath(&doc, shape).is_err());

    // A click after a close starts the next subpath.
    canvas::append_point(&doc, shape, Vec2::new(50.0, 50.0)).unwrap().apply(&mut doc).unwrap();
    let path = canvas::path_of(&doc, shape).unwrap();
    assert_eq!(path.verbs.last(), Some(&Verb::Move));
    wisp_rig::Skin::compile(doc).expect("still valid");
}

#[test]
fn a_new_shape_is_visible_the_moment_it_exists() {
    isolate();
    let mut doc = shipped_doc();
    let path = canvas::blob(Vec2::new(128.0, 128.0), 30.0);
    canvas::new_shape(&doc, "probe", &path, "violet").apply(&mut doc).unwrap();
    let s = &doc.shapes[shape_named(&doc, "probe")];
    let fill = s.fill.as_ref().expect("a new shape arrives with a fill");
    assert_eq!(fill.color, "violet");
    assert!(s.z > 0, "and on top of what is already there");
    let skin = wisp_rig::Skin::compile(doc).expect("valid");
    let compiled = skin.shapes.iter().find(|x| &*x.name == "probe").unwrap();
    assert!(compiled.fill.is_some());
}

#[test]
fn the_default_new_shape_is_round_because_she_is_a_creature() {
    // SPEC §3.5b: the geometry rule governs chrome, and the thing the editor
    // makes is not chrome. A new part of her starts as a circle.
    let p = canvas::blob(Vec2::new(0.0, 0.0), 10.0);
    assert!(p.verbs.iter().filter(|v| **v == Verb::Cubic).count() == 4);
    for ring in p.flatten(16) {
        for q in ring {
            assert!(
                (q.len() - 10.0).abs() < 0.2,
                "every flattened point should sit on the circle: {q:?}"
            );
        }
    }
}

#[test]
fn painting_a_fill_with_a_gradient_that_does_not_exist_is_refused() {
    isolate();
    let doc = shipped_doc();
    assert!(canvas::set_fill_gradient(&doc, 0, "no_such_gradient").is_err());
    assert!(canvas::set_fill_gradient(&doc, 0, &doc.gradients[0].name.clone()).is_ok());
}

#[test]
fn an_untouched_shapes_path_text_is_carried_through_byte_for_byte() {
    isolate();
    let mut doc = shipped_doc();
    let originals: Vec<String> = doc.shapes.iter().map(|s| s.path.clone()).collect();
    let edited = shape_named(&doc, "body");
    canvas::move_points(&doc, edited, &[0], Vec2::new(1.0, 0.0))
        .unwrap()
        .apply(&mut doc)
        .unwrap();
    for (i, s) in doc.shapes.iter().enumerate() {
        if i == edited {
            continue;
        }
        assert_eq!(s.path, originals[i], "shape {:?} was rewritten and should not have", s.name);
    }
}

#[test]
fn the_canvas_anchor_and_size_are_editable_and_validated() {
    isolate();
    let mut doc = shipped_doc();
    canvas::set_anchor(&doc, Vec2::new(10.0, 20.0)).apply(&mut doc).unwrap();
    assert_eq!(doc.canvas.anchor[0].0, 10.0);
    assert!(canvas::set_canvas_size(&doc, Vec2::new(0.0, 100.0)).is_err());
    canvas::set_canvas_size(&doc, Vec2::new(300.0, 400.0))
        .unwrap()
        .apply(&mut doc)
        .unwrap();
    assert_eq!(doc.canvas.size[1].0, 400.0);
    wisp_rig::Skin::compile(doc).expect("still valid");
}

#[test]
fn every_shape_in_the_shipped_skin_parses_and_re_emits() {
    isolate();
    let doc = shipped_doc();
    let before = canonical(&doc);
    for i in 0..doc.shapes.len() {
        let path = canvas::path_of(&doc, i)
            .unwrap_or_else(|e| panic!("shape {:?}: {e}", doc.shapes[i].name));
        assert!(!path.points.is_empty(), "shape {:?} has no points", doc.shapes[i].name);
        // Re-emitting and re-parsing must give the same geometry.
        let again = wisp_rig::path::Path::parse(&path.to_svg()).unwrap();
        assert_eq!(again.verbs, path.verbs);
        for (a, b) in again.points.iter().zip(&path.points) {
            assert!(a.dist(*b) < 1e-3, "{a:?} vs {b:?}");
        }
    }
    assert_eq!(before, canonical(&doc), "reading must not mutate");
}
