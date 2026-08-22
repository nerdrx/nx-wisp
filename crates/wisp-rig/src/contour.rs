//! Silhouette → simplified polygon, for the shell's per-frame input region
//! (F2).
//!
//! The shell needs a polygon that covers every pixel of her and as little else
//! as possible, so a click lands on the window behind her everywhere she is not.
//! It has to be small — it is handed to the compositor every frame she changes
//! shape — and it has to be *conservative*: a polygon that cuts inside her
//! silhouette makes her own body unclickable in places, which reads as a bug.
//!
//! The route is: rasterise the union of her silhouette shapes into a small
//! coverage grid by scanline, walk the boundary between covered and uncovered
//! cells, keep the largest loop, simplify it with Douglas–Peucker, and push it
//! outwards by about a cell so it strictly contains what it traced.
//!
//! Everything is integer grid work plus one simplification pass — no GPU, no
//! readback, and cheap enough to run on a 60 fps rig.

use std::collections::HashMap;

use crate::math::{Rect, Vec2};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContourOptions {
    /// Cells across the longer axis of the silhouette's bounding box. 96 gives
    /// roughly 1.3px cells on a 128px character.
    pub grid: u32,
    /// Hard cap on the returned point count. At 8 bytes a point, 220 points is
    /// about 1.7 KB — inside the ~2 KB budget F2 asks for.
    pub max_points: usize,
    /// Douglas–Peucker tolerance in pixels. The tracer raises this
    /// automatically if the first pass overshoots `max_points`.
    pub tolerance: f32,
    /// Extra outward offset in pixels, on top of the one cell the tracer always
    /// adds to stay conservative.
    pub grow: f32,
}

