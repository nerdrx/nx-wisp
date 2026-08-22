//! Drawing the canvas: her, and everything the editor puts on top of her.
//!
//! # Pure, and therefore assertable
//!
//! Nothing here touches a GPU. Every function takes a [`Scene`] — `wisp-paint`'s
//! flat command list — and appends to it, so a test can build the canvas
//! headlessly and assert on the commands. The GPU only appears when something
//! decides to *render* the scene, which is the host's job.
//!
//! # The bridge is ours, and that is a reported gap
//!
//! `wisp-rig` hands out its own `DrawShape`/`Paint`; `wisp-paint` knows nothing
//! about skeletons. The join already exists — in `wisp-shell`'s `bridge` module
//! — but SPEC §2 lets this crate depend on proto, rig, paint and theme, and
//! **not** on shell. So the conversion is repeated here.
//!
//! It is not a straight copy. Shell's bridge maps a frame that is already in
//! surface pixels; this one folds the editor's pan-and-zoom in as it goes, so
//! there is no second pass over the points and a shape at 8× zoom is
//! tessellated at 8× rather than scaled up from a 1× tessellation. The right
//! long-term home for the shared half is `wisp-paint`, and that is in the
//! report.
//!
//! # She is round; the editor is not
//!
//! SPEC §3.5b, and it decides every colour and every corner below. The
//! *artwork* is drawn exactly as the rig produced it — no editor styling
//! touches her. The *chrome* on top — handles, bone gizmos, the canvas frame —
//! is angular, hairline, and made of the theme's tokens: violet for what is
//! selected, cyan for light and structure, muted for what is only there for
//! reference.

use wisp_paint::geom::{Path as PPath, PathBuilder, Point, Rect};
use wisp_paint::paint::{Paint as PPaint, Stop};
use wisp_paint::scene::Scene;
use wisp_rig::math::Vec2;
use wisp_rig::paint::Rgba;
use wisp_rig::path::Verb;
use wisp_rig::skeleton::Pose;
use wisp_rig::skin::doc::SkinDoc;
use wisp_rig::{DrawShape, Paint as RPaint, RigFrame, Skin};
use wisp_theme::{palette, Color, Radius};

use crate::select::{Selection, Target};
use crate::view::Viewport;

// ------------------------------------------------------------------- colours

/// The canvas ground. Deep space, never flat black (DESIGN.md §1).
pub const CANVAS_BG: Color = palette::BG_BOTTOM;
/// The rectangle the skin declares as its canvas.
pub const CANVAS_EDGE: Color = palette::LINE;
/// An unselected path outline.
pub const OUTLINE: Color = Color::hexa(0x9a8fc0, 0.55);
/// A selected path outline, and a selected handle.
pub const SELECTED: Color = palette::VIOLET_SOFT;
/// A handle that is not selected.
pub const HANDLE: Color = palette::TEXT;
/// An off-curve control point, and the hairline that tethers it.
pub const CONTROL: Color = palette::CYAN;
/// Bone gizmos.
pub const BONE: Color = Color::hexa(0x00e5ff, 0.72);
pub const BONE_SELECTED: Color = palette::AMBER;
/// The canvas anchor cross — where she touches the ground.
pub const ANCHOR: Color = palette::CYAN;

/// Handle sizes, in **pixels**, so they stay the same size at every zoom.
pub const ANCHOR_HANDLE_PX: f32 = 7.0;
pub const CONTROL_HANDLE_PX: f32 = 5.0;
pub const HAIRLINE_PX: f32 = 1.0;
pub const BONE_WIDTH_PX: f32 = 2.0;

// -------------------------------------------------------------- rig → paint

fn color(c: Rgba) -> Color {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    Color::rgba(q(c.r), q(c.g), q(c.b), q(c.a))
}

fn stops(src: &[wisp_rig::paint::GradientStop]) -> Vec<Stop> {
    src.iter().map(|s| Stop { at: s.at, color: color(s.color) }).collect()
}

