//! End-to-end: the whole rig running, at every size and every tier.
//!
//! No GPU, no compositor, no window — SPEC §4's rule for pure modules. Every
//! assertion here is about geometry the renderer would be handed.

use wisp_proto::{Governed, Tier, TierReason};
use wisp_rig::contour::ContourOptions;
use wisp_rig::math::{Rect, Vec2};
use wisp_rig::physics::{self, BodyState, Forces, PhysicsParams, Surface};
use wisp_rig::rig::{Detail, Rig, RigInput};
use wisp_rig::{default_skin, RigFrame};

mod support;

fn rig() -> Rig {
    support::isolate_config_dir();
    Rig::new(default_skin().expect("the shipped skin compiles"))
}

fn at(anchor: Vec2) -> RigInput {
    RigInput { size_px: 128.0, anchor, ..Default::default() }
}

/// Run `frames` frames at 60 fps and return the rig.
fn run(rig: &mut Rig, input: &RigInput, frames: usize) {
    for _ in 0..frames {
        rig.update(1.0 / 60.0, input);
    }
}

fn all_finite(f: &RigFrame) -> bool {
    f.shapes
        .iter()
        .all(|s| s.points.iter().all(|p| p.is_finite()))
}

// ---------------------------------------------------------------------------
// Basics
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_rig_produces_a_frame_at_the_anchor() {
    let mut r = rig();
    r.update(1.0 / 60.0, &at(Vec2::new(900.0, 540.0)));
    let f = r.frame();
    assert!(!f.shapes.is_empty());
    assert!(all_finite(f));
    assert!(!f.bounds.is_empty());
    // The canvas anchor is her bottom point, so she sits above where she is
    // placed and is roughly centred on it horizontally.
    assert!((f.bounds.min.x + f.bounds.max.x) * 0.5 - 900.0 < 30.0);
    assert!(f.bounds.min.y < 540.0);
}

#[test]
fn shapes_come_out_back_to_front() {
    let mut r = rig();
    r.update(1.0 / 60.0, &at(Vec2::ZERO));
    let z: Vec<i32> = r.frame().shapes.iter().map(|s| s.z).collect();
    let mut sorted = z.clone();
    sorted.sort_unstable();
    assert_eq!(z, sorted);
    assert_eq!(&*r.frame().shapes[0].name, "aura", "the bloom draws first");
    assert_eq!(
        &*r.frame().shapes.last().unwrap().name,
        "edge",
        "the lit edge draws last"
    );
}

#[test]
fn she_never_freezes() {
    // F70. With no input at all, breathing and blinking still move her.
    let mut r = rig();
    let input = at(Vec2::new(500.0, 500.0));
    r.update(1.0 / 60.0, &input);
    let first = r.frame().shape("shell").unwrap().points.clone();
    run(&mut r, &input, 30);
    let later = r.frame().shape("shell").unwrap().points.clone();
    let moved: f32 = first
        .iter()
        .zip(later.iter())
        .map(|(a, b)| a.dist(*b))
        .fold(0.0, f32::max);
    assert!(moved > 0.2, "she is standing perfectly still: moved {moved}px");
}

#[test]
fn she_blinks() {
    // The blink layer runs on top of everything and closes the eyes for a
    // handful of frames per cycle.
    let mut r = rig();
    let input = at(Vec2::new(500.0, 500.0));
    let mut min_height = f32::MAX;
    let mut max_height: f32 = 0.0;
    for _ in 0..400 {
        r.update(1.0 / 60.0, &input);
        let eye = r.frame().shape("eye_l").unwrap().bounds();
        min_height = min_height.min(eye.height());
        max_height = max_height.max(eye.height());
    }
    assert!(
        min_height < max_height * 0.4,
        "the eyes never closed: {min_height} to {max_height}"
    );
}

#[test]
fn the_rig_is_deterministic() {
    let run_once = || {
        let mut r = rig();
        let input = RigInput {
            size_px: 128.0,
            anchor: Vec2::new(300.0, 300.0),
            velocity: Vec2::new(180.0, -60.0),
            cursor: Some(Vec2::new(900.0, 200.0)),
            ..Default::default()
        };
        run(&mut r, &input, 200);
        r.frame().shapes.iter().map(|s| s.points.clone()).collect::<Vec<_>>()
    };
    assert_eq!(run_once(), run_once());
}

