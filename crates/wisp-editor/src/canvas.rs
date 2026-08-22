//! The vector canvas: reading a shape's points, hit-testing them, and turning
//! a pointer gesture into a [`Command`].
//!
//! # Points are addressed the way the format addresses them
//!
//! `[[shape.weight]]` numbers a path's points "in the order the path lists
//! them — control points included". This module uses exactly that numbering,
//! everywhere, so a point index is the same integer in the canvas, in the
//! weight table, and in the file. There is no second numbering to convert
//! between and therefore no place for an off-by-one to hide.
//!
//! # Editing goes through the path string
//!
//! A shape's geometry lives in the document as an SVG-subset string.
//! `wisp-rig` owns both directions of that (`Path::parse` / `Path::to_svg`),
//! so an edit here is: parse, mutate the point array, re-emit. That keeps the
//! format's only geometry representation authoritative and means the editor
//! can never invent geometry the runtime cannot read back.
//!
//! Re-emitting **normalises the string's formatting** — spacing and number
//! rendering become `to_svg`'s. That only ever happens to a shape the operator
//! actually edited; an untouched shape's path text is carried through byte for
//! byte, which is what keeps `save → load → save` stable on the shipped skin.

use wisp_rig::math::Vec2;
use wisp_rig::path::{Path, PathError, Verb};
use wisp_rig::skin::doc::{to_pt, Num, PaintDoc, ShapeDoc, SkinDoc, WeightDoc};

use crate::cmd::Command;
use crate::error::EditError;
use crate::select::{Selection, Target};
use crate::view::Viewport;

/// How many line segments a curve becomes when it is hit-tested or drawn as an
/// overlay. Sixteen is what the contour tracer uses for the silhouette, so the
/// outline the operator clicks is the outline the runtime clips against.
pub const CURVE_SAMPLES: usize = 16;

/// Parse one shape's path, with the shape's name in any error.
pub fn path_of(doc: &SkinDoc, shape: usize) -> Result<Path, EditError> {
    let s = doc
        .shapes
        .get(shape)
        .ok_or(EditError::NoSuchIndex { kind: "shape", at: shape, len: doc.shapes.len() })?;
    Path::parse(&s.path).map_err(|e| EditError::BadPath {
        at: format!("shape {:?}", s.name),
        reason: e.to_string(),
    })
}

/// Every point of a shape's path, in canvas units and in path order.
pub fn points_of(doc: &SkinDoc, shape: usize) -> Result<Vec<Vec2>, EditError> {
    Ok(path_of(doc, shape)?.points)
}

/// Which verb owns point `i`, and which of its slots the point fills.
///
/// `Close` consumes no points, so it never owns one.
pub fn verb_of_point(path: &Path, i: usize) -> Option<(usize, usize)> {
    let mut p = 0usize;
    for (vi, v) in path.verbs.iter().enumerate() {
        let n = v.point_count();
        if n > 0 && i < p + n {
            return Some((vi, i - p));
        }
        p += n;
    }
    None
}

/// Is this point an **anchor** — a point the curve passes through — rather
/// than an off-curve control handle? The canvas draws the two differently and
/// the pen tool only ever appends anchors.
pub fn is_anchor(path: &Path, i: usize) -> bool {
    match verb_of_point(path, i) {
        Some((vi, slot)) => slot + 1 == path.verbs[vi].point_count(),
        None => false,
    }
}

// ---------------------------------------------------------------- hit testing

/// What the pointer is over on the canvas, most specific first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hit {
    /// A path point. `dist` is in canvas units, for tie-breaking.
    Point { shape: usize, point: usize, dist: f32 },
    /// A point on a segment between two anchors — where the pen inserts.
    Segment { shape: usize, after_point: usize, at: Vec2, dist: f32 },
    /// The filled interior of a shape.
    Shape(usize),
    /// A bone's origin handle.
    BoneHead { bone: usize, dist: f32 },
    /// A bone's tip handle — the one puppet mode drags.
    BoneTip { bone: usize, dist: f32 },
    Nothing,
}

impl Hit {
    pub fn target(&self) -> Option<Target> {
        match *self {
            Hit::Point { shape, point, .. } => Some(Target::Point { shape, point }),
            Hit::Shape(s) => Some(Target::Shape(s)),
            Hit::BoneHead { bone, .. } | Hit::BoneTip { bone, .. } => Some(Target::Bone(bone)),
            _ => None,
        }
    }
}