/// One rig paint, in the viewport's space.
///
/// Linear gradients keep their direction, which is scale-invariant; radial
/// ones are expressed relative to the shape's bounding box, so they need no
/// conversion either. The focus offset is folded into the centre at half
/// weight — the painter has no two-point conical form, and half weight keeps
/// the highlight off-axis, which is the part that matters (DESIGN.md §1).
fn paint_of(p: &RPaint, bbox: Rect) -> PPaint {
    match p {
        RPaint::Solid(c) => PPaint::Solid(color(*c)),
        RPaint::Linear(g) => {
            let (dx, dy) = (g.end.x - g.start.x, g.end.y - g.start.y);
            let angle = dx.atan2(-dy).to_degrees();
            PPaint::Linear {
                angle_deg: if angle.is_finite() { angle } else { 180.0 },
                stops: stops(&g.stops),
            }
        }
        RPaint::Radial(g) => {
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

/// A deformed shape's geometry, mapped through the viewport in one pass.
pub fn path_of(shape: &DrawShape, view: &Viewport) -> PPath {
    let mut pb = PathBuilder::new();
    let p = &shape.points;
    let s = |v: Vec2| {
        let q = view.to_screen(v);
        (q.x, q.y)
    };
    let mut i = 0usize;
    for v in &shape.verbs {
        match v {
            Verb::Move => {
                if i >= p.len() {
                    break;
                }
                let (x, y) = s(p[i]);
                pb.move_to(x, y);
                i += 1;
            }
            Verb::Line => {
                if i >= p.len() {
                    break;
                }
                let (x, y) = s(p[i]);
                pb.line_to(x, y);
                i += 1;
            }
            Verb::Quad => {
                if i + 1 >= p.len() {
                    break;
                }
                let (cx, cy) = s(p[i]);
                let (x, y) = s(p[i + 1]);
                pb.quad_to(cx, cy, x, y);
                i += 2;
            }
            Verb::Cubic => {
                if i + 2 >= p.len() {
                    break;
                }
                let (a, b) = (s(p[i]), s(p[i + 1]));
                let (x, y) = s(p[i + 2]);
                pb.cubic_to(a.0, a.1, b.0, b.1, x, y);
                i += 3;
            }
            Verb::Close => {
                pb.close();
            }
        }
    }
    pb.finish()
}

/// Draw a posed frame into the viewport at `alpha`.
///
/// `alpha` is what makes onion skinning work: a ghost is the same call with a
/// third of the opacity.
pub fn draw_frame(frame: &RigFrame, view: &Viewport, alpha: f32, scene: &mut Scene) {
    let alpha = alpha.clamp(0.0, 1.0);
    if alpha <= 0.001 {
        return;
    }
    for shape in &frame.shapes {
        if shape.opacity <= 0.001 {
            continue;
        }
        let path = path_of(shape, view);
        let bbox = path.bbox();
        if bbox.is_empty() {
            continue;
        }
        let o = shape.opacity * alpha;
        if let Some(f) = &shape.fill {
            scene.fill(path.clone(), paint_of(f, bbox).with_opacity(o));
        }
        if let Some(st) = &shape.stroke {
            scene.stroke(path, paint_of(&st.paint, bbox).with_opacity(o), st.width * view.zoom);
        }
    }
}

// ------------------------------------------------------------------- chrome

/// The canvas ground and the rectangle the skin declares.
pub fn draw_ground(doc: &SkinDoc, view: &Viewport, scene: &mut Scene) {
    scene.fill_rect(view.rect, Radius::CARD, PPaint::Solid(CANVAS_BG));

    let size = Vec2::new(doc.canvas.size[0].0, doc.canvas.size[1].0);
    let tl = view.to_screen(Vec2::ZERO);
    let br = view.to_screen(size);
    let r = Rect::new(tl.x, tl.y, br.x - tl.x, br.y - tl.y);
    scene.stroke(PPath::rect(r), PPaint::Solid(CANVAS_EDGE), HAIRLINE_PX);

    // The anchor: where the canvas meets her position on screen. A cross, not
    // a dot — a dot at 4px is indistinguishable from a path handle, and this
    // is the one point in the document that is not geometry.
    let a = view.to_screen(Vec2::new(doc.canvas.anchor[0].0, doc.canvas.anchor[1].0));
    let k = 9.0;
    let cross = PPath::build(|pb| {
        pb.move_to(a.x - k, a.y).line_to(a.x + k, a.y);
        pb.move_to(a.x, a.y - k).line_to(a.x, a.y + k);
    });
    scene.stroke(cross, PPaint::Solid(ANCHOR.with_alpha(0.8)), HAIRLINE_PX);
}

/// The rest-pose outline of one shape, straight from the document — what the
/// pen and the point handles actually edit, as opposed to the deformed shape
/// the rig draws.
pub fn draw_rest_outline(
    doc: &SkinDoc,
    shape: usize,
    view: &Viewport,
    selected: bool,
    scene: &mut Scene,
) {
    let Ok(path) = crate::canvas::path_of(doc, shape) else { return };
    let rings = path.flatten(crate::canvas::CURVE_SAMPLES);
    let mut pb = PathBuilder::new();
    for ring in &rings {
        for (i, p) in ring.iter().enumerate() {
            let q = view.to_screen(*p);
            if i == 0 {
                pb.move_to(q.x, q.y);
            } else {
                pb.line_to(q.x, q.y);
            }
        }
    }
    let outline = pb.finish();
    if outline.bbox().is_empty() {
        return;
    }
    let c = if selected { SELECTED } else { OUTLINE };
    scene.stroke(outline, PPaint::Solid(c), HAIRLINE_PX);
}

/// A square handle, centred on a screen point. Squares, not circles: DESIGN.md
/// sanctions exactly two circles and neither of them is a path handle.
fn handle(at: Point, size_px: f32, fill: Color, scene: &mut Scene) {
    let h = size_px * 0.5;
    let r = Rect::new(at.x - h, at.y - h, size_px, size_px);
    scene.fill_rect(r, Radius::XS, PPaint::Solid(fill));
}

/// Every point of a shape, with control points tethered to their anchors.
pub fn draw_handles(
    doc: &SkinDoc,
    shape: usize,
    view: &Viewport,
    selection: &Selection,
    scene: &mut Scene,
) {
    let Ok(path) = crate::canvas::path_of(doc, shape) else { return };

    // Tethers first, so the handles sit on top of them.
    let mut tether = PathBuilder::new();
    let mut any_tether = false;
    let mut i = 0usize;
    let mut cursor: Option<Vec2> = None;
    for v in &path.verbs {
        let n = v.point_count();
        if n == 0 {
            continue;
        }
        let pts = &path.points[i..i + n];
        if n > 1 {
            // Controls tether to the point before the verb and to its endpoint.
            if let Some(prev) = cursor {
                let a = view.to_screen(prev);
                let b = view.to_screen(pts[0]);
                tether.move_to(a.x, a.y).line_to(b.x, b.y);
            }
            let a = view.to_screen(pts[n - 2]);
            let b = view.to_screen(pts[n - 1]);
            tether.move_to(a.x, a.y).line_to(b.x, b.y);
            any_tether = true;
        }
        cursor = Some(pts[n - 1]);
        i += n;
    }
    if any_tether {
        scene.stroke(tether.finish(), PPaint::Solid(CONTROL.with_alpha(0.35)), HAIRLINE_PX);
    }

    for (idx, p) in path.points.iter().enumerate() {
        let at = view.to_screen(*p);
        if !view.rect.contains(at) {
            continue;
        }
        let is_sel = selection.contains(Target::Point { shape, point: idx });
        let anchor = crate::canvas::is_anchor(&path, idx);
        let (size, base) = if anchor {
            (ANCHOR_HANDLE_PX, HANDLE)
        } else {
            (CONTROL_HANDLE_PX, CONTROL)
        };
        let c = if is_sel { SELECTED } else { base };
        // A selected handle gets a halo rather than a bigger box, so the grid
        // of handles keeps its rhythm while the selection is still obvious.
        if is_sel {
            handle(at, size + 4.0, SELECTED.with_alpha(0.28), scene);
        }
        handle(at, size, c, scene);
    }
}

/// Bone gizmos: a tapered spike from head to tip, with a square at the joint.
pub fn draw_bones(
    skin: &Skin,
    pose: &Pose,
    view: &Viewport,
    selection: &Selection,
    scene: &mut Scene,
) {
    for i in 0..skin.skeleton.len() {
        let head = view.to_screen(pose.world_pos(i));
        let tip = view.to_screen(pose.world_tip(&skin.skeleton, i));
        let selected = selection.contains(Target::Bone(i));
        let c = if selected { BONE_SELECTED } else { BONE };

        let dx = tip.x - head.x;
        let dy = tip.y - head.y;
        let len = (dx * dx + dy * dy).sqrt();
        if len > 3.0 {
            // A spike: wide at the head, a point at the tip. Reads as a
            // direction without needing an arrowhead.
            let (nx, ny) = (-dy / len, dx / len);
            let w = (len * 0.14).clamp(2.0, 7.0);
            let spike = PPath::build(|pb| {
                pb.move_to(head.x + nx * w, head.y + ny * w)
                    .line_to(tip.x, tip.y)
                    .line_to(head.x - nx * w, head.y - ny * w)
                    .close();
            });
            scene.fill(spike, PPaint::Solid(c.with_alpha(if selected { 0.55 } else { 0.30 })));
            scene.stroke(
                PPath::build(|pb| {
                    pb.move_to(head.x, head.y).line_to(tip.x, tip.y);
                }),
                PPaint::Solid(c),
                BONE_WIDTH_PX,
            );
        }
        handle(head, if selected { 8.0 } else { 6.0 }, c, scene);
    }
}

/// The IK chains a skin declares, drawn over the bones they own.
pub fn draw_ik(skin: &Skin, pose: &Pose, view: &Viewport, scene: &mut Scene) {
    use wisp_rig::skin::IkKind;
    for def in &skin.iks {
        match def.kind {
            IkKind::TwoBone { root, mid, end, .. } => {
                let a = view.to_screen(pose.world_pos(root));
                let b = view.to_screen(pose.world_pos(mid));
                let c = view.to_screen(pose.world_pos(end));
                scene.stroke(
                    PPath::build(|pb| {
                        pb.move_to(a.x, a.y).line_to(b.x, b.y).line_to(c.x, c.y);
                    }),
                    PPaint::Solid(palette::AMBER.with_alpha(0.7)),
                    HAIRLINE_PX * 2.0,
                );
                handle(c, 9.0, palette::AMBER, scene);
            }
            IkKind::LookAt { bone, cfg } => {
                let o = pose.world_pos(bone);
                let at = view.to_screen(o);
                // The cone the constraint permits, so "she can see it" is
                // visible rather than a number in a field.
                let base = pose.world[bone].rotation() + cfg.forward.angle();
                let r = 34.0;
                let arc = PPath::build(|pb| {
                    pb.move_to(at.x, at.y);
                    let steps = 12;
                    for k in 0..=steps {
                        let t = k as f32 / steps as f32;
                        let a = base - cfg.max_angle + 2.0 * cfg.max_angle * t;
                        pb.line_to(at.x + r * a.cos(), at.y + r * a.sin());
                    }
                    pb.close();
                });
                scene.fill(arc, PPaint::Solid(palette::CYAN.with_alpha(0.12)));
                handle(at, 9.0, palette::CYAN, scene);
            }
        }
    }
}

/// A marquee, for a rubber-band selection.
pub fn draw_marquee(from: Point, to: Point, scene: &mut Scene) {
    let r = Rect::new(from.x.min(to.x), from.y.min(to.y), (to.x - from.x).abs(), (to.y - from.y).abs());
    if r.is_empty() {
        return;
    }
    scene.fill_rect(r, Radius::XS, PPaint::Solid(SELECTED.with_alpha(0.12)));
    scene.stroke(PPath::rect(r), PPaint::Solid(SELECTED), HAIRLINE_PX);
}

/// The weight of every point of a shape towards one bone, as a heat overlay:
/// cyan where the bone owns the point, invisible where it does not.
///
/// This is the weight brush's feedback, and it is the reason weight painting
/// is usable at all — a number in a table cannot tell you that the seam
/// between her body and her tail is one point too high.
pub fn draw_weights(
    doc: &SkinDoc,
    shape: usize,
    bone: &str,
    view: &Viewport,
    scene: &mut Scene,
) {
    let Ok(path) = crate::canvas::path_of(doc, shape) else { return };
    let Some(s) = doc.shapes.get(shape) else { return };
    for idx in 0..path.points.len() {
        let w = weight_of(s, idx, bone);
        if w <= 0.001 {
            continue;
        }
        let at = view.to_screen(path.points[idx]);
        if !view.rect.contains(at) {
            continue;
        }
        handle(at, 4.0 + 7.0 * w, CONTROL.with_alpha(0.25 + 0.55 * w), scene);
    }
}

/// What one point's weight towards a bone currently is, taking the shape's
/// rigid bind into account when there is no explicit entry.
pub fn weight_of(shape: &wisp_rig::skin::doc::ShapeDoc, point: usize, bone: &str) -> f32 {
    if let Some(w) = shape.weights.iter().find(|w| w.point == point) {
        return w
            .bones
            .iter()
            .zip(&w.weights)
            .find(|(n, _)| n.as_str() == bone)
            .map(|(_, v)| v.0)
            .unwrap_or(0.0);
    }
    if shape.bind == bone {
        return 1.0;
    }
    if let Some(a) = &shape.bind_auto {
        if a.bones.iter().any(|n| n == bone) {
            // Auto-bound points have no stored number; show them as
            // "influenced, amount decided at compile time" rather than as
            // nothing.
            return 0.5;
        }
    }
    0.0
}