#[test]
fn pathological_frame_times_do_not_break_her() {
    let mut r = rig();
    let input = at(Vec2::new(400.0, 400.0));
    for dt in [0.0, -1.0, f32::NAN, f32::INFINITY, 900.0] {
        r.update(dt, &input);
        assert!(all_finite(r.frame()), "dt = {dt} produced NaN geometry");
    }
}

// ---------------------------------------------------------------------------
// F75 — resolution independence
// ---------------------------------------------------------------------------

#[test]
fn she_is_the_same_creature_at_ninety_six_pixels_and_at_five_hundred_and_twelve() {
    let mut small = rig();
    let mut large = rig();
    let anchor = Vec2::new(1000.0, 600.0);
    small.update(1.0 / 60.0, &RigInput { size_px: 96.0, anchor, ..Default::default() });
    large.update(1.0 / 60.0, &RigInput { size_px: 512.0, anchor, ..Default::default() });

    let sb = small.frame().bounds;
    let lb = large.frame().bounds;
    let ratio = lb.width() / sb.width();
    assert!(
        (ratio - 512.0 / 96.0).abs() < 0.05,
        "she does not scale linearly: {ratio}"
    );
    // Same proportions, so she reads as the same character, not a stretched one.
    assert!(
        ((lb.width() / lb.height()) - (sb.width() / sb.height())).abs() < 0.01,
        "aspect changed with size"
    );
    // The lit edge scales with her, staying a hairline.
    let se = small.frame().shape("edge").unwrap().stroke.as_ref().unwrap().width;
    let le = large.frame().shape("edge").unwrap().stroke.as_ref().unwrap().width;
    assert!((le / se - 512.0 / 96.0).abs() < 0.05, "the edge did not scale");
}

#[test]
fn the_size_slider_is_clamped_to_the_skins_range() {
    let mut r = rig();
    let anchor = Vec2::new(500.0, 500.0);
    r.update(1.0 / 60.0, &RigInput { size_px: 5000.0, anchor, ..Default::default() });
    let huge = r.frame().size_px;
    assert_eq!(huge, 512.0);
    r.update(1.0 / 60.0, &RigInput { size_px: 1.0, anchor, ..Default::default() });
    assert_eq!(r.frame().size_px, 48.0);
}

// ---------------------------------------------------------------------------
// F69 — attention
// ---------------------------------------------------------------------------

#[test]
fn her_eyes_follow_the_cursor() {
    let anchor = Vec2::new(600.0, 600.0);
    let eye_centre = |cursor: Vec2| {
        let mut r = rig();
        // Enough frames to settle, but the look-at is not a spring so one is
        // nearly enough.
        for _ in 0..4 {
            r.update(
                1.0 / 60.0,
                &RigInput { size_px: 256.0, anchor, cursor: Some(cursor), ..Default::default() },
            );
        }
        let l = r.frame().shape("pupil_l").unwrap().bounds();
        Vec2::new((l.min.x + l.max.x) * 0.5, (l.min.y + l.max.y) * 0.5)
    };
    let left = eye_centre(Vec2::new(100.0, 500.0));
    let right = eye_centre(Vec2::new(1500.0, 500.0));
    assert!(
        right.x > left.x + 2.0,
        "her eyes did not track: left {left:?}, right {right:?}"
    );
}

#[test]
fn a_cursor_on_the_far_side_of_the_screen_does_not_spin_her_head() {
    let anchor = Vec2::new(600.0, 600.0);
    let mut r = rig();
    let input = RigInput {
        size_px: 256.0,
        anchor,
        cursor: Some(Vec2::new(-9000.0, 9000.0)),
        ..Default::default()
    };
    run(&mut r, &input, 30);
    // The eyes stay inside the head. The cone is what guarantees it.
    let head = r.frame().shape("shell").unwrap().bounds();
    let eye = r.frame().shape("eye_l").unwrap().bounds();
    assert!(head.contains(eye.min) && head.contains(eye.max), "an eye left her body");
}

