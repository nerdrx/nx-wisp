//! Offscreen render tests. Every one of these draws into a texture and
//! asserts on read-back pixels — SPEC §4, and the pattern the M0 spike proved.
//! **Nothing here opens a window**: the operator is using this machine.
//!
//! Set `WISP_PAINT_DUMP=/some/dir` to have every case also write a PNG, which
//! is how DESIGN.md §11's "review by looking, not by reading code" gets done.

use wisp_paint::{
    adapter::AdapterPreference, atlas, geom::Rect, paint::Paint, scene::Scene, text::TextEngine,
    widget, BakeItem, Image, Painter,
};
use wisp_proto::{Governed, Tier, TierReason};
use wisp_theme::{
    component::ButtonVariant,
    palette,
    surface::{Floating, Structural},
    tokens, Color, Radius,
};

/// One painter for the whole file: bringing up Vulkan costs more than every
/// test in here put together.
fn painter() -> std::sync::MutexGuard<'static, Painter> {
    use std::sync::{Mutex, OnceLock};
    static P: OnceLock<Mutex<Painter>> = OnceLock::new();
    P.get_or_init(|| {
        // SPEC §4: no test may touch the operator's real state.
        if std::env::var_os("NX_WISP_CONFIG_DIR").is_none() {
            let dir = std::env::temp_dir().join(format!("nx-wisp-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok();
            std::env::set_var("NX_WISP_CONFIG_DIR", &dir);
        }
        Mutex::new(
            Painter::new(AdapterPreference::HighPerformance)
                .expect("a Vulkan adapter — this crate is Vulkan-only by SPEC §1"),
        )
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

fn draw(p: &mut Painter, w: u32, h: u32, name: &str, build: impl FnOnce(&mut Scene)) -> Image {
    let target = p.offscreen(w, h).unwrap();
    let mut scene = Scene::new();
    build(&mut scene);
    p.render(&target, &scene).unwrap();
    let img = p.read(&target).unwrap();
    dump(&img, name);
    img
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

fn close(a: Color, b: Color, tol: u8) -> bool {
    let d = |x: u8, y: u8| (x as i32 - y as i32).unsigned_abs() as u8;
    d(a.r, b.r) <= tol && d(a.g, b.g) <= tol && d(a.b, b.b) <= tol && d(a.a, b.a) <= tol
}

// ---------------------------------------------------------------- the basics

#[test]
fn a_violet_rounded_rect_is_exactly_violet() {
    let mut p = painter();
    let img = draw(&mut p, 128, 128, "violet_rect", |s| {
        s.fill_rect(Rect::new(16.0, 16.0, 96.0, 96.0), Radius::CARD, palette::VIOLET);
    });
    // #7700FF comes back byte for byte: the target is Rgba8Unorm and the paint
    // is opaque, so there is no colour management in the way.
    assert_eq!(img.pixel(64, 64), [0x77, 0x00, 0xff, 0xff]);
    assert_eq!(img.color(64, 64), palette::VIOLET);
    assert_eq!(img.color(20, 20), palette::VIOLET);
}

#[test]
fn nothing_drawn_stays_fully_transparent() {
    let mut p = painter();
    let img = draw(&mut p, 64, 64, "empty", |_| {});
    assert!(img.data.iter().all(|b| *b == 0), "an empty scene must be entirely transparent");

    // …and around a shape, too — this is what makes the layer surface
    // click-through everywhere she is not.
    let img = draw(&mut p, 128, 128, "transparent_margin", |s| {
        s.fill_rect(Rect::new(48.0, 48.0, 32.0, 32.0), Radius::SM, palette::VIOLET);
    });
    for (x, y) in [(0, 0), (127, 0), (0, 127), (127, 127), (10, 64), (64, 10)] {
        assert_eq!(img.pixel(x, y), [0, 0, 0, 0], "({x},{y}) should be untouched");
    }
    assert_eq!(img.alpha(64, 64), 255);
}

#[test]
fn the_corner_radius_is_the_radius_that_was_asked_for() {
    let mut p = painter();
    // A 6px radius on a rect starting at (0,0): the pixel at (1,1) is outside
    // the arc and the pixel at (6,6) is inside it. At 3px, (3,3) is inside.
    for (radius, outside, inside) in
        [(Radius::CARD, (1u32, 1u32), (6u32, 6u32)), (Radius::XS, (0, 0), (3, 3))]
    {
        let img = draw(&mut p, 64, 64, &format!("radius_{}", radius.px_u8()), |s| {
            s.fill_rect(Rect::new(0.0, 0.0, 64.0, 64.0), radius, palette::VIOLET);
        });
        assert!(
            img.alpha(outside.0, outside.1) < 128,
            "radius {}: {outside:?} should be outside the corner arc (alpha {})",
            radius.px_u8(),
            img.alpha(outside.0, outside.1)
        );
        assert!(
            img.alpha(inside.0, inside.1) > 200,
            "radius {}: {inside:?} should be inside (alpha {})",
            radius.px_u8(),
            img.alpha(inside.0, inside.1)
        );
        // Every corner is cut, not just the first one.
        assert!(img.alpha(63, 63) < 200 || radius.px_u8() < 4);
        assert_eq!(img.alpha(32, 32), 255, "the middle is solid");
    }
}

#[test]
fn a_larger_radius_removes_more_of_the_corner() {
    let mut p = painter();
    let corner_alpha = |p: &mut Painter, r: Radius| -> u32 {
        let img = draw(p, 32, 32, "corner", |s| {
            s.fill_rect(Rect::from_size(32.0, 32.0), r, palette::VIOLET);
        });
        (0..8).flat_map(|y| (0..8).map(move |x| (x, y))).map(|(x, y)| img.alpha(x, y) as u32).sum()
    };
    let sharp = corner_alpha(&mut p, Radius::XS);
    let soft = corner_alpha(&mut p, Radius::CARD);
    assert!(soft < sharp, "6px must eat more corner than 3px: {soft} vs {sharp}");
}

// ------------------------------------------------------------------ gradients

#[test]
fn a_linear_gradient_actually_varies_along_its_axis() {
    let mut p = painter();
    let img = draw(&mut p, 128, 128, "gradient_linear", |s| {
        s.fill_rect(Rect::from_size(128.0, 128.0), Radius::XS, Paint::token(tokens::FIELD));
    });
    let top = img.color(64, 2);
    let mid = img.color(64, 64);
    let bottom = img.color(64, 125);
    assert_ne!(top, bottom, "the gradient did not vary");
    assert_ne!(mid, top);
    // --bg-top is darker than --bg-bottom, and the ramp is monotone.
    assert!(top.luminance() < bottom.luminance());
    assert!(mid.luminance() > top.luminance() && mid.luminance() < bottom.luminance());
    // The endpoints land on the token's own colours.
    assert!(close(top, palette::BG_TOP, 3), "top was {top:?}, wanted {:?}", palette::BG_TOP);
    assert!(close(bottom, palette::BG_BOTTOM, 3));
}

#[test]
fn a_gradient_at_157_degrees_is_brightest_in_the_upper_left() {
    let mut p = painter();
    let img = draw(&mut p, 128, 128, "gradient_lit", |s| {
        s.fill_rect(Rect::from_size(128.0, 128.0), Radius::CARD, Paint::token(tokens::SURFACE_1));
    });
    let tl = img.color(10, 10).luminance();
    let br = img.color(118, 118).luminance();
    assert!(tl > br, "light must come from the upper-left: {tl} vs {br}");
    // …and it must vary along the diagonal, not just at the ends.
    let mid = img.color(64, 64).luminance();
    assert!(mid < tl && mid > br);
}

#[test]
fn a_radial_gradient_falls_off_from_its_centre() {
    let mut p = painter();
    let img = draw(&mut p, 128, 128, "gradient_radial", |s| {
        let paint = Paint::Radial {
            cx: 0.5,
            cy: 0.5,
            rx: 0.5,
            ry: 0.5,
            stops: vec![
                wisp_paint::paint::Stop { at: 0.0, color: palette::CYAN },
                wisp_paint::paint::Stop { at: 1.0, color: palette::CYAN.with_alpha(0.0) },
            ],
        };
        s.fill_rect(Rect::from_size(128.0, 128.0), Radius::XS, paint);
    });
    assert!(img.alpha(64, 64) > 240, "the centre should be near-opaque");
    assert!(img.alpha(64, 64) > img.alpha(64, 96));
    assert!(img.alpha(64, 96) > img.alpha(64, 126));
}

#[test]
fn the_multi_stop_hairline_is_bright_in_the_middle_and_gone_at_the_ends() {
    let mut p = painter();
    let img = draw(&mut p, 128, 8, "hairline", |s| {
        s.hairline(Rect::new(0.0, 4.0, 128.0, 1.0));
    });
    let a = |x: u32| (0..8).map(|y| img.alpha(x, y) as u32).sum::<u32>();
    assert!(a(64) > a(4), "the hairline must be brightest in the middle");
    assert!(a(2) < 8, "it must fade to nothing at the ends");
    assert!(a(126) < 8);
}

// ---------------------------------------------------------------- the language

#[test]
fn a_card_is_opaque_everywhere_and_carries_a_lit_edge() {
    let mut p = painter();
    let img = draw(&mut p, 128, 96, "card", |s| {
        s.structural(Rect::new(8.0, 8.0, 112.0, 80.0), Structural::Card);
    });
    // v1.5: structural surfaces are opaque elevation steps.
    for (x, y) in [(64u32, 48u32), (16, 16), (110, 80)] {
        assert_eq!(img.alpha(x, y), 255, "a card must be opaque at ({x},{y})");
    }
    // The 1px edge is brighter than the fill just inside it, top-left.
    let edge = img.color(9, 24).luminance();
    let fill = img.color(20, 24).luminance();
    assert!(edge > fill, "the top-left edge should be lit: {edge} vs {fill}");
    // …and darker than the fill at the bottom-right.
    let dark_edge = img.color(118, 70).luminance();
    let near = img.color(108, 70).luminance();
    assert!(dark_edge < near, "the bottom-right edge should be shadowed");
}

#[test]
fn a_floating_layer_blurs_what_is_behind_it_and_still_hides_it() {
    let mut p = painter();
    let img = draw(&mut p, 160, 160, "floating_sheet", |s| {
        // A hard-edged violet/cyan checker underneath, so a blur is obvious.
        for i in 0..8 {
            for j in 0..8 {
                let c = if (i + j) % 2 == 0 { palette::VIOLET } else { palette::CYAN };
                s.fill_rect(
                    Rect::new(i as f32 * 20.0, j as f32 * 20.0, 20.0, 20.0),
                    Radius::XS,
                    c,
                );
            }
        }
        s.floating(Rect::new(30.0, 30.0, 100.0, 100.0), Floating::Sheet);
    });

    // Inside the sheet the checker must be gone: sample a run of pixels that
    // straddles two checker cells and check the variance has collapsed.
    let inside: Vec<f32> = (45..115).step_by(5).map(|x| img.color(x, 80).luminance()).collect();
    let spread = inside.iter().cloned().fold(f32::MIN, f32::max)
        - inside.iter().cloned().fold(f32::MAX, f32::min);
    assert!(spread < 0.02, "the ≥0.85 body should hide the checker; spread was {spread}");

    // Outside it, the checker is untouched.
    assert_eq!(img.color(10, 10), palette::VIOLET);
    assert_eq!(img.color(30, 10), palette::CYAN);

    // The sheet is opaque enough to read text on.
    assert!(img.alpha(80, 80) > 250);
    // And it is dark — a glass body is a dark surface, not a light one.
    assert!(img.color(80, 80).luminance() < 0.1);
}

#[test]
fn the_backdrop_blur_really_blurs() {
    // `floating` covers its own blur with a ≥0.85 body, which is the point of
    // v1.5 — so to prove the blur pipeline itself works, draw the backdrop
    // back without a body over it.
    use wisp_paint::scene::Cmd;
    let mut p = painter();
    let img = draw(&mut p, 128, 128, "blur_only", |s| {
        for i in 0..8 {
            for j in 0..8 {
                let c = if (i + j) % 2 == 0 { palette::VIOLET } else { palette::CYAN };
                s.fill_rect(Rect::new(i as f32 * 16.0, j as f32 * 16.0, 16.0, 16.0), Radius::XS, c);
            }
        }
        s.push(Cmd::BlurBackdrop { blur: wisp_theme::Blur::SHEET });
        s.push(Cmd::Backdrop { rect: Rect::new(32.0, 32.0, 64.0, 64.0), radius: Radius::CARD });
    });

    // Inside the blurred patch the checker's hard edges are gone: adjacent
    // cells that were pure violet and pure cyan now read as one muddle.
    let a = img.color(44, 64);
    let b = img.color(60, 64);
    assert!(
        (a.r as i32 - b.r as i32).abs() < 40 && (a.g as i32 - b.g as i32).abs() < 40,
        "the blur did not run: {a:?} vs {b:?}"
    );
    // Outside it the checker is still crisp.
    assert_eq!(img.color(8, 8), palette::VIOLET);
    assert_eq!(img.color(24, 8), palette::CYAN);
}

#[test]
fn the_blur_is_a_budget_the_painter_counts() {
    let mut p = painter();
    let target = p.offscreen(64, 64).unwrap();
    let mut s = Scene::new();
    s.structural(Rect::from_size(64.0, 64.0), Structural::Card);
    p.render(&target, &s).unwrap();
    assert_eq!(p.last_blur_count(), 0, "a card fakes it — §4's cardinal rule");

    let mut s = Scene::new();
    s.floating(Rect::new(4.0, 4.0, 56.0, 24.0), Floating::Toast);
    s.floating(Rect::new(4.0, 34.0, 56.0, 24.0), Floating::Menu);
    p.render(&target, &s).unwrap();
    assert_eq!(p.last_blur_count(), 2);
    assert!(p.last_blur_count() <= wisp_theme::surface::MAX_SIMULTANEOUS_BLURS);
}

#[test]
fn clipping_actually_clips() {
    let mut p = painter();
    let img = draw(&mut p, 64, 64, "clip", |s| {
        s.push_clip(Rect::new(0.0, 0.0, 32.0, 64.0));
        s.fill_rect(Rect::from_size(64.0, 64.0), Radius::XS, palette::VIOLET);
        s.pop_clip();
    });
    assert_eq!(img.alpha(10, 32), 255);
    assert_eq!(img.alpha(50, 32), 0, "everything right of the clip must be untouched");
}

// --------------------------------------------------------------------- text

#[test]
fn text_produces_non_empty_coverage_in_the_right_place() {
    let mut p = painter();
    let mut text = TextEngine::new();
    let style = wisp_theme::typography::TITLE;
    let run = text.run(&p, "Wisp", &style, None);
    assert!(run.width > 0.0 && run.height > 0.0, "the run measured to nothing");

    let rect = run.rect_at(8.0, 8.0);
    let img = draw(&mut p, 160, 64, "text", |s| {
        s.text(rect, run.tex.clone(), palette::TEXT);
    });

    let lit = img.data.chunks(4).filter(|px| px[3] > 32).count();
    assert!(lit > 40, "only {lit} pixels of coverage — the glyphs did not rasterise");

    // The ink lands inside the run's box and nowhere else.
    let mut min_x = u32::MAX;
    let mut max_x = 0;
    for y in 0..img.h {
        for x in 0..img.w {
            if img.alpha(x, y) > 32 {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
            }
        }
    }
    assert!(min_x >= 6, "ink started at {min_x}, left of the layout origin");
    assert!(
        (max_x as f32) <= 8.0 + run.width + 4.0,
        "ink ran to {max_x}, past the measured advance {}",
        run.width
    );

    // It is tinted with --text, not left white.
    let sample = (0..img.h)
        .flat_map(|y| (0..img.w).map(move |x| (x, y)))
        .find(|(x, y)| img.alpha(*x, *y) > 240)
        .map(|(x, y)| img.color(x, y));
    if let Some(c) = sample {
        // Only the hue matters: a glyph edge is partially covered by design,
        // so its alpha is whatever the rasteriser decided.
        assert!(
            close(c.with_alpha(1.0), palette::TEXT, 6),
            "glyph colour was {c:?}, wanted --text"
        );
    }
}

#[test]
fn a_shorter_string_covers_fewer_pixels() {
    let mut p = painter();
    let mut text = TextEngine::new();
    let style = wisp_theme::typography::BODY;
    let ink = |p: &mut Painter, t: &mut TextEngine, s: &str| {
        let run = t.run(p, s, &style, None);
        let rect = run.rect_at(2.0, 2.0);
        let img = draw(p, 256, 32, "ink", |sc| {
            sc.text(rect, run.tex.clone(), palette::TEXT);
        });
        img.data.chunks(4).filter(|px| px[3] > 32).count()
    };
    let short = ink(&mut p, &mut text, "hi");
    let long = ink(&mut p, &mut text, "hi there, operator");
    assert!(long > short * 2, "{long} vs {short}");
}

#[test]
fn the_text_cache_returns_the_same_texture_for_the_same_run() {
    let p = painter();
    let mut text = TextEngine::new();
    let style = wisp_theme::typography::BODY;
    let a = text.run(&p, "Senses", &style, None);
    let b = text.run(&p, "Senses", &style, None);
    assert_eq!(a.tex, b.tex);
    assert_eq!(text.cached_runs(), 1);
    let c = text.run(&p, "Senses", &wisp_theme::typography::TITLE, None);
    assert_ne!(a.tex, c.tex, "a different size is a different raster");
    assert_eq!(text.cached_runs(), 2);
}

// ------------------------------------------------------------- sprite atlas

#[test]
fn the_sprite_path_reproduces_the_vector_path() {
    let mut p = painter();
    const W: u32 = 96;
    const H: u32 = 64;

    let build = |s: &mut Scene| {
        s.structural(Rect::new(4.0, 4.0, 88.0, 56.0), Structural::Card);
        s.fill_rect(Rect::new(16.0, 16.0, 32.0, 32.0), Radius::SM, palette::VIOLET);
        s.fill_rect(Rect::new(56.0, 20.0, 24.0, 24.0), Radius::XS, Paint::token(tokens::SHEEN));
    };

    // 1. straight through the vector path, at the tier the bake will use
    p.set_tier(Tier::Full, &TierReason::Idle);
    let vector = draw(&mut p, W, H, "atlas_vector", build);

    // 2. baked once, then drawn as a textured quad. `bake` forces the vector
    //    path at T1, so the atlas slot holds exactly the image above.
    let mut baked = Scene::new();
    build(&mut baked);
    let atlas = atlas::bake(&mut p, 256, 128, vec![BakeItem::new("idle", W, H, baked)]).unwrap();
    assert_eq!(atlas.len(), 1);
    assert!(atlas.occupancy() > 0.0 && atlas.occupancy() < 1.0);

    // Drawn at T3, where sprites are actually used: 1× and 1:1, so every tap
    // lands on a texel centre and the round-trip is lossless.
    p.set_tier(Tier::Lobotomised, &TierReason::VrSession);
    let sprite = draw(&mut p, W, H, "atlas_sprite", |s| {
        atlas.draw(s, "idle", 0.0, 0.0).unwrap();
    });
    assert_eq!(p.last_draw_calls(), 2, "one quad and the resolve — nothing else at T3");
    p.set_tier(Tier::Full, &TierReason::Idle);

    let mean = vector.mean_abs_diff(&sprite);
    let max = vector.max_abs_diff(&sprite);
    assert!(mean < 1.0, "the sprite path drifted from the vector path: mean {mean}");
    assert!(max <= 4, "worst-pixel difference was {max}");
}

#[test]
fn baking_works_even_when_the_governor_has_already_pulled_her_down_to_t3() {
    let mut p = painter();
    p.set_tier(Tier::Lobotomised, &TierReason::HeavyProcess { name: "a game".into() });

    let mut scene = Scene::new();
    scene.fill_rect(Rect::from_size(32.0, 32.0), Radius::SM, palette::CYAN);
    let atlas = atlas::bake(&mut p, 64, 64, vec![BakeItem::new("dot", 32, 32, scene)]).unwrap();

    // The tier survives the bake…
    assert_eq!(p.tier(), Tier::Lobotomised);
    // …and the sprite is drawable at T3, which is the whole point.
    let img = draw(&mut p, 32, 32, "t3_sprite", |s| {
        atlas.draw(s, "dot", 0.0, 0.0).unwrap();
    });
    assert!(close(img.color(16, 16), palette::CYAN, 2), "got {:?}", img.color(16, 16));
    p.set_tier(Tier::Full, &TierReason::Idle);
}

#[test]
fn a_missing_sprite_is_an_error_not_a_blank_frame() {
    let mut p = painter();
    let atlas = atlas::bake(&mut p, 64, 64, vec![]).unwrap();
    let mut s = Scene::new();
    assert!(atlas.draw(&mut s, "nope", 0.0, 0.0).is_err());
    assert!(s.is_empty());
}

// ----------------------------------------------------------------- the tiers

/// Restores the shared painter's tier even if the test panics.
///
/// `painter()` hands out a static behind a mutex, so a test that panics between
/// `set_tier(Lobotomised)` and setting it back leaves every LATER test drawing
/// at T3. That is how one real disagreement in this test turned into three
/// failures elsewhere and sent me looking in the wrong place.
struct RestoreTier;
impl Drop for RestoreTier {
    fn drop(&mut self) {
        // Declared before the guard in the test, so it drops after it — the
        // lock is free by the time we get here, and `painter()` recovers from
        // poisoning.
        painter().set_tier(Tier::Full, &TierReason::Idle);
    }
}

#[test]
fn t3_draws_sprites_and_refuses_everything_else() {
    let _restore = RestoreTier;
    let mut p = painter();
    let mut scene = Scene::new();
    scene.fill_rect(Rect::from_size(24.0, 24.0), Radius::SM, palette::CYAN);
    let atlas = atlas::bake(&mut p, 64, 64, vec![BakeItem::new("dot", 24, 24, scene)]).unwrap();

    let target = p.offscreen(64, 64).unwrap();
    let mut s = Scene::new();
    // A FLAT solid fill, which survives (see below); a glass toast, whose blur
    // and gradients must be shed; and a sprite, which must draw.
    s.fill_rect(Rect::new(32.0, 32.0, 32.0, 32.0), Radius::CARD, palette::VIOLET);
    s.floating(Rect::new(0.0, 32.0, 32.0, 32.0), Floating::Toast);
    atlas.draw(&mut s, "dot", 0.0, 0.0).unwrap();

    p.set_tier(Tier::Lobotomised, &TierReason::VrSession);
    p.render(&target, &s).unwrap();
    let img = p.read(&target).unwrap();
    dump(&img, "tier_t3");
    assert!(close(img.color(12, 12), palette::CYAN, 2), "the sprite must still draw");
    // T3's promise is "no compute passes, nothing on the discrete GPU" — it was
    // never "nothing at all". Flat solid fills, strokes and text are a handful
    // of trivial draw calls on the integrated GPU, which is the whole point of
    // moving her there. This matters because T3 is exactly when a warning
    // counts: an NX Sentry alarm arrives while the operator is in a headset,
    // and wisp-attn already lets Alarm through at T3. The painter used to
    // discard the resulting bubble, so she raised the alarm to nobody.
    assert_eq!(img.alpha(48, 48), 255, "a flat solid fill still draws at T3");
    assert_eq!(p.last_blur_count(), 0, "T3 spends no blurs");
    assert_eq!(p.ssaa(), 1, "T3 does not supersample");

    // T4: nothing at all, but still a transparent frame.
    p.set_tier(Tier::Dormant, &TierReason::PowerCritical);
    p.render(&target, &s).unwrap();
    let img = p.read(&target).unwrap();
    dump(&img, "tier_t4");
    assert!(img.data.iter().all(|b| *b == 0), "T4 must draw nothing at all");

    p.set_tier(Tier::Full, &TierReason::Idle);
    p.render(&target, &s).unwrap();
    let img = p.read(&target).unwrap();
    assert_eq!(img.alpha(48, 48), 255, "and it all comes back on the way up");
    assert_eq!(p.last_blur_count(), 1);
}

#[test]
fn the_frame_budget_follows_the_tier_ladder() {
    let mut p = painter();
    for (tier, fps) in [
        (Tier::Feral, 60),
        (Tier::Full, 60),
        (Tier::Reduced, 30),
        (Tier::Lobotomised, 15),
    ] {
        p.set_tier(tier, &TierReason::Idle);
        let d = p.frame_interval().unwrap();
        assert!(
            (d.as_secs_f64() - 1.0 / fps as f64).abs() < 1e-9,
            "{tier:?} wanted {fps}fps, got {d:?}"
        );
    }
    p.set_tier(Tier::Dormant, &TierReason::Pinned);
    assert_eq!(p.frame_interval(), None);
    p.set_tier(Tier::Full, &TierReason::Idle);
}

#[test]
fn dropping_a_tier_does_not_change_what_is_drawn_only_how_finely() {
    let mut p = painter();
    let build = |s: &mut Scene| {
        s.structural(Rect::new(4.0, 4.0, 56.0, 56.0), Structural::Card);
        s.fill_rect(Rect::new(16.0, 16.0, 32.0, 32.0), Radius::CARD, palette::VIOLET);
    };
    p.set_tier(Tier::Full, &TierReason::Idle);
    let hi = draw(&mut p, 64, 64, "tier_t1", build);
    assert_eq!(p.ssaa(), 2);

    p.set_tier(Tier::Reduced, &TierReason::GpuPressure { busy_pct: 80 });
    let lo = draw(&mut p, 64, 64, "tier_t2", build);
    assert_eq!(p.ssaa(), 1);

    // Same picture; only the corner antialiasing differs.
    assert_eq!(hi.color(32, 32), lo.color(32, 32));
    assert!(hi.mean_abs_diff(&lo) < 4.0, "T2 changed the drawing, not just the sampling");
    p.set_tier(Tier::Full, &TierReason::Idle);
}

// ------------------------------------------------------------- widget layer

#[test]
fn a_laid_out_panel_paints_where_layout_said_it_would() {
    let mut p = painter();
    let mut text = TextEngine::new();
    let mut ui = widget::Ui::new();

    let content = widget::card(&mut ui, widget::Size::FILL);
    widget::label(&mut ui, content, "Senses", wisp_theme::Role::TitleSmall);
    widget::paragraph(&mut ui, content, "Ambient senses run unprompted.");
    let btn = widget::button(&mut ui, content, "Configure", ButtonVariant::Primary);

    let bounds = Rect::new(8.0, 8.0, 240.0, 160.0);
    ui.layout(&mut text, bounds);

    let target = p.offscreen(256, 176).unwrap();
    let mut scene = Scene::new();
    ui.paint(&p, &mut text, &mut scene);
    p.render(&target, &scene).unwrap();
    let img = p.read(&target).unwrap();
    dump(&img, "widget_panel");

    // The card fills its bounds and nothing outside them.
    assert_eq!(img.alpha(128, 88), 255);
    assert_eq!(img.pixel(2, 2), [0, 0, 0, 0]);
    assert_eq!(img.pixel(253, 173), [0, 0, 0, 0]);

    // The button sits inside the card's padding and is violet-led.
    let r = ui.node(btn).layout;
    assert!(r.x >= bounds.x + wisp_theme::Insets::CARD.left.get() - 0.01);
    let c = img.color(r.centre().x as u32, r.centre().y as u32);
    assert!(c.b > c.g, "the primary button should read violet, got {c:?}");
}

#[test]
fn a_scroll_container_clips_its_overflow() {
    let mut p = painter();
    let mut text = TextEngine::new();
    let mut ui = widget::Ui::new();

    let sc = ui.add(widget::Widget::Scroll { offset: 0.0 }, widget::Size::FILL);
    let inner = ui.add_to(
        sc,
        widget::Widget::Fill { paint: Paint::solid(palette::VIOLET), radius: Radius::XS },
        widget::Size::fixed(64.0, 200.0),
    );
    let _ = inner;
    ui.layout(&mut text, Rect::new(0.0, 0.0, 64.0, 64.0));

    let target = p.offscreen(64, 128).unwrap();
    let mut scene = Scene::new();
    ui.paint(&p, &mut text, &mut scene);
    p.render(&target, &scene).unwrap();
    let img = p.read(&target).unwrap();
    dump(&img, "widget_scroll");

    assert_eq!(img.alpha(32, 32), 255, "inside the viewport");
    assert_eq!(img.alpha(32, 100), 0, "below the viewport must be clipped away");
}

#[test]
fn a_disabled_button_is_visibly_dimmer() {
    let mut p = painter();
    let mut text = TextEngine::new();

    let render = |p: &mut Painter, text: &mut TextEngine, disabled: bool| -> Image {
        let mut ui = widget::Ui::new();
        let root = ui.add(
            widget::Widget::Stack {
                axis: widget::Axis::Column,
                gap: wisp_theme::Space::ZERO,
                padding: wisp_theme::Insets::ZERO,
                align: widget::Align::Start,
            },
            widget::Size::FILL,
        );
        let b = widget::button(&mut ui, root, "Enable", ButtonVariant::Primary);
        if let widget::Widget::Button { state, .. } = &mut ui.node_mut(b).widget {
            state.disabled = disabled;
        }
        ui.layout(text, Rect::new(4.0, 4.0, 180.0, 48.0));
        let target = p.offscreen(192, 56).unwrap();
        let mut scene = Scene::new();
        ui.paint(p, text, &mut scene);
        p.render(&target, &scene).unwrap();
        let img = p.read(&target).unwrap();
        dump(&img, if disabled { "button_disabled" } else { "button_enabled" });
        img
    };

    let on = render(&mut p, &mut text, false);
    let off = render(&mut p, &mut text, true);
    let ink = |i: &Image| i.data.chunks(4).map(|px| px[3] as u64).sum::<u64>();
    assert!(ink(&off) < ink(&on), "40% opacity must show: {} vs {}", ink(&off), ink(&on));
    assert!(ink(&off) > 0, "…but it is dimmed, not hidden");
}

#[test]
fn the_whole_language_renders_without_a_window() {
    // The review sheet of DESIGN.md §11, as one frame. Dump it with
    // WISP_PAINT_DUMP and look at it; assert only that it is coherent.
    let mut p = painter();
    let mut text = TextEngine::new();
    let img = {
        let target = p.offscreen(320, 240).unwrap();
        let mut s = Scene::new();
        s.fill_rect(Rect::from_size(320.0, 240.0), Radius::XS, Paint::token(tokens::FIELD));
        s.fill_rect(Rect::from_size(320.0, 240.0), Radius::XS, Paint::token(tokens::NEBULA_VIOLET));
        s.fill_rect(Rect::from_size(320.0, 240.0), Radius::XS, Paint::token(tokens::NEBULA_CYAN));
        s.structural(Rect::new(16.0, 16.0, 288.0, 96.0), Structural::Card);
        s.recessed(Rect::new(32.0, 64.0, 256.0, 32.0), wisp_theme::Recessed::Well);
        s.hairline(Rect::new(32.0, 56.0, 256.0, 1.0));
        let run = text.run(&p, "She costs nothing when it matters.", &wisp_theme::typography::BODY, None);
        s.text(run.rect_at(32.0, 32.0), run.tex.clone(), palette::TEXT);
        s.circle(
            wisp_paint::Point::new(292.0, 36.0),
            wisp_theme::component::Status::Live.dot(),
            palette::CYAN,
        );
        s.floating(Rect::new(48.0, 136.0, 224.0, 80.0), Floating::Bubble);
        let run = text.run(&p, "Hello.", &wisp_theme::typography::TITLE, None);
        s.text(run.rect_at(72.0, 160.0), run.tex.clone(), palette::TEXT);
        p.render(&target, &s).unwrap();
        p.read(&target).unwrap()
    };
    dump(&img, "review_sheet");

    // Field is opaque everywhere; nothing is accidentally see-through.
    assert!((0..320).step_by(17).all(|x| img.alpha(x, 8) == 255));
    // The card is above the field and lighter than it.
    assert!(img.color(160, 24).luminance() > img.color(160, 8).luminance());
    // The well is darker than the card it is cut into.
    assert!(img.color(160, 80).luminance() < img.color(160, 24).luminance());
    // The bubble is a floating layer and hides the field behind it.
    assert!(img.alpha(160, 180) == 255);
    assert_eq!(p.last_blur_count(), 1);
}
