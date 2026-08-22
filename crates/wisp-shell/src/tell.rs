//! SPEC §0.3's visible tell: **nothing invisible.**
//!
//! > Mic, clipboard and screen access have a visible tell on the character
//! > herself whenever they are live, and every use is recorded.
//!
//! This is the "visible" half. It is a pure scene builder — no clock, no state,
//! no GPU — that turns `(SenseId, active, anchor, size, phase)` into the
//! commands for a badge pinned to her silhouette.
//!
//! ## Rules that shape it
//!
//! **It has to survive every tier.** A tell that the governor can shed is not a
//! tell, so this draws nothing a T3 sprite-only frame could not: solid fills
//! and stroked geometry, no gradients, no blur, no specular, no text. That also
//! means it is bakeable by `wisp_paint::atlas` if the T3 path ever needs it,
//! and it means [`build`] takes no `Tier` — there is no tier at which the
//! answer changes.
//!
//! **It must not read as an expression.** She is a round, soft creature
//! (SPEC §3.5b explicitly exempts her from the geometry rule). The tell is the
//! opposite: an opaque, hard-cut chip in DESIGN.md's chrome language, with a
//! 3px radius and a 1px signal outline. Nothing she can *do* looks like that,
//! so a tell can never be mistaken for a mood.
//!
//! **It sits on her, not next to her.** The badges are laid out on her own
//! silhouette rather than beside it, so they cannot be pushed off the output
//! when she roams into a corner, and so "on the character herself" is literally
//! true rather than approximately true.
//!
//! **The colour is `--danger`.** DESIGN.md §1 keeps `#ff5470` for danger only,
//! and a live microphone is the one thing in this product that can actually
//! hurt the operator. The three senses are told apart by their **glyph**, not
//! by hue — an operator who cannot distinguish red from amber still reads the
//! difference between a level meter, a clipboard and a capture reticle.

use wisp_paint::geom::{Path, Point, Rect};
use wisp_paint::paint::Paint;
use wisp_paint::scene::Scene;
use wisp_proto::{Consent, SenseId};
use wisp_theme::{palette, Radius};

/// Badge edge as a fraction of her drawn size, then clamped: she can be shed
/// down to a very small sprite and the tell still has to be readable.
const BADGE_FRACTION: f32 = 0.17;
const BADGE_MIN_PX: f32 = 16.0;
const BADGE_MAX_PX: f32 = 28.0;
/// Inset from her silhouette, and the gap between stacked badges.
const PAD_FRACTION: f32 = 0.30;
/// How far the pulse halo reaches at its widest. Also capped at half the gap
/// between slots, so three live senses read as three chips rather than as one
/// pink slab.
const HALO_PX: f32 = 5.0;
const HALO_ALPHA: f32 = 0.42;
/// Glyph stroke, in DESIGN.md §8's 1.5–2px band.
const STROKE_PX: f32 = 1.75;

/// The colour of "this is live and it is invasive".
pub const TELL_COLOR: wisp_theme::Color = palette::DANGER;
/// The chip the glyph sits on. Opaque, because §0.3 says the operator must be
/// able to see it — over any desktop, at any moment, with no exceptions.
pub const PLATE_COLOR: wisp_theme::Color = palette::BG_TOP;

/// Which row a sense takes when several are live at once. Fixed per sense, so
/// the microphone is always in the same place and the operator learns it.
///
/// `None` for every sense that is not [`Consent::Invasive`] — SPEC §3.7 gives
/// the tell to mic, clipboard and screen and to nothing else. A tell on an
/// ambient sense would teach the operator to ignore tells.
pub fn slot(sense: SenseId) -> Option<u8> {
    match sense {
        SenseId::Microphone => Some(0),
        SenseId::Clipboard => Some(1),
        SenseId::Screen => Some(2),
        _ => None,
    }
}

/// Her silhouette: `size` square, centred on `anchor`.
fn her_box(anchor: Point, size: f32) -> Rect {
    let s = size.max(0.0);
    Rect::new(anchor.x - s * 0.5, anchor.y - s * 0.5, s, s)
}