/// Which shapes the canvas will hit-test, and in what order. Later entries win
/// ties because they paint on top — the same rule the widget layer uses.
fn z_order(doc: &SkinDoc) -> Vec<usize> {
    let mut order: Vec<usize> = (0..doc.shapes.len()).collect();
    order.sort_by(|a, b| {
        doc.shapes[*a]
            .z
            .cmp(&doc.shapes[*b].z)
            .then(a.cmp(b))
    });
    order
}

/// The point nearest the pointer, if one is inside the grab radius.
///
/// Points of an already-selected shape win ties, so editing a shape that
/// overlaps another does not keep snatching the neighbour's handles.
pub fn hit_point(
    doc: &SkinDoc,
    view: &Viewport,
    at: wisp_paint::geom::Point,
    selection: &Selection,
) -> Hit {
    let p = view.to_canvas(at);
    let r = view.grab_radius();
    let mut best = Hit::Nothing;
    let mut best_score = f32::INFINITY;
    for shape in z_order(doc) {
        let Ok(path) = path_of(doc, shape) else { continue };
        let preferred = selection.contains(Target::Shape(shape))
            || selection.points_of(shape).is_empty().eq(&false);
        for (i, q) in path.points.iter().enumerate() {
            let d = q.dist(p);
            if d > r {
                continue;
            }
            // A preferred shape gets a half-radius handicap, which is enough
            // to win a tie and not enough to steal a clearly closer handle.
            let score = if preferred { d - r * 0.5 } else { d };
            if score < best_score {
                best_score = score;
                best = Hit::Point { shape, point: i, dist: d };
            }
        }
    }
    best
}

/// The nearest point *on* a segment, for the pen tool's insert.
pub fn hit_segment(doc: &SkinDoc, view: &Viewport, at: wisp_paint::geom::Point) -> Hit {
    let p = view.to_canvas(at);
    let r = view.grab_radius();
    let mut best = Hit::Nothing;
    let mut best_d = f32::INFINITY;
    for shape in z_order(doc) {
        let Ok(path) = path_of(doc, shape) else { continue };
        // Walk verbs, tracking the index of the last point each one consumed.
        let mut pi = 0usize;
        let mut cursor = Vec2::ZERO;
        let mut start = Vec2::ZERO;
        for v in &path.verbs {
            let n = v.point_count();
            let pts = &path.points[pi..pi + n];
            let (from, to, samples): (Vec2, Vec2, Vec<Vec2>) = match v {
                Verb::Move => {
                    cursor = pts[0];
                    start = pts[0];
                    pi += n;
                    continue;
                }
                Verb::Line => (cursor, pts[0], vec![cursor, pts[0]]),
                Verb::Quad | Verb::Cubic => {
                    let mut s = Vec::with_capacity(CURVE_SAMPLES + 1);
                    for k in 0..=CURVE_SAMPLES {
                        let t = k as f32 / CURVE_SAMPLES as f32;
                        s.push(match v {
                            Verb::Quad => wisp_rig::path::eval_quad(cursor, pts[0], pts[1], t),
                            _ => wisp_rig::path::eval_cubic(cursor, pts[0], pts[1], pts[2], t),
                        });
                    }
                    (cursor, pts[n - 1], s)
                }
                Verb::Close => (cursor, start, vec![cursor, start]),
            };
            let _ = from;
            for w in samples.windows(2) {
                let (q, _) = p.closest_on_segment(w[0], w[1]);
                let d = q.dist(p);
                if d <= r && d < best_d {
                    best_d = d;
                    // The point after which a new one is inserted is this
                    // verb's last consumed point (or the previous verb's, for
                    // a Close).
                    let after = if n == 0 { pi.saturating_sub(1) } else { pi + n - 1 };
                    best = Hit::Segment { shape, after_point: after, at: q, dist: d };
                }
            }
            cursor = to;
            pi += n;
        }
    }
    best
}

/// The topmost shape whose interior contains the pointer.
pub fn hit_shape(doc: &SkinDoc, view: &Viewport, at: wisp_paint::geom::Point) -> Hit {
    let p = view.to_canvas(at);
    for shape in z_order(doc).into_iter().rev() {
        let Ok(path) = path_of(doc, shape) else { continue };
        if contains(&path, p) {
            return Hit::Shape(shape);
        }
    }
    Hit::Nothing
}

/// Even-odd containment across every subpath — the rule that makes a hole in a
/// donut behave like a hole for the pointer as well as for the renderer.
pub fn contains(path: &Path, p: Vec2) -> bool {
    let rings = path.flatten(CURVE_SAMPLES);
    let mut inside = false;
    for ring in rings {
        if ring.len() < 3 {
            continue;
        }
        let poly = wisp_rig::contour::Polygon { points: ring };
        if poly.contains(p) {
            inside = !inside;
        }
    }
    inside
}