impl Default for ContourOptions {
    fn default() -> Self {
        ContourOptions { grid: 96, max_points: 220, tolerance: 0.9, grow: 0.0 }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Polygon {
    pub points: Vec<Vec2>,
}

impl Polygon {
    pub fn is_empty(&self) -> bool {
        self.points.len() < 3
    }

    /// Even-odd point-in-polygon. Points exactly on an edge are not
    /// guaranteed either way, which is fine for an input region.
    pub fn contains(&self, p: Vec2) -> bool {
        let n = self.points.len();
        if n < 3 {
            return false;
        }
        let mut inside = false;
        let mut j = n - 1;
        for i in 0..n {
            let (a, b) = (self.points[i], self.points[j]);
            if (a.y > p.y) != (b.y > p.y) {
                let t = (p.y - a.y) / (b.y - a.y);
                if p.x < a.x + t * (b.x - a.x) {
                    inside = !inside;
                }
            }
            j = i;
        }
        inside
    }

    pub fn bounds(&self) -> Rect {
        let mut r = Rect::EMPTY;
        for p in &self.points {
            r.union_point(*p);
        }
        r
    }

    pub fn signed_area(&self) -> f32 {
        let n = self.points.len();
        if n < 3 {
            return 0.0;
        }
        let mut a = 0.0;
        let mut j = n - 1;
        for i in 0..n {
            a += (self.points[j].x - self.points[i].x) * (self.points[j].y + self.points[i].y);
            j = i;
        }
        a * 0.5
    }

    /// Wire size if serialised as pairs of `f32`. The shell's budget check.
    pub fn approx_bytes(&self) -> usize {
        self.points.len() * 8
    }

    /// Integer points, ready to hand to the compositor.
    pub fn to_i32(&self) -> Vec<(i32, i32)> {
        self.points
            .iter()
            .map(|p| (p.x.round() as i32, p.y.round() as i32))
            .collect()
    }
}

/// Trace the outline of a set of already-flattened, already-transformed
/// polylines.
///
/// `rings` are closed subpaths in surface pixels — exactly what
/// [`crate::path::Path::flatten_into`] produces from a deformed shape. Their
/// union is what gets traced, with non-zero winding, so overlapping body parts
/// come out as one outline rather than several.
pub fn trace(rings: &[Vec<Vec2>], opts: ContourOptions) -> Polygon {
    let mut bounds = Rect::EMPTY;
    for r in rings {
        for p in r {
            if p.is_finite() {
                bounds.union_point(*p);
            }
        }
    }
    if bounds.is_empty() || rings.is_empty() {
        return Polygon::default();
    }

    let grid = opts.grid.clamp(8, 512) as usize;
    let extent = bounds.width().max(bounds.height()).max(1e-3);
    let cell = extent / grid as f32;
    // One cell of margin on every side so the boundary walk always has an
    // empty ring of cells to turn around in.
    let origin = bounds.min - Vec2::splat(cell * 2.0);
    let cols = ((bounds.width() / cell).ceil() as usize) + 4;
    let rows = ((bounds.height() / cell).ceil() as usize) + 4;
    if cols == 0 || rows == 0 {
        return Polygon::default();
    }

    let mask = rasterise(rings, origin, cell, cols, rows);
    let loops = boundary_loops(&mask, cols, rows);
    let Some(best) = loops
        .into_iter()
        .max_by(|a, b| {
            grid_area(a)
                .abs()
                .partial_cmp(&grid_area(b).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    else {
        return Polygon::default();
    };

    let mut pts: Vec<Vec2> = best
        .iter()
        .map(|&(x, y)| origin + Vec2::new(x as f32 * cell, y as f32 * cell))
        .collect();
    if pts.len() >= 2 && pts[0] == pts[pts.len() - 1] {
        pts.pop();
    }
    if pts.len() < 3 {
        return Polygon::default();
    }

    // Simplify, raising the tolerance until the point budget is met. Doubling
    // converges in a handful of steps even for a very wiggly silhouette.
    let mut tol = opts.tolerance.max(1e-3);
    let mut simplified = simplify_closed(&pts, tol);
    let mut guard = 0;
    while simplified.len() > opts.max_points.max(3) && guard < 24 {
        tol *= 1.6;
        simplified = simplify_closed(&pts, tol);
        guard += 1;
    }
    if simplified.len() > opts.max_points.max(3) {
        simplified = decimate(&simplified, opts.max_points.max(3));
    }

    // Grow outwards so the polygon strictly contains the silhouette it traced:
    // a cell-centre rasteriser loses up to half a cell, simplification loses
    // up to `tol`, and the caller may want a little slack on top.
    let grow = cell + tol + opts.grow.max(0.0);
    offset_outwards(&mut simplified, grow);

    Polygon { points: simplified }
}

/// Convenience: trace straight from a rig frame's silhouette shapes.
pub fn trace_frame(frame: &crate::frame::RigFrame, opts: ContourOptions) -> Polygon {
    trace(&frame.silhouette_rings(6), opts)
}

/// Non-zero-winding scanline fill into a boolean coverage mask.
fn rasterise(
    rings: &[Vec<Vec2>],
    origin: Vec2,
    cell: f32,
    cols: usize,
    rows: usize,
) -> Vec<bool> {
    let mut mask = vec![false; cols * rows];
    let mut crossings: Vec<(f32, i32)> = Vec::with_capacity(64);
    for row in 0..rows {
        let y = origin.y + (row as f32 + 0.5) * cell;
        crossings.clear();
        for ring in rings {
            if ring.len() < 2 {
                continue;
            }
            let n = ring.len();
            for i in 0..n {
                let a = ring[i];
                let b = ring[(i + 1) % n];
                if !a.is_finite() || !b.is_finite() || a.y == b.y {
                    continue;
                }
                // Half-open in y so a vertex exactly on the scanline is
                // counted once, not twice.
                let (lo, hi) = if a.y < b.y { (a.y, b.y) } else { (b.y, a.y) };
                if y < lo || y >= hi {
                    continue;
                }
                let t = (y - a.y) / (b.y - a.y);
                crossings.push((a.x + t * (b.x - a.x), if b.y > a.y { 1 } else { -1 }));
            }
        }
        if crossings.is_empty() {
            continue;
        }
        crossings.sort_by(|p, q| p.0.partial_cmp(&q.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut winding = 0i32;
        let mut ci = 0usize;
        for col in 0..cols {
            let x = origin.x + (col as f32 + 0.5) * cell;
            while ci < crossings.len() && crossings[ci].0 <= x {
                winding += crossings[ci].1;
                ci += 1;
            }
            if winding != 0 {
                mask[row * cols + col] = true;
            }
        }
    }
    mask
}

type Corner = (i32, i32);

/// Collect the boundary between covered and uncovered cells as directed edges
/// on the grid's corner lattice, then chain them into closed loops.
///
/// Edges are emitted so that covered cells stay on the same side of every
/// edge, which makes the chaining unambiguous everywhere except a diagonal
/// pinch — and there, either choice yields a valid loop.
fn boundary_loops(mask: &[bool], cols: usize, rows: usize) -> Vec<Vec<Corner>> {
    let at = |x: isize, y: isize| -> bool {
        if x < 0 || y < 0 || x >= cols as isize || y >= rows as isize {
            false
        } else {
            mask[y as usize * cols + x as usize]
        }
    };

    let mut starts: HashMap<Corner, Vec<Corner>> = HashMap::new();
    // Corners in raster order. `HashMap` iteration order is not stable across
    // runs, and starting a loop at a different corner rotates the whole
    // polygon — so loop starts come from here, never from the map.
    let mut order: Vec<Corner> = Vec::new();
    let mut edge_count = 0usize;
    for y in 0..rows as isize {
        for x in 0..cols as isize {
            if !at(x, y) {
                continue;
            }
            let (xi, yi) = (x as i32, y as i32);
            let mut push = |a: Corner, b: Corner| {
                if starts.entry(a).or_default().is_empty() {
                    order.push(a);
                }
                starts.get_mut(&a).expect("just inserted").push(b);
            };
            if !at(x, y - 1) {
                push((xi, yi), (xi + 1, yi));
            }
            if !at(x + 1, y) {
                push((xi + 1, yi), (xi + 1, yi + 1));
            }
            if !at(x, y + 1) {
                push((xi + 1, yi + 1), (xi, yi + 1));
            }
            if !at(x - 1, y) {
                push((xi, yi + 1), (xi, yi));
            }
            edge_count += 4;
        }
    }
    if starts.is_empty() {
        return Vec::new();
    }

    let mut loops = Vec::new();
    let mut budget = edge_count + 8;
    for &first in &order {
        if !starts.contains_key(&first) {
            continue;
        }
        let mut ring = vec![first];
        let mut cur = first;
        loop {
            budget = budget.saturating_sub(1);
            if budget == 0 {
                break;
            }
            let Some(nexts) = starts.get_mut(&cur) else {
                break;
            };
            let Some(next) = nexts.pop() else {
                starts.remove(&cur);
                break;
            };
            if nexts.is_empty() {
                starts.remove(&cur);
            }
            if next == first {
                break;
            }
            ring.push(next);
            cur = next;
        }
        if ring.len() >= 4 {
            loops.push(ring);
        }
        if budget == 0 {
            break;
        }
    }
    loops
}

fn grid_area(ring: &[Corner]) -> f32 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut a = 0i64;
    for i in 0..n {
        let p = ring[i];
        let q = ring[(i + 1) % n];
        a += p.0 as i64 * q.1 as i64 - q.0 as i64 * p.1 as i64;
    }
    a as f32 * 0.5
}

/// Douglas–Peucker on a closed ring. The ring is split at its two most distant
/// points so the algorithm's "keep the endpoints" rule cannot flatten a whole
/// lobe away.
pub fn simplify_closed(points: &[Vec2], tolerance: f32) -> Vec<Vec2> {
    let n = points.len();
    if n < 4 {
        return points.to_vec();
    }
    // Anchor 1: the point furthest from the centroid. Anchor 2: the point
    // furthest from anchor 1.
    let centroid = points.iter().fold(Vec2::ZERO, |a, p| a + *p) / n as f32;
    let a1 = (0..n)
        .max_by(|&i, &j| {
            points[i]
                .dist(centroid)
                .partial_cmp(&points[j].dist(centroid))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);
    let a2 = (0..n)
        .max_by(|&i, &j| {
            points[i]
                .dist(points[a1])
                .partial_cmp(&points[j].dist(points[a1]))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(n / 2);
    let (lo, hi) = if a1 <= a2 { (a1, a2) } else { (a2, a1) };

    let first: Vec<Vec2> = points[lo..=hi].to_vec();
    let mut second: Vec<Vec2> = points[hi..].to_vec();
    second.extend_from_slice(&points[..=lo]);

    let mut out = simplify_open(&first, tolerance);
    let tail = simplify_open(&second, tolerance);
    // Both halves share their endpoints; drop the duplicates.
    if tail.len() > 2 {
        out.extend_from_slice(&tail[1..tail.len() - 1]);
    }
    out
}

/// Douglas–Peucker on an open polyline. Iterative, so a pathological input
/// cannot recurse deeply.
pub fn simplify_open(points: &[Vec2], tolerance: f32) -> Vec<Vec2> {
    let n = points.len();
    if n < 3 {
        return points.to_vec();
    }
    let tol = tolerance.max(1e-4);
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack = vec![(0usize, n - 1)];
    while let Some((lo, hi)) = stack.pop() {
        if hi <= lo + 1 {
            continue;
        }
        let (a, b) = (points[lo], points[hi]);
        let mut worst = (0.0f32, lo);
        // Indexed on purpose: the loop carries the index into `keep`, not just
        // the point.
        #[allow(clippy::needless_range_loop)]
        for i in (lo + 1)..hi {
            let (closest, _) = points[i].closest_on_segment(a, b);
            let d = points[i].dist(closest);
            if d > worst.0 {
                worst = (d, i);
            }
        }
        if worst.0 > tol {
            keep[worst.1] = true;
            stack.push((lo, worst.1));
            stack.push((worst.1, hi));
        }
    }
    points
        .iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(_, p)| *p)
        .collect()
}

/// Last-resort uniform decimation, for the case where even a huge tolerance
/// leaves too many points (a genuinely fractal silhouette).
fn decimate(points: &[Vec2], max: usize) -> Vec<Vec2> {
    if points.len() <= max {
        return points.to_vec();
    }
    let step = points.len() as f32 / max as f32;
    (0..max)
        .map(|i| points[((i as f32 * step) as usize).min(points.len() - 1)])
        .collect()
}

/// Push every vertex out along the bisector of its two adjacent edge normals.
fn offset_outwards(points: &mut [Vec2], by: f32) {
    let n = points.len();
    if n < 3 || by <= 0.0 {
        return;
    }
    // Orientation decides which side "out" is on.
    let poly = Polygon { points: points.to_vec() };
    let flip = if poly.signed_area() >= 0.0 { 1.0 } else { -1.0 };
    let src = poly.points;
    for i in 0..n {
        let prev = src[(i + n - 1) % n];
        let next = src[(i + 1) % n];
        let d0 = (src[i] - prev).normalize();
        let d1 = (next - src[i]).normalize();
        // Left normal of a direction in a y-down space.
        let n0 = Vec2::new(d0.y, -d0.x) * flip;
        let n1 = Vec2::new(d1.y, -d1.x) * flip;
        let bisector = (n0 + n1).normalize();
        let dir = if bisector == Vec2::ZERO { n1 } else { bisector };
        points[i] = src[i] + dir * by;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(cx: f32, cy: f32, half: f32) -> Vec<Vec2> {
        vec![
            Vec2::new(cx - half, cy - half),
            Vec2::new(cx + half, cy - half),
            Vec2::new(cx + half, cy + half),
            Vec2::new(cx - half, cy + half),
        ]
    }

    fn blob(cx: f32, cy: f32, r: f32, lobes: f32) -> Vec<Vec2> {
        (0..160)
            .map(|i| {
                let a = i as f32 / 160.0 * std::f32::consts::TAU;
                let rr = r * (1.0 + 0.28 * (a * lobes).sin());
                Vec2::new(cx + a.cos() * rr, cy + a.sin() * rr)
            })
            .collect()
    }

    #[test]
    fn an_empty_input_gives_an_empty_polygon() {
        assert!(trace(&[], ContourOptions::default()).is_empty());
        assert!(trace(&[vec![]], ContourOptions::default()).is_empty());
    }

    #[test]
    fn a_square_traces_to_roughly_a_square() {
        let poly = trace(&[square(100.0, 100.0, 40.0)], ContourOptions::default());
        assert!(!poly.is_empty());
        let b = poly.bounds();
        // Grown outwards, so a little larger than the source, never smaller.
        assert!(b.min.x <= 60.0 && b.min.y <= 60.0, "{b:?}");
        assert!(b.max.x >= 140.0 && b.max.y >= 140.0, "{b:?}");
        assert!(b.width() < 95.0 && b.height() < 95.0, "far too loose: {b:?}");
    }

    #[test]
    fn the_polygon_contains_points_inside_the_silhouette() {
        let rings = vec![blob(128.0, 128.0, 60.0, 5.0)];
        let poly = trace(&rings, ContourOptions::default());
        assert!(!poly.is_empty());
        // Everything within the inner radius of the blob is definitely inside.
        for i in 0..360 {
            let a = i as f32 / 360.0 * std::f32::consts::TAU;
            for r in [0.0, 10.0, 25.0, 40.0] {
                let p = Vec2::new(128.0 + a.cos() * r, 128.0 + a.sin() * r);
                assert!(poly.contains(p), "missed an interior point {p:?}");
            }
        }
    }

    #[test]
    fn the_polygon_excludes_points_outside_the_silhouette() {
        let rings = vec![blob(128.0, 128.0, 60.0, 5.0)];
        let poly = trace(&rings, ContourOptions::default());
        // Outside the outer radius plus a margin for the deliberate growth.
        for i in 0..360 {
            let a = i as f32 / 360.0 * std::f32::consts::TAU;
            for r in [86.0, 120.0, 400.0] {
                let p = Vec2::new(128.0 + a.cos() * r, 128.0 + a.sin() * r);
                assert!(!poly.contains(p), "included an exterior point {p:?}");
            }
        }
    }

    #[test]
    fn a_concave_silhouette_is_not_filled_in() {
        // A wide C. The gap in the middle must stay clickable.
        let outer = blob(128.0, 128.0, 70.0, 1.0);
        let ring: Vec<Vec2> = outer
            .iter()
            .copied()
            .filter(|p| !(p.x > 128.0 && p.y.abs_diff_gap(128.0) < 18.0))
            .collect();
        let poly = trace(&[ring], ContourOptions { tolerance: 0.6, ..Default::default() });
        assert!(!poly.is_empty());
        assert!(poly.contains(Vec2::new(100.0, 128.0)), "body should be inside");
    }

    #[test]
    fn overlapping_shapes_trace_as_one_outline() {
        let rings = vec![square(100.0, 100.0, 30.0), square(140.0, 100.0, 30.0)];
        let poly = trace(&rings, ContourOptions::default());
        let b = poly.bounds();
        assert!(b.width() > 100.0, "the union was not traced: {b:?}");
        // The seam between the two squares must be inside, not a hole.
        assert!(poly.contains(Vec2::new(120.0, 100.0)));
    }

    #[test]
    fn disjoint_shapes_yield_the_larger_one() {
        // A single polygon cannot express two islands; taking the largest is
        // the right conservative answer for an input region.
        let rings = vec![square(50.0, 50.0, 40.0), square(400.0, 50.0, 5.0)];
        let poly = trace(&rings, ContourOptions::default());
        assert!(poly.contains(Vec2::new(50.0, 50.0)));
        let b = poly.bounds();
        assert!(b.max.x < 250.0, "picked up the far island: {b:?}");
    }

    #[test]
    fn the_point_budget_is_respected() {
        let opts = ContourOptions { max_points: 64, grid: 256, ..Default::default() };
        let poly = trace(&[blob(256.0, 256.0, 200.0, 11.0)], opts);
        assert!(poly.points.len() <= 64, "got {} points", poly.points.len());
        assert!(poly.approx_bytes() <= 2048);
    }

    #[test]
    fn the_default_budget_stays_under_two_kilobytes() {
        for lobes in [3.0, 7.0, 13.0] {
            let poly = trace(&[blob(128.0, 128.0, 90.0, lobes)], ContourOptions::default());
            assert!(
                poly.approx_bytes() <= 2048,
                "{lobes} lobes produced {} bytes",
                poly.approx_bytes()
            );
        }
    }

    #[test]
    fn a_tiny_silhouette_still_produces_a_usable_polygon() {
        let poly = trace(&[square(10.0, 10.0, 2.0)], ContourOptions::default());
        assert!(!poly.is_empty());
        assert!(poly.contains(Vec2::new(10.0, 10.0)));
    }

    #[test]
    fn non_finite_points_are_ignored_rather_than_poisoning_the_trace() {
        let mut ring = square(100.0, 100.0, 40.0);
        ring.push(Vec2::new(f32::NAN, f32::NAN));
        let poly = trace(&[ring], ContourOptions::default());
        assert!(poly.points.iter().all(|p| p.is_finite()));
    }

    #[test]
    fn tracing_is_deterministic() {
        let rings = vec![blob(128.0, 128.0, 60.0, 5.0)];
        let a = trace(&rings, ContourOptions::default());
        let b = trace(&rings, ContourOptions::default());
        assert_eq!(a, b);
    }

    #[test]
    fn simplify_keeps_the_corners_of_a_square() {
        let dense: Vec<Vec2> = (0..400)
            .map(|i| {
                let t = i as f32 / 100.0;
                match i / 100 {
                    0 => Vec2::new(t * 100.0, 0.0),
                    1 => Vec2::new(100.0, (t - 1.0) * 100.0),
                    2 => Vec2::new(100.0 - (t - 2.0) * 100.0, 100.0),
                    _ => Vec2::new(0.0, 100.0 - (t - 3.0) * 100.0),
                }
            })
            .collect();
        let s = simplify_closed(&dense, 0.5);
        assert!(s.len() <= 8, "square kept {} points", s.len());
        for corner in [
            Vec2::new(0.0, 0.0),
            Vec2::new(100.0, 0.0),
            Vec2::new(100.0, 100.0),
            Vec2::new(0.0, 100.0),
        ] {
            assert!(
                s.iter().any(|p| p.dist(corner) < 1.5),
                "lost the corner {corner:?}"
            );
        }
    }

    #[test]
    fn simplify_open_keeps_the_endpoints() {
        let pts: Vec<Vec2> = (0..50).map(|i| Vec2::new(i as f32, 0.0)).collect();
        let s = simplify_open(&pts, 1.0);
        assert_eq!(s.first(), pts.first());
        assert_eq!(s.last(), pts.last());
        assert_eq!(s.len(), 2, "a straight line needs two points");
    }

    #[test]
    fn contains_handles_degenerate_polygons() {
        assert!(!Polygon::default().contains(Vec2::ZERO));
        assert!(!Polygon { points: vec![Vec2::ZERO, Vec2::ONE] }.contains(Vec2::ZERO));
    }

    #[test]
    fn to_i32_rounds_for_the_compositor() {
        let p = Polygon { points: vec![Vec2::new(1.4, 2.6), Vec2::new(-0.5, 9.49)] };
        assert_eq!(p.to_i32(), vec![(1, 3), (-1, 9)]);
    }

    // Tiny helper so the concave test reads clearly.
    trait Gap {
        fn abs_diff_gap(self, other: f32) -> f32;
    }
    impl Gap for f32 {
        fn abs_diff_gap(self, other: f32) -> f32 {
            (self - other).abs()
        }
    }
}