#[test]
fn attention_falls_back_to_the_cursor_when_nothing_else_has_it() {
    let mut a = rig();
    let mut b = rig();
    let anchor = Vec2::new(600.0, 600.0);
    let cursor = Some(Vec2::new(1400.0, 300.0));
    a.update(1.0 / 60.0, &RigInput { anchor, cursor, ..Default::default() });
    b.update(
        1.0 / 60.0,
        &RigInput { anchor, cursor, attention: cursor, ..Default::default() },
    );
    assert_eq!(
        a.frame().shape("eye_l").unwrap().points,
        b.frame().shape("eye_l").unwrap().points
    );
}

// ---------------------------------------------------------------------------
// F67 — motion quality
// ---------------------------------------------------------------------------

#[test]
fn moving_fast_stretches_her_along_her_travel() {
    let anchor = Vec2::new(600.0, 600.0);
    let width_at = |velocity: Vec2| {
        let mut r = rig();
        let input = RigInput { size_px: 256.0, anchor, velocity, ..Default::default() };
        run(&mut r, &input, 40);
        r.frame().shape("shell").unwrap().bounds()
    };
    let still = width_at(Vec2::ZERO);
    let sideways = width_at(Vec2::new(1400.0, 0.0));
    assert!(
        sideways.width() > still.width() * 1.03,
        "no horizontal stretch: {} vs {}",
        sideways.width(),
        still.width()
    );
    assert!(
        sideways.height() < still.height(),
        "she did not squash across her travel"
    );
}

#[test]
fn the_body_lags_and_overshoots_when_she_changes_direction() {
    // The visible body chases a spring that rings past its target — that is
    // what makes a turn read as weight rather than as a teleport.
    let mut r = rig();
    let mut x = 400.0f32;
    // Travel right for a while.
    for _ in 0..60 {
        x += 8.0;
        r.update(1.0 / 60.0, &at(Vec2::new(x, 500.0)));
    }
    let offset_before = r.frame().shape("shell").unwrap().bounds().min.x - x;
    // Stop dead.
    let mut min_offset = f32::MAX;
    let mut max_offset = f32::MIN;
    for _ in 0..90 {
        r.update(1.0 / 60.0, &at(Vec2::new(x, 500.0)));
        let o = r.frame().shape("shell").unwrap().bounds().min.x - x;
        min_offset = min_offset.min(o);
        max_offset = max_offset.max(o);
    }
    assert!(
        max_offset - min_offset > 1.0,
        "the body did not settle at all after stopping ({min_offset}..{max_offset}, was {offset_before})"
    );
}

#[test]
fn the_tail_arrives_late_and_keeps_going() {
    let mut r = rig();
    let mut x = 400.0f32;
    r.update(1.0 / 60.0, &at(Vec2::new(x, 500.0)));
    let rest_gap = tail_gap(&r);
    for _ in 0..12 {
        x += 22.0;
        r.update(1.0 / 60.0, &at(Vec2::new(x, 500.0)));
    }
    let moving_gap = tail_gap(&r);
    assert!(
        moving_gap > rest_gap + 1.0,
        "the tail kept up perfectly: {rest_gap} then {moving_gap}"
    );

    // Stop, and it carries on for a few frames rather than snapping back.
    let before = r.frame().shape("tail_b").unwrap().bounds();
    r.update(1.0 / 60.0, &at(Vec2::new(x, 500.0)));
    let after = r.frame().shape("tail_b").unwrap().bounds();
    assert!(before.min.dist(after.min) > 0.05, "the tail froze the instant she stopped");
}

/// How far the tail's tip trails behind the point directly below her body.
fn tail_gap(r: &Rig) -> f32 {
    let body = r.frame().shape("shell").unwrap().bounds();
    let tail = r.frame().shape("tail_b").unwrap().bounds();
    let body_x = (body.min.x + body.max.x) * 0.5;
    let tail_x = (tail.min.x + tail.max.x) * 0.5;
    (body_x - tail_x).abs()
}