/// Where the badge for `sense` lands, in surface pixels. `None` when the sense
/// has no tell to show.
///
/// The three slots form one column down her right side, and the column is
/// **centred on her box** rather than hung from its top: a slot's position is
/// then a function of her alone, so turning the microphone off never moves the
/// clipboard badge. Below roughly 60px of character the badges hold their
/// minimum readable size and the column overhangs her silhouette by a few
/// pixels at each end — legibility wins, and the layer surface carries padding
/// around her for exactly this kind of overhang.
pub fn badge_rect(sense: SenseId, anchor: Point, size: f32) -> Option<Rect> {
    let slot = slot(sense)? as f32;
    let her = her_box(anchor, size);
    let b = (size * BADGE_FRACTION).clamp(BADGE_MIN_PX, BADGE_MAX_PX);
    let pad = b * PAD_FRACTION;
    let n = INVASIVE.len() as f32;
    let column = n * b + (n - 1.0) * pad;
    let top = her.centre().y - column * 0.5;
    Some(Rect::new(her.right() - pad - b, top + slot * (b + pad), b, b))
}

/// The pulse, as a triangle wave. `phase` is a 0..1 position the host advances;
/// this module owns no clock, exactly like `wisp-attn`.
fn pulse(phase: f32) -> f32 {
    if !phase.is_finite() {
        return 0.0;
    }
    let p = phase.rem_euclid(1.0);
    1.0 - (2.0 * p - 1.0).abs()
}

/// Emit the tell. Draws nothing when `active` is false, and nothing for a sense
/// that is not [`Consent::Invasive`].
pub fn build(sense: SenseId, active: bool, anchor: Point, size: f32, phase: f32, scene: &mut Scene) {
    if !active || sense.consent() != Consent::Invasive {
        return;
    }
    let Some(r) = badge_rect(sense, anchor, size) else { return };
    if r.is_empty() {
        return;
    }
    let p = pulse(phase);

    // 1. The breath. A solid halo behind the chip, growing and fading with the
    //    phase — it is the only moving part, and it is what catches the eye in
    //    peripheral vision.
    if p > 0.0 {
        let reach = HALO_PX.min(r.w * PAD_FRACTION * 0.5);
        scene.fill_rect(
            r.inset(-(1.0 + reach * p)),
            Radius::SM,
            Paint::solid(TELL_COLOR.with_alpha(HALO_ALPHA * p)),
        );
    }

    // 2. The chip: opaque, hard-cut, unmistakably chrome rather than creature.
    scene.fill_rect(r, Radius::XS, Paint::solid(PLATE_COLOR));
    scene.stroke(
        Path::rounded_rect(r.inset(0.5), Radius::XS),
        Paint::solid(TELL_COLOR),
        1.0,
    );

    // 3. The glyph, which is what says *which* sense.
    glyph(sense, r.inset(r.w * 0.22), scene);
}

/// Convenience for callers that just want the commands.
pub fn scene(sense: SenseId, active: bool, anchor: Point, size: f32, phase: f32) -> Scene {
    let mut s = Scene::new();
    build(sense, active, anchor, size, phase, &mut s);
    s
}

/// Every invasive sense, for a caller that wants to draw whatever is live.
pub const INVASIVE: [SenseId; 3] = [SenseId::Microphone, SenseId::Clipboard, SenseId::Screen];