/// Everything under the pointer, in the order the select tool tries them.
pub fn pick(
    doc: &SkinDoc,
    view: &Viewport,
    at: wisp_paint::geom::Point,
    selection: &Selection,
) -> Hit {
    match hit_point(doc, view, at, selection) {
        Hit::Nothing => hit_shape(doc, view, at),
        h => h,
    }
}

// ------------------------------------------------------------------ edit ops

/// Rebuild a shape with a new point array, keeping its verbs.
fn with_points(shape: &ShapeDoc, path: &Path, points: Vec<Vec2>) -> ShapeDoc {
    let mut out = shape.clone();
    let np = Path { verbs: path.verbs.clone(), points };
    out.path = np.to_svg();
    out
}

/// Move a set of points by a delta, in canvas units.
pub fn move_points(
    doc: &SkinDoc,
    shape: usize,
    points: &[usize],
    delta: Vec2,
) -> Result<Command, EditError> {
    let path = path_of(doc, shape)?;
    let mut pts = path.points.clone();
    for &i in points {
        if i >= pts.len() {
            return Err(EditError::NoSuchIndex { kind: "point", at: i, len: pts.len() });
        }
        pts[i] += delta;
    }
    Ok(Command::SetShape { at: shape, value: Box::new(with_points(&doc.shapes[shape], &path, pts)) })
}

/// Put one point at an absolute position — what a drag commits.
pub fn set_point(
    doc: &SkinDoc,
    shape: usize,
    point: usize,
    to: Vec2,
) -> Result<Command, EditError> {
    if !to.is_finite() {
        return Err(EditError::NotFinite { at: "the point's position", value: to.x });
    }
    let path = path_of(doc, shape)?;
    let mut pts = path.points.clone();
    if point >= pts.len() {
        return Err(EditError::NoSuchIndex { kind: "point", at: point, len: pts.len() });
    }
    pts[point] = to;
    Ok(Command::SetShape { at: shape, value: Box::new(with_points(&doc.shapes[shape], &path, pts)) })
}

/// Remap `[[shape.weight]]` point numbers after points were inserted or
/// removed. `map` returns the new index of an old point, or `None` if it went
/// away with the geometry it was describing.
fn remap_weights(weights: &[WeightDoc], map: impl Fn(usize) -> Option<usize>) -> Vec<WeightDoc> {
    let mut out: Vec<WeightDoc> = weights
        .iter()
        .filter_map(|w| map(w.point).map(|p| WeightDoc { point: p, ..w.clone() }))
        .collect();
    out.sort_by_key(|w| w.point);
    out
}

/// Append a line-to at the end of a shape's path, or start the path if it is
/// empty. The pen tool's plain click.
pub fn append_point(doc: &SkinDoc, shape: usize, at: Vec2) -> Result<Command, EditError> {
    let path = path_of(doc, shape)?;
    let mut np = path.clone();
    if np.verbs.is_empty() {
        np.move_to(at);
    } else if matches!(np.verbs.last(), Some(Verb::Close)) {
        // A closed subpath is finished; a click starts the next one.
        np.move_to(at);
    } else {
        np.line_to(at);
    }
    let mut out = doc.shapes[shape].clone();
    out.path = np.to_svg();
    // Appending never renumbers an existing point.
    Ok(Command::SetShape { at: shape, value: Box::new(out) })
}

/// Split a segment, putting a new anchor at `at`.
///
/// Curves are split by inserting a `Line` rather than by subdividing the
/// Bézier: the format's `C` and `Q` carry their own control points, and
/// de Casteljau on an edit would silently move the two neighbouring handles.
/// Splitting into a line is the edit the operator asked for and nothing else;
/// dragging the new point's neighbours back into a curve is one more gesture,
/// and it is *their* gesture.
pub fn split_segment(
    doc: &SkinDoc,
    shape: usize,
    after_point: usize,
    at: Vec2,
) -> Result<Command, EditError> {
    let path = path_of(doc, shape)?;
    let Some((vi, _)) = verb_of_point(&path, after_point) else {
        return Err(EditError::NoSuchIndex {
            kind: "point",
            at: after_point,
            len: path.points.len(),
        });
    };
    let mut verbs = path.verbs.clone();
    let mut points = path.points.clone();
    verbs.insert(vi + 1, Verb::Line);
    points.insert(after_point + 1, at);
    let np = Path { verbs, points };
    let mut out = doc.shapes[shape].clone();
    out.path = np.to_svg();
    out.weights = remap_weights(&doc.shapes[shape].weights, |p| {
        Some(if p > after_point { p + 1 } else { p })
    });
    Ok(Command::SetShape { at: shape, value: Box::new(out) })
}