#[test]
fn the_internal_light_slides_against_her_travel() {
    // DESIGN.md §1: light rides motion. The cyan mote inside her is displaced
    // by her velocity, so the highlight moves through the glass.
    let spark_at = |velocity: Vec2| {
        let mut r = rig();
        let input =
            RigInput { size_px: 256.0, anchor: Vec2::new(600.0, 600.0), velocity, ..Default::default() };
        run(&mut r, &input, 60);
        let b = r.frame().shape("spark").unwrap().bounds();
        Vec2::new((b.min.x + b.max.x) * 0.5, (b.min.y + b.max.y) * 0.5)
    };
    let still = spark_at(Vec2::ZERO);
    let right = spark_at(Vec2::new(900.0, 0.0));
    let left = spark_at(Vec2::new(-900.0, 0.0));
    assert!(right.x < still.x - 1.0, "the light did not lag her travel: {right:?} vs {still:?}");
    assert!(left.x > still.x + 1.0);
}

// ---------------------------------------------------------------------------
// F74 — expressions
// ---------------------------------------------------------------------------

#[test]
fn every_expression_can_be_set_and_changes_her_face() {
    let mut r = rig();
    let input = at(Vec2::new(500.0, 500.0));
    let mut seen = Vec::new();
    for name in wisp_rig::REQUIRED_EXPRESSIONS {
        assert!(r.set_expression(name), "could not set {name}");
        // Past the fade, and into the clip.
        run(&mut r, &input, 40);
        assert_eq!(r.current_expression(), Some(name));
        let eye = r.frame().shape("eye_l").unwrap().bounds().height();
        seen.push((name, eye));
        assert!(all_finite(r.frame()));
    }
    // Not every expression is about the eyes, but the set as a whole must
    // produce genuinely different faces.
    let heights: Vec<f32> = seen.iter().map(|(_, h)| *h).collect();
    let spread = heights.iter().cloned().fold(f32::MIN, f32::max)
        - heights.iter().cloned().fold(f32::MAX, f32::min);
    assert!(spread > 2.0, "the expressions all look the same: {seen:?}");
}

#[test]
fn an_expression_the_skin_does_not_have_is_refused() {
    let mut r = rig();
    assert!(!r.set_expression("incandescent"));
}

#[test]
fn an_expression_rides_on_top_of_what_she_is_doing() {
    // F70's whole point: setting a face does not stop her walking.
    let mut r = rig();
    let input = at(Vec2::new(500.0, 500.0));
    assert!(r.play("walk", 0.0));
    r.set_expression("delighted");
    run(&mut r, &input, 30);
    assert_eq!(r.player().current(0), r.skin().clip_index("walk"));
    assert_eq!(r.current_expression(), Some("delighted"));
}

#[test]
fn clips_can_be_played_by_name_and_unknown_ones_are_refused() {
    let mut r = rig();
    for name in wisp_rig::REQUIRED_CLIPS {
        assert!(r.play(name, 120.0), "could not play {name}");
        run(&mut r, &at(Vec2::ZERO), 20);
        assert!(all_finite(r.frame()));
    }
    assert!(!r.play("moonwalk", 0.0));
    assert!(!r.play_on("nonexistent-layer", "idle", 0.0));
}

#[test]
fn a_crossfade_lands_between_the_two_clips_and_then_on_the_new_one() {
    let mut r = rig();
    let input = at(Vec2::new(500.0, 500.0));
    r.play("idle", 0.0);
    run(&mut r, &input, 10);
    r.play("sleep", 400.0);
    assert!(r.player().fading(0));
    run(&mut r, &input, 12);
    assert!(r.player().fading(0), "the fade finished far too early");
    run(&mut r, &input, 30);
    assert!(!r.player().fading(0));
    assert_eq!(r.player().current(0), r.skin().clip_index("sleep"));
}

// ---------------------------------------------------------------------------
// F2 — the click-through outline
// ---------------------------------------------------------------------------