fn glyph(sense: SenseId, g: Rect, scene: &mut Scene) {
    let ink = Paint::solid(TELL_COLOR);
    let at = |u: f32, v: f32| (g.x + u * g.w, g.y + v * g.h);
    match sense {
        // A level meter: three bars, the middle one tallest. Reads as "it is
        // hearing you" rather than as a device.
        SenseId::Microphone => {
            for (u, top, bottom) in [(0.14f32, 0.30f32, 0.70f32), (0.44, 0.06, 0.94), (0.74, 0.24, 0.76)] {
                let (x, y0) = at(u, top);
                let (_, y1) = at(u, bottom);
                scene.fill(
                    Path::rect(Rect::new(x, y0, g.w * 0.12, y1 - y0)),
                    ink.clone(),
                );
            }
        }
        // A clipboard: a board with a clip on top and two ruled lines.
        SenseId::Clipboard => {
            let (bx, by) = at(0.10, 0.18);
            let (rx, ry) = at(0.90, 0.96);
            scene.stroke(
                Path::rect(Rect::new(bx, by, rx - bx, ry - by)),
                ink.clone(),
                STROKE_PX,
            );
            let (cx, cy) = at(0.30, 0.00);
            let (dx, dy) = at(0.70, 0.30);
            scene.fill(Path::rect(Rect::new(cx, cy, dx - cx, dy - cy)), ink.clone());
        }
        // A capture reticle: four corner brackets around a filled centre.
        SenseId::Screen => {
            let arm = 0.30f32;
            for (u, v, su, sv) in
                [(0.0f32, 0.0f32, 1.0f32, 1.0f32), (1.0, 0.0, -1.0, 1.0), (0.0, 1.0, 1.0, -1.0), (1.0, 1.0, -1.0, -1.0)]
            {
                let (ax, ay) = at(u + arm * su, v);
                let (bx, by) = at(u, v);
                let (cx, cy) = at(u, v + arm * sv);
                scene.stroke(
                    Path::build(|p| {
                        p.move_to(ax, ay).line_to(bx, by).line_to(cx, cy);
                    }),
                    ink.clone(),
                    STROKE_PX,
                );
            }
            let (px, py) = at(0.36, 0.36);
            let (qx, qy) = at(0.64, 0.64);
            scene.fill(Path::rect(Rect::new(px, py, qx - px, qy - py)), ink.clone());
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_paint::paint::Paint;
    use wisp_paint::scene::Cmd;

    const HER: f32 = 160.0;

    fn at() -> Point {
        Point::new(800.0, 600.0)
    }

    fn shape(s: &Scene) -> String {
        format!("{:?}", s.cmds())
    }

    #[test]
    fn every_invasive_sense_shows_something_while_it_is_live() {
        for sense in INVASIVE {
            let s = scene(sense, true, at(), HER, 0.5);
            assert!(!s.is_empty(), "{sense:?} is live and invisible — SPEC §0.3");
            assert_eq!(sense.consent(), Consent::Invasive);
        }
    }

    #[test]
    fn nothing_is_drawn_when_the_sense_is_not_live() {
        for sense in INVASIVE {
            for phase in [0.0f32, 0.25, 0.5, 0.99] {
                assert!(scene(sense, false, at(), HER, phase).is_empty(), "{sense:?}");
            }
        }
    }

    #[test]
    fn an_ambient_sense_never_borrows_the_tell() {
        // A tell on something harmless would teach the operator to ignore it.
        for sense in [
            SenseId::Idle,
            SenseId::ActiveWindow,
            SenseId::WindowGeometry,
            SenseId::Media,
            SenseId::Audio,
            SenseId::Notifications,
            SenseId::Vitals,
            SenseId::Workspace,
            SenseId::Fleet,
        ] {
            assert_eq!(slot(sense), None, "{sense:?}");
            assert!(scene(sense, true, at(), HER, 0.5).is_empty(), "{sense:?}");
            assert!(badge_rect(sense, at(), HER).is_none());
        }
    }

    #[test]
    fn each_sense_draws_a_different_glyph() {
        let drawn: Vec<(SenseId, String)> = INVASIVE
            .iter()
            .map(|s| (*s, shape(&scene(*s, true, at(), HER, 0.5))))
            .collect();
        for i in 0..drawn.len() {
            for j in (i + 1)..drawn.len() {
                assert_ne!(
                    drawn[i].1, drawn[j].1,
                    "{:?} and {:?} are indistinguishable",
                    drawn[i].0, drawn[j].0
                );
            }
        }
    }

    #[test]
    fn simultaneous_tells_never_overlap() {
        let rects: Vec<Rect> =
            INVASIVE.iter().filter_map(|s| badge_rect(*s, at(), HER)).collect();
        assert_eq!(rects.len(), 3);
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(
                    rects[i].intersect(rects[j]).is_empty(),
                    "{:?} overlaps {:?}",
                    rects[i],
                    rects[j]
                );
            }
        }
    }

    #[test]
    fn the_tell_stays_on_her_so_it_cannot_be_pushed_off_the_output() {
        for size in [48.0f32, 96.0, 160.0, 320.0] {
            let her = her_box(at(), size);
            for sense in INVASIVE {
                let r = badge_rect(sense, at(), size).expect("invasive");
                // Always horizontally inside her, always centred on her.
                assert!(
                    r.x >= her.x && r.right() <= her.right() + 0.01,
                    "{sense:?} at size {size}: {r:?} left her box {her:?} sideways"
                );
                assert!(
                    her.contains(r.centre()),
                    "{sense:?} at size {size}: {r:?} is not on her at all"
                );
                // At any size she is actually drawn at, the column fits.
                if size >= 96.0 {
                    assert!(
                        r.y >= her.y && r.bottom() <= her.bottom() + 0.01,
                        "{sense:?} at size {size}: {r:?} escaped {her:?}"
                    );
                } else {
                    // Below that the overhang is bounded by one badge gap.
                    let over = (her.y - r.y).max(r.bottom() - her.bottom()).max(0.0);
                    assert!(over <= r.w * PAD_FRACTION + 0.01, "overhang {over} at size {size}");
                }
            }
        }
    }

    #[test]
    fn a_slot_never_moves_when_another_sense_turns_off() {
        // Fixed slots are the whole point: the operator learns where to look.
        let a = badge_rect(SenseId::Screen, at(), HER);
        let b = badge_rect(SenseId::Screen, at(), HER);
        assert_eq!(a, b);
        assert_eq!(slot(SenseId::Microphone), Some(0));
        assert_eq!(slot(SenseId::Clipboard), Some(1));
        assert_eq!(slot(SenseId::Screen), Some(2));
    }

    #[test]
    fn the_badge_stays_readable_however_small_she_is_shed() {
        for size in [16.0f32, 48.0, 160.0, 1000.0] {
            let r = badge_rect(SenseId::Microphone, at(), size).expect("invasive");
            assert!(
                (BADGE_MIN_PX..=BADGE_MAX_PX).contains(&r.w),
                "size {size} produced a {}px badge",
                r.w
            );
        }
    }

    #[test]
    fn the_tell_survives_t3_flat_fills_only() {
        // No tier argument exists on purpose: the answer never changes. What
        // must hold is that nothing here needs the vector-only machinery.
        for sense in INVASIVE {
            let s = scene(sense, true, at(), HER, 0.5);
            assert_eq!(s.blur_count(), 0);
            for c in s.cmds() {
                match c {
                    Cmd::Fill { paint, .. } | Cmd::Stroke { paint, .. } => assert!(
                        matches!(paint, Paint::Solid(_)),
                        "{sense:?} emitted a gradient: {paint:?}"
                    ),
                    Cmd::Backdrop { .. } | Cmd::BlurBackdrop { .. } => panic!("{c:?}"),
                    Cmd::Text { .. } => panic!("the tell must not depend on shaping"),
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn the_chip_is_opaque_and_the_mark_is_the_danger_colour() {
        let s = scene(SenseId::Microphone, true, at(), HER, 0.5);
        assert!(PLATE_COLOR.is_opaque(), "a see-through tell is not a tell");
        assert_eq!(TELL_COLOR.to_hex(), 0xff5470, "§1: danger is #ff5470");
        let plate = s.cmds().iter().any(|c| {
            matches!(c, Cmd::Fill { paint, .. } if *paint == Paint::solid(PLATE_COLOR))
        });
        assert!(plate, "the glyph must sit on an opaque plate");
    }

    #[test]
    fn the_pulse_is_visible_and_is_driven_only_by_the_phase() {
        let quiet = scene(SenseId::Screen, true, at(), HER, 0.0);
        let loud = scene(SenseId::Screen, true, at(), HER, 0.5);
        assert_ne!(shape(&quiet), shape(&loud), "the tell must breathe");
        assert!(loud.cmds().len() > quiet.cmds().len(), "the halo appears at the peak");
        // Same phase, same commands: no hidden clock.
        assert_eq!(shape(&loud), shape(&scene(SenseId::Screen, true, at(), HER, 0.5)));
        // …and the phase wraps rather than clipping.
        assert_eq!(shape(&loud), shape(&scene(SenseId::Screen, true, at(), HER, 1.5)));
        assert_eq!(pulse(0.0), 0.0);
        assert_eq!(pulse(0.5), 1.0);
        assert!((pulse(1.0) - 0.0).abs() < 1e-6);
        assert_eq!(pulse(f32::NAN), 0.0);
    }

    #[test]
    fn the_badge_is_angular_chrome_not_a_soft_creature_shape() {
        // §3.5b exempts *her* from the geometry rule. The tell is chrome, so
        // the rule binds it — and that contrast is what makes it legible.
        let s = scene(SenseId::Clipboard, true, at(), HER, 0.3);
        assert!(!s.is_empty());
        assert!(Radius::XS.px_u8() >= Radius::MIN_PX && Radius::XS.px_u8() <= Radius::MAX_PX);
        assert!((1.5..=2.0).contains(&STROKE_PX), "§8's stroke band");
    }

    /// §11's review-by-looking, offscreen only (SPEC §4 forbids a window).
    /// `WISP_SHELL_DUMP=<dir>` writes the frames over a checkerboard.
    fn dump(img: &wisp_paint::Image, name: &str) {
        let Some(dir) = std::env::var_os("WISP_SHELL_DUMP") else { return };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).ok();
        let mut out = format!("P6\n{} {}\n255\n", img.w, img.h).into_bytes();
        for y in 0..img.h {
            for x in 0..img.w {
                let [r, g, b, a] = img.pixel(x, y);
                let light = ((x / 24) + (y / 24)) % 2 == 0;
                let bg: [u8; 3] = if light { [0x86, 0x86, 0x8e] } else { [0x4e, 0x4e, 0x58] };
                let inv = (255 - a) as u32;
                for (i, c) in [r, g, b].into_iter().enumerate() {
                    out.push((c as u32 + bg[i] as u32 * inv / 255).min(255) as u8);
                }
            }
        }
        std::fs::write(dir.join(format!("{name}.ppm")), out).ok();
    }

    #[test]
    fn the_tell_renders_and_can_be_reviewed_by_looking() {
        if std::env::var_os("NX_WISP_CONFIG_DIR").is_none() {
            let dir = std::env::temp_dir().join(format!("nx-wisp-shell-{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok();
            std::env::set_var("NX_WISP_CONFIG_DIR", &dir);
        }
        let Ok(mut p) = wisp_paint::Painter::new(wisp_paint::AdapterPreference::HighPerformance)
        else {
            eprintln!("no Vulkan adapter here — skipping the look-at-it test");
            return;
        };
        let (w, h) = (420u32, 260u32);
        let her_size = 160.0;
        let a = Point::new(210.0, 130.0);

        for (name, senses, phase) in [
            ("tell_mic", vec![SenseId::Microphone], 0.5f32),
            ("tell_clipboard", vec![SenseId::Clipboard], 0.5),
            ("tell_screen", vec![SenseId::Screen], 0.5),
            ("tell_all_peak", INVASIVE.to_vec(), 0.5),
            ("tell_all_trough", INVASIVE.to_vec(), 0.02),
        ] {
            let target = p.offscreen(w, h).expect("offscreen");
            let mut s = Scene::new();
            // A stand-in for her: soft, round, violet — §3.5b's creature. The
            // tell has to look like instrumentation stuck to that.
            s.fill(
                Path::rounded_rect(her_box(a, her_size), Radius::CARD),
                Paint::solid(wisp_theme::palette::VIOLET.with_alpha(0.75)),
            );
            for sense in &senses {
                build(*sense, true, a, her_size, phase, &mut s);
            }
            assert_eq!(s.blur_count(), 0);
            p.render(&target, &s).expect("render");
            let img = p.read(&target).expect("read");
            dump(&img, name);
            // Every chip that was asked for is fully opaque where it landed.
            for sense in &senses {
                let c = badge_rect(*sense, a, her_size).expect("invasive").centre();
                assert_eq!(
                    img.alpha(c.x as u32, c.y as u32),
                    255,
                    "{name}: the {sense:?} tell is see-through"
                );
            }
        }
    }

    #[test]
    fn a_degenerate_size_does_not_panic_or_draw_nonsense() {
        for size in [0.0f32, -10.0] {
            let s = scene(SenseId::Microphone, true, at(), size, 0.5);
            let _ = s.bounds();
        }
    }
}
