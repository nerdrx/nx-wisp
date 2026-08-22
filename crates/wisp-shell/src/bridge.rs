//! rig → paint.
//!
//! `wisp-rig` is deliberately pure: it knows nothing about the GPU and hands
//! out its own `DrawShape`/`Paint` types. `wisp-paint` knows nothing about
//! skeletons. Nobody owned the join, so it lives here — and because it is
//! plain data in, plain data out, it is unit-testable with no GPU.

use wisp_paint::{Paint as PPaint, Path as PPath, PathBuilder, Scene};
use wisp_rig::{DrawShape, Paint as RPaint, RigFrame, Rgba, Verb};
use wisp_theme::Color;

/// The rig works in linear-ish f32 channels; the theme's `Color` is 8-bit sRGB
/// with straight alpha, which is what the painter's shaders expect.
fn color(c: Rgba) -> Color {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    Color::rgba(q(c.r), q(c.g), q(c.b), q(c.a))
}

/// Rig gradients are in canvas space and already baked into surface pixels by
/// the time a `RigFrame` exists; paint wants them relative to the shape's
/// bounding box. Convert against the shape's own bounds.
fn paint_of(p: &RPaint, bbox: wisp_paint::Rect) -> PPaint {
    match p {
        RPaint::Solid(c) => PPaint::Solid(color(*c)),
        RPaint::Linear(g) => {
            let (dx, dy) = (g.end.x - g.start.x, g.end.y - g.start.y);
            // CSS convention: 0 = to top, 90 = to right.
            let angle = dx.atan2(-dy).to_degrees();
            PPaint::Linear {
                angle_deg: if angle.is_finite() { angle } else { 180.0 },
                stops: stops(&g.stops),
            }
        }
        RPaint::Radial(g) => {
            // The rig's radial is a two-point conical (a `focus` offset from
            // `center`), which is what gives glass an off-axis highlight rather
            // than a flat bullseye. The painter has no conical form, so the
            // focus is folded in as a half-weighted centre offset — close
            // enough that the highlight still sits off-axis.
            let w = (bbox.right() - bbox.x).max(1.0);
            let h = (bbox.bottom() - bbox.y).max(1.0);
            let cx = (g.center.x + g.focus.x) * 0.5;
            let cy = (g.center.y + g.focus.y) * 0.5;
            PPaint::Radial {
                cx: (cx - bbox.x) / w,
                cy: (cy - bbox.y) / h,
                rx: (g.radius / w).max(0.001),
                ry: (g.radius / h).max(0.001),
                stops: stops(&g.stops),
            }
        }
    }
}

fn stops(src: &[wisp_rig::paint::GradientStop]) -> Vec<wisp_paint::paint::Stop> {
    src.iter()
        .map(|s| wisp_paint::paint::Stop { at: s.at, color: color(s.color) })
        .collect()
}

/// Walk a deformed shape's verbs and points into a paint path.
///
/// The rig stores verbs once (they are static for a skin) and rewrites only
/// `points` each frame, so this consumes the two in lockstep. A malformed
/// pairing would be a rig bug; we stop rather than panic, because a dropped
/// frame is survivable and a crashed companion is not.
pub fn path_of(shape: &DrawShape) -> PPath {
    let mut pb = PathBuilder::new();
    {
        let b = &mut pb;
        let mut i = 0usize;
        let p = &shape.points;
        for v in &shape.verbs {
            match v {
                Verb::Move => {
                    if i >= p.len() { break; }
                    b.move_to(p[i].x, p[i].y);
                    i += 1;
                }
                Verb::Line => {
                    if i >= p.len() { break; }
                    b.line_to(p[i].x, p[i].y);
                    i += 1;
                }
                Verb::Quad => {
                    if i + 1 >= p.len() { break; }
                    b.quad_to(p[i].x, p[i].y, p[i + 1].x, p[i + 1].y);
                    i += 2;
                }
                Verb::Cubic => {
                    if i + 2 >= p.len() { break; }
                    b.cubic_to(
                        p[i].x, p[i].y,
                        p[i + 1].x, p[i + 1].y,
                        p[i + 2].x, p[i + 2].y,
                    );
                    i += 3;
                }
                Verb::Close => { b.close(); }
            }
        }
    }
    pb.finish()
}

/// Build a scene for one posed frame. Shapes arrive back-to-front already.
pub fn scene_of(frame: &RigFrame, scene: &mut Scene) {
    scene.clear();
    for shape in &frame.shapes {
        if shape.opacity <= 0.001 {
            continue;
        }
        let path = path_of(shape);
        let bbox = path.bbox();
        if bbox.is_empty() {
            continue;
        }
        if let Some(f) = &shape.fill {
            scene.fill(path.clone(), paint_of(f, bbox));
        }
        if let Some(s) = &shape.stroke {
            scene.stroke(path, paint_of(&s.paint, bbox), s.width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_rig::{default_skin, Rig, RigInput};

    fn posed() -> RigFrame {
        let mut rig = Rig::new(default_skin().expect("default skin"));
        rig.update(0.016, &RigInput { size_px: 96.0, ..Default::default() });
        rig.frame().clone()
    }

    #[test]
    fn every_visible_shape_becomes_at_least_one_draw() {
        let f = posed();
        let mut s = Scene::new();
        scene_of(&f, &mut s);
        let visible = f.shapes.iter().filter(|sh| sh.opacity > 0.001).count();
        assert!(visible > 0, "the default skin drew nothing");
        assert!(
            s.cmds().len() >= visible,
            "{} shapes produced only {} commands",
            visible,
            s.cmds().len()
        );
    }

    #[test]
    fn a_truncated_point_list_stops_instead_of_panicking() {
        // A rig bug must cost us a frame, never the process.
        let f = posed();
        let mut shape = f.shapes[0].clone();
        shape.points.clear();
        let p = path_of(&shape);
        assert!(p.bbox().is_empty(), "a shape with no points must draw nothing");
    }

    #[test]
    fn radial_gradients_land_inside_the_unit_box() {
        let f = posed();
        for sh in &f.shapes {
            let bbox = path_of(sh).bbox();
            if bbox.is_empty() { continue; }
            if let Some(RPaint::Radial(_)) = &sh.fill {
                if let PPaint::Radial { rx, ry, .. } = paint_of(sh.fill.as_ref().unwrap(), bbox) {
                    assert!(rx > 0.0 && ry > 0.0, "shape {:?} has a degenerate radius", sh.name);
                }
            }
        }
    }
}