#[test]
fn the_outline_covers_her_body_and_nothing_else() {
    let mut r = rig();
    let anchor = Vec2::new(800.0, 600.0);
    run(&mut r, &at(anchor), 5);
    let poly = r.contour(ContourOptions::default());
    assert!(!poly.is_empty());

    // Everything inside the shell is clickable.
    let shell = r.frame().shape("shell").unwrap().bounds();
    let centre = Vec2::new(
        (shell.min.x + shell.max.x) * 0.5,
        (shell.min.y + shell.max.y) * 0.5,
    );
    assert!(poly.contains(centre), "her middle is not clickable");
    for f in [0.2f32, 0.4, 0.6] {
        for dir in [
            Vec2::new(1.0, 0.0),
            Vec2::new(-1.0, 0.0),
            Vec2::new(0.0, 1.0),
            Vec2::new(0.0, -1.0),
        ] {
            let p = centre + dir * (shell.width() * 0.5 * f);
            assert!(poly.contains(p), "{p:?} is inside her but not in the outline");
        }
    }

    // The bloom around her is not.
    let far = Rect::new(
        r.frame().bounds.min - Vec2::splat(40.0),
        r.frame().bounds.max + Vec2::splat(40.0),
    );
    for p in [
        Vec2::new(far.min.x, far.min.y),
        Vec2::new(far.max.x, far.min.y),
        Vec2::new(far.min.x, far.max.y),
        Vec2::new(far.max.x, far.max.y),
        anchor + Vec2::new(600.0, 0.0),
        anchor - Vec2::new(0.0, 900.0),
    ] {
        assert!(!poly.contains(p), "{p:?} is nowhere near her but is clickable");
    }
}

#[test]
fn the_outline_stays_inside_the_two_kilobyte_budget_at_every_size() {
    let mut r = rig();
    for size in [48.0, 96.0, 128.0, 256.0, 512.0] {
        run(
            &mut r,
            &RigInput { size_px: size, anchor: Vec2::new(800.0, 600.0), ..Default::default() },
            5,
        );
        let poly = r.contour(ContourOptions::default());
        assert!(
            poly.approx_bytes() <= 2048,
            "at {size}px the outline is {} bytes ({} points)",
            poly.approx_bytes(),
            poly.points.len()
        );
        assert!(!poly.is_empty(), "no outline at {size}px");
    }
}

#[test]
fn the_outline_follows_her_as_she_animates() {
    let mut r = rig();
    let a = Vec2::new(400.0, 400.0);
    run(&mut r, &at(a), 5);
    let first = r.contour(ContourOptions::default());
    let b = Vec2::new(1200.0, 400.0);
    run(&mut r, &at(b), 60);
    let second = r.contour(ContourOptions::default());
    assert!(first.bounds().max.x < second.bounds().min.x, "the outline stayed behind");
    assert!(second.contains(b - Vec2::new(0.0, 40.0)));
}

#[test]
fn the_outline_is_integer_ready_for_the_compositor() {
    let mut r = rig();
    run(&mut r, &at(Vec2::new(700.0, 500.0)), 5);
    let poly = r.contour(ContourOptions::default());
    let ints = poly.to_i32();
    assert_eq!(ints.len(), poly.points.len());
    assert!(ints.iter().all(|(x, y)| x.abs() < 100_000 && y.abs() < 100_000));
}

// ---------------------------------------------------------------------------
// SPEC §3.1 — the governor
// ---------------------------------------------------------------------------

#[test]
fn a_downgrade_sheds_work_immediately_and_keeps_drawing() {
    let mut r = rig();
    let input = at(Vec2::new(500.0, 500.0));
    run(&mut r, &input, 30);
    assert_eq!(r.detail(), Detail::FULL);

    r.set_tier(Tier::Lobotomised, &TierReason::VrSession);
    assert_eq!(r.tier(), Tier::Lobotomised);
    assert!(!r.detail().secondary_motion, "the tail simulation should be off at T3");
    assert!(!r.detail().look_at);
    assert!(!r.detail().crossfade);

    // She still animates — F71's whole point is that cuteness survives
    // lobotomisation.
    let before = r.frame().shape("shell").unwrap().points.clone();
    run(&mut r, &input, 30);
    assert!(all_finite(r.frame()));
    let after = r.frame().shape("shell").unwrap().points.clone();
    assert_ne!(before, after, "she stopped moving entirely at T3");
}

#[test]
fn every_tier_produces_a_usable_frame_and_outline() {
    for tier in [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant] {
        let mut r = rig();
        r.set_tier(tier, &TierReason::Pinned);
        run(&mut r, &at(Vec2::new(700.0, 500.0)), 20);
        assert!(all_finite(r.frame()), "{tier:?} produced NaN geometry");
        let poly = r.contour(ContourOptions::default());
        assert!(!poly.is_empty(), "{tier:?} produced no outline");
        assert!(poly.approx_bytes() <= 2048, "{tier:?} blew the outline budget");
    }
}