/// Delete a point — and with it the verb that owned it, because a `C` with two
/// points is not a curve.
///
/// Deleting the `M` of a subpath promotes the next verb to a `M` so the
/// remaining geometry keeps a start; deleting the last point of a subpath
/// takes the trailing `Z` with it.
pub fn delete_point(doc: &SkinDoc, shape: usize, point: usize) -> Result<Command, EditError> {
    let path = path_of(doc, shape)?;
    let Some((vi, _)) = verb_of_point(&path, point) else {
        return Err(EditError::NoSuchIndex { kind: "point", at: point, len: path.points.len() });
    };
    // Where this verb's points start.
    let first: usize = path.verbs[..vi].iter().map(|v| v.point_count()).sum();
    let count = path.verbs[vi].point_count();

    let mut verbs = path.verbs.clone();
    let mut points = path.points.clone();
    points.drain(first..first + count);
    let removed_was_move = verbs[vi] == Verb::Move;
    verbs.remove(vi);

    if removed_was_move {
        // Promote whatever follows, unless the subpath is now empty.
        match verbs.get(vi).copied() {
            Some(Verb::Close) | None => {
                if verbs.get(vi) == Some(&Verb::Close) {
                    verbs.remove(vi);
                }
            }
            Some(next) => {
                let n = next.point_count();
                // Keep only the endpoint; a Move takes one point.
                let start: usize = verbs[..vi].iter().map(|v| v.point_count()).sum();
                let end_pt = points[start + n - 1];
                points.drain(start..start + n);
                points.insert(start, end_pt);
                verbs[vi] = Verb::Move;
            }
        }
    }
    // A subpath of one point that is immediately closed is noise.
    if verbs.len() == 2 && verbs[0] == Verb::Move && verbs[1] == Verb::Close {
        verbs.clear();
        points.clear();
    }

    let np = Path { verbs, points };
    let mut out = doc.shapes[shape].clone();
    out.path = np.to_svg();
    let removed_range = first..first + count;
    out.weights = remap_weights(&doc.shapes[shape].weights, |p| {
        if removed_range.contains(&p) {
            None
        } else if p >= removed_range.end {
            Some(p - count)
        } else {
            Some(p)
        }
    });
    Ok(Command::SetShape { at: shape, value: Box::new(out) })
}

/// Close the shape's last open subpath.
pub fn close_subpath(doc: &SkinDoc, shape: usize) -> Result<Command, EditError> {
    let path = path_of(doc, shape)?;
    if path.verbs.is_empty() || matches!(path.verbs.last(), Some(Verb::Close)) {
        return Err(EditError::BadPath {
            at: format!("shape {:?}", doc.shapes[shape].name),
            reason: "there is no open subpath to close".into(),
        });
    }
    let mut np = path.clone();
    np.close();
    let mut out = doc.shapes[shape].clone();
    out.path = np.to_svg();
    Ok(Command::SetShape { at: shape, value: Box::new(out) })
}

/// A new shape, appended on top of everything else.
///
/// It arrives with a fill so it is visible the moment it exists — a shape you
/// cannot see is a shape you cannot edit, and "add shape then wonder where it
/// went" is the worst first thirty seconds an editor can offer.
pub fn new_shape(doc: &SkinDoc, name: &str, path: &Path, fill_color: &str) -> Command {
    let z = doc.shapes.iter().map(|s| s.z).max().unwrap_or(0) + 1;
    let bind = doc.bones.first().map(|b| b.name.clone()).unwrap_or_default();
    Command::InsertShape {
        at: doc.shapes.len(),
        value: Box::new(ShapeDoc {
            name: name.to_string(),
            z,
            opacity: None,
            silhouette: true,
            fill_rule: String::new(),
            path: path.to_svg(),
            bind,
            fill: Some(PaintDoc {
                color: fill_color.to_string(),
                gradient: String::new(),
                alpha: None,
            }),
            stroke: None,
            bind_auto: None,
            weights: Vec::new(),
        }),
    }
}