#[test]
fn upgrading_back_restores_the_full_rig_without_a_jolt() {
    let mut r = rig();
    let input = at(Vec2::new(500.0, 500.0));
    run(&mut r, &input, 30);
    r.set_tier(Tier::Lobotomised, &TierReason::VrSession);
    run(&mut r, &input, 30);
    r.set_tier(Tier::Full, &TierReason::Idle);
    assert_eq!(r.detail(), Detail::FULL);
    let before = r.frame().shape("tail_b").unwrap().bounds();
    r.update(1.0 / 60.0, &input);
    let after = r.frame().shape("tail_b").unwrap().bounds();
    assert!(
        before.min.dist(after.min) < 12.0,
        "the tail snapped on the way back up: {before:?} -> {after:?}"
    );
}

#[test]
fn cost_falls_as_the_tier_drops() {
    let full = <Rig as Governed>::cost_at(Tier::Full);
    let reduced = <Rig as Governed>::cost_at(Tier::Reduced);
    let lobo = <Rig as Governed>::cost_at(Tier::Lobotomised);
    let dormant = <Rig as Governed>::cost_at(Tier::Dormant);
    assert!(full.cpu_centi_pct > reduced.cpu_centi_pct);
    assert!(reduced.cpu_centi_pct > lobo.cpu_centi_pct);
    assert_eq!(dormant.cpu_centi_pct, 0);
    // The vector rig holds no VRAM of its own; that is the renderer's to
    // account for.
    assert_eq!(full.vram_mib, 0);
}

// ---------------------------------------------------------------------------
// Budget
// ---------------------------------------------------------------------------

#[test]
fn the_per_frame_path_does_not_allocate() {
    let mut r = rig();
    let input = at(Vec2::new(500.0, 500.0));
    run(&mut r, &input, 10);
    let caps: Vec<usize> = r.frame().shapes.iter().map(|s| s.points.capacity()).collect();
    let stops: Vec<usize> = r
        .frame()
        .shapes
        .iter()
        .map(|s| match s.fill.as_ref() {
            Some(wisp_rig::Paint::Linear(g)) => g.stops.capacity(),
            Some(wisp_rig::Paint::Radial(g)) => g.stops.capacity(),
            _ => 0,
        })
        .collect();

    for i in 0..600 {
        let moving = RigInput {
            anchor: Vec2::new(500.0 + i as f32, 500.0),
            velocity: Vec2::new(60.0, (i as f32 * 0.1).sin() * 400.0),
            cursor: Some(Vec2::new(900.0, 300.0)),
            ..input
        };
        r.update(1.0 / 60.0, &moving);
    }

    let caps_after: Vec<usize> = r.frame().shapes.iter().map(|s| s.points.capacity()).collect();
    assert_eq!(caps, caps_after, "a shape's point buffer was reallocated");
    let stops_after: Vec<usize> = r
        .frame()
        .shapes
        .iter()
        .map(|s| match s.fill.as_ref() {
            Some(wisp_rig::Paint::Linear(g)) => g.stops.capacity(),
            Some(wisp_rig::Paint::Radial(g)) => g.stops.capacity(),
            _ => 0,
        })
        .collect();
    assert_eq!(stops, stops_after, "a gradient's stop list was rebuilt");
}

#[test]
fn a_thousand_frames_stay_bounded() {
    let mut r = rig();
    for i in 0..1000 {
        let t = i as f32 / 60.0;
        r.update(
            1.0 / 60.0,
            &RigInput {
                anchor: Vec2::new(500.0 + t.sin() * 300.0, 400.0 + t.cos() * 120.0),
                velocity: Vec2::new(t.cos() * 300.0, -t.sin() * 120.0),
                cursor: Some(Vec2::new(900.0, 200.0)),
                ..at(Vec2::ZERO)
            },
        );
    }
    let b = r.frame().bounds;
    assert!(b.width() < 400.0 && b.height() < 400.0, "she drifted apart: {b:?}");
    assert!(all_finite(r.frame()));
}

// ---------------------------------------------------------------------------
// F5 / F72 — physics driving the rig
// ---------------------------------------------------------------------------

#[test]
fn throwing_her_moves_the_rig_and_she_lands_and_recovers() {
    let mut r = rig();
    let params = r.skin().physics;
    let surfaces = [Surface { id: 1, y: 900.0, x0: 0.0, x1: 1920.0 }];
    let forces = Forces {
        params,
        surfaces: &surfaces,
        bounds: Some(Rect::new(Vec2::new(0.0, 0.0), Vec2::new(1920.0, 1080.0))),
        grab: None,
        wind: Vec2::ZERO,
    };

    let mut body = BodyState {
        pos: Vec2::new(200.0, 300.0),
        vel: Vec2::new(700.0, -300.0),
        ..Default::default()
    };
    r.play("thrown", 80.0);

    let mut landed = false;
    let mut hard = false;
    for _ in 0..900 {
        let step = physics::step(&body, 1.0 / 120.0, &forces);
        body = step.state;
        if let Some(PhysicsEventLanded { speed: _, hard: h }) = as_landing(step.event) {
            landed = true;
            hard |= h;
            r.play("idle", 160.0);
        }
        r.update(
            1.0 / 120.0,
            &RigInput {
                size_px: 128.0,
                anchor: body.pos,
                velocity: body.vel,
                grabbed: body.grabbed,
                on_ground: body.on_ground,
                ..Default::default()
            },
        );
        assert!(all_finite(r.frame()));
    }
    assert!(landed, "she never came down");
    assert!(hard, "a throw from that height should be a hard landing");
    assert!(body.on_ground);
    assert!(body.pos.x > 400.0, "the throw went nowhere: {:?}", body.pos);
    // The rig ends up where the body is.
    assert!(
        (r.frame().anchor - body.pos).len() < 1e-3,
        "the rig is not where she is"
    );
}

struct PhysicsEventLanded {
    #[allow(dead_code)]
    speed: f32,
    hard: bool,
}

fn as_landing(e: Option<physics::PhysicsEvent>) -> Option<PhysicsEventLanded> {
    match e {
        Some(physics::PhysicsEvent::Landed { speed, hard, .. }) => {
            Some(PhysicsEventLanded { speed, hard })
        }
        _ => None,
    }
}

#[test]
fn dragging_her_builds_a_throw_and_the_rig_follows_the_pointer() {
    let mut r = rig();
    let surfaces = [Surface { id: 1, y: 900.0, x0: 0.0, x1: 1920.0 }];
    let mut forces = Forces {
        params: PhysicsParams::default(),
        surfaces: &surfaces,
        bounds: None,
        grab: None,
        wind: Vec2::ZERO,
    };
    let mut body = BodyState::at(Vec2::new(300.0, 300.0));
    r.play("pet", 0.0);
    for i in 1..=20 {
        let pointer = Vec2::new(300.0 + i as f32 * 18.0, 300.0);
        forces.grab = Some(pointer);
        body = physics::step(&body, 1.0 / 120.0, &forces).state;
        r.update(
            1.0 / 120.0,
            &RigInput {
                anchor: body.pos,
                velocity: body.vel,
                grabbed: true,
                on_ground: false,
                ..at(Vec2::ZERO)
            },
        );
        assert_eq!(body.pos, pointer, "she is not under the pointer");
    }
    assert!(body.grabbed);
    assert!(body.vel.x > 500.0, "the drag built no throw: {:?}", body.vel);
    assert!(all_finite(r.frame()));
}

#[test]
fn snap_puts_every_simulation_where_it_belongs_after_a_teleport() {
    let mut r = rig();
    let mut x = 300.0f32;
    for _ in 0..40 {
        x += 20.0;
        r.update(1.0 / 60.0, &at(Vec2::new(x, 500.0)));
    }
    // Teleport to the other monitor.
    let far = Vec2::new(3400.0, 200.0);
    r.update(1.0 / 60.0, &at(far));
    r.snap();
    r.update(1.0 / 60.0, &at(far));
    let b = r.frame().bounds;
    assert!(
        b.contains(far - Vec2::new(0.0, 20.0)),
        "she is smeared between the two positions: {b:?}"
    );
    assert!(all_finite(r.frame()));
}