/// A rounded blob, the shape a new part of a creature usually wants to be.
///
/// SPEC §3.5b: the geometry rule governs chrome, and she is not chrome. The
/// editor's own panels are angular; the thing the editor *makes* starts round,
/// because that is what is correct for her.
pub fn blob(centre: Vec2, radius: f32) -> Path {
    // Four cubics, the standard circle approximation.
    const K: f32 = 0.552_284_8;
    let r = radius;
    let k = r * K;
    let c = centre;
    let mut p = Path::new();
    p.move_to(Vec2::new(c.x, c.y - r));
    p.cubic_to(
        Vec2::new(c.x + k, c.y - r),
        Vec2::new(c.x + r, c.y - k),
        Vec2::new(c.x + r, c.y),
    );
    p.cubic_to(
        Vec2::new(c.x + r, c.y + k),
        Vec2::new(c.x + k, c.y + r),
        Vec2::new(c.x, c.y + r),
    );
    p.cubic_to(
        Vec2::new(c.x - k, c.y + r),
        Vec2::new(c.x - r, c.y + k),
        Vec2::new(c.x - r, c.y),
    );
    p.cubic_to(
        Vec2::new(c.x - r, c.y - k),
        Vec2::new(c.x - k, c.y - r),
        Vec2::new(c.x, c.y - r),
    );
    p.close();
    p
}

/// Set a shape's fill to a named colour or a hex literal.
pub fn set_fill_color(doc: &SkinDoc, shape: usize, color: &str) -> Result<Command, EditError> {
    let s = doc
        .shapes
        .get(shape)
        .ok_or(EditError::NoSuchIndex { kind: "shape", at: shape, len: doc.shapes.len() })?;
    let mut out = s.clone();
    let alpha = out.fill.as_ref().and_then(|f| f.alpha);
    out.fill = Some(PaintDoc { color: color.to_string(), gradient: String::new(), alpha });
    Ok(Command::SetShape { at: shape, value: Box::new(out) })
}

/// Set a shape's fill to a named gradient.
pub fn set_fill_gradient(doc: &SkinDoc, shape: usize, gradient: &str) -> Result<Command, EditError> {
    let s = doc
        .shapes
        .get(shape)
        .ok_or(EditError::NoSuchIndex { kind: "shape", at: shape, len: doc.shapes.len() })?;
    if !doc.gradients.iter().any(|g| g.name == gradient) {
        return Err(EditError::NoSuchName { kind: "gradient", name: gradient.to_string() });
    }
    let mut out = s.clone();
    let alpha = out.fill.as_ref().and_then(|f| f.alpha);
    out.fill =
        Some(PaintDoc { color: String::new(), gradient: gradient.to_string(), alpha });
    Ok(Command::SetShape { at: shape, value: Box::new(out) })
}

/// Set a shape's paint order.
pub fn set_z(doc: &SkinDoc, shape: usize, z: i32) -> Result<Command, EditError> {
    let s = doc
        .shapes
        .get(shape)
        .ok_or(EditError::NoSuchIndex { kind: "shape", at: shape, len: doc.shapes.len() })?;
    Ok(Command::SetShape { at: shape, value: Box::new(ShapeDoc { z, ..s.clone() }) })
}

/// The bounding box of a shape's points, in canvas units.
pub fn bounds_of(doc: &SkinDoc, shape: usize) -> Result<wisp_rig::math::Rect, EditError> {
    Ok(path_of(doc, shape)?.bounds())
}

/// The canvas rectangle, as the document declares it.
pub fn canvas_rect(doc: &SkinDoc) -> wisp_rig::math::Rect {
    let size = Vec2::new(doc.canvas.size[0].0, doc.canvas.size[1].0);
    wisp_rig::math::Rect::new(Vec2::ZERO, size)
}

/// Move the canvas anchor — the point of the canvas that lands on her
/// position on screen.
pub fn set_anchor(doc: &SkinDoc, at: Vec2) -> Command {
    Command::SetCanvas(wisp_rig::skin::doc::CanvasDoc { anchor: to_pt(at), ..doc.canvas.clone() })
}

/// Resize the canvas.
pub fn set_canvas_size(doc: &SkinDoc, size: Vec2) -> Result<Command, EditError> {
    if !(size.x > 0.0 && size.y > 0.0 && size.is_finite()) {
        return Err(EditError::NotFinite { at: "the canvas size", value: size.x });
    }
    Ok(Command::SetCanvas(wisp_rig::skin::doc::CanvasDoc {
        size: [Num(size.x), Num(size.y)],
        ..doc.canvas.clone()
    }))
}

/// Turn a `PathError` into the editor's own refusal, for callers that parse
/// operator-typed path text.
pub fn path_error(shape_name: &str, e: PathError) -> EditError {
    EditError::BadPath { at: format!("shape {shape_name:?}"), reason: e.to_string() }
}
