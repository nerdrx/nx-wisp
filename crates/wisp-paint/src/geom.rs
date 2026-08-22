//! Rectangles and paths. Everything is in **logical pixels with y down**,
//! matching Wayland's surface-local coordinates.

use lyon::path::builder::BorderRadii;
use lyon::path::{Path as LyonPath, Winding};
use wisp_theme::Radius;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Point {
        Point { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const ZERO: Rect = Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };

    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    pub const fn from_size(w: f32, h: f32) -> Rect {
        Rect { x: 0.0, y: 0.0, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn centre(&self) -> Point {
        Point::new(self.x + self.w * 0.5, self.y + self.h * 0.5)
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    pub fn translate(&self, dx: f32, dy: f32) -> Rect {
        Rect { x: self.x + dx, y: self.y + dy, ..*self }
    }

    pub fn inset(&self, d: f32) -> Rect {
        Rect {
            x: self.x + d,
            y: self.y + d,
            w: (self.w - 2.0 * d).max(0.0),
            h: (self.h - 2.0 * d).max(0.0),
        }
    }

    pub fn inset_by(&self, i: wisp_theme::Insets) -> Rect {
        Rect {
            x: self.x + i.left.get(),
            y: self.y + i.top.get(),
            w: (self.w - i.horizontal()).max(0.0),
            h: (self.h - i.vertical()).max(0.0),
        }
    }

    /// Scale about the rect's own centre. The press recipe of DESIGN.md §10.
    pub fn scale_about_centre(&self, s: f32) -> Rect {
        let c = self.centre();
        Rect { x: c.x - self.w * s * 0.5, y: c.y - self.h * s * 0.5, w: self.w * s, h: self.h * s }
    }

    pub fn intersect(&self, o: Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        Rect { x, y, w: (r - x).max(0.0), h: (b - y).max(0.0) }
    }

    pub fn union(&self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return *self;
        }
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        let r = self.right().max(o.right());
        let b = self.bottom().max(o.bottom());
        Rect { x, y, w: r - x, h: b - y }
    }

    /// Normalised 0..1 position of `p` inside this rect. What the gradient
    /// shader is handed per vertex.
    pub fn local(&self, x: f32, y: f32) -> (f32, f32) {
        let w = if self.w.abs() < 1e-6 { 1.0 } else { self.w };
        let h = if self.h.abs() < 1e-6 { 1.0 } else { self.h };
        ((x - self.x) / w, (y - self.y) / h)
    }
}

/// A filled or stroked outline. Thin wrapper over `lyon::path::Path` so the
/// tessellator can stay an implementation detail — if vello ever pins a wgpu
/// we can use, only this file and `tess.rs` change.
#[derive(Debug, Clone)]
pub struct Path {
    pub(crate) inner: LyonPath,
    pub(crate) bbox: Rect,
}

impl Path {
    pub fn bbox(&self) -> Rect {
        self.bbox
    }

    /// The workhorse. `radius` is a theme [`Radius`], so a pill is unaskable.
    pub fn rounded_rect(r: Rect, radius: Radius) -> Path {
        Path::rounded_rect_px(r, radius.px())
    }

    /// Same, but taking raw pixels — for the *few* callers that legitimately
    /// need a non-token radius: a circle via [`Path::circle`], and the atlas
    /// baker replaying a baked scene.
    pub(crate) fn rounded_rect_px(r: Rect, radius: f32) -> Path {
        let radius = radius.min(r.w * 0.5).min(r.h * 0.5).max(0.0);
        let mut b = LyonPath::builder();
        b.add_rounded_rectangle(
            &lyon::geom::Box2D::new(
                lyon::geom::point(r.x, r.y),
                lyon::geom::point(r.right(), r.bottom()),
            ),
            &BorderRadii::new(radius),
            Winding::Positive,
        );
        Path { inner: b.build(), bbox: r }
    }

    pub fn rect(r: Rect) -> Path {
        let mut b = LyonPath::builder();
        b.add_rectangle(
            &lyon::geom::Box2D::new(
                lyon::geom::point(r.x, r.y),
                lyon::geom::point(r.right(), r.bottom()),
            ),
            Winding::Positive,
        );
        Path { inner: b.build(), bbox: r }
    }

    /// A perfect circle — and the only way to draw one is to have named a
    /// sanctioned use to `wisp-theme` first.
    pub fn circle(centre: Point, c: wisp_theme::Circle) -> Path {
        let r = c.radius_px();
        let mut b = LyonPath::builder();
        b.add_circle(lyon::geom::point(centre.x, centre.y), r, Winding::Positive);
        Path {
            inner: b.build(),
            bbox: Rect::new(centre.x - r, centre.y - r, r * 2.0, r * 2.0),
        }
    }

    /// A free-form outline, for the rig's silhouette and the icon set.
    pub fn build(f: impl FnOnce(&mut PathBuilder)) -> Path {
        let mut pb = PathBuilder::new();
        f(&mut pb);
        pb.finish()
    }
}

/// A tiny builder that tracks its own bounding box, because the gradient
/// shader needs one and lyon does not keep it.
pub struct PathBuilder {
    b: lyon::path::path::Builder,
    min: (f32, f32),
    max: (f32, f32),
    open: bool,
}

impl Default for PathBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PathBuilder {
    pub fn new() -> PathBuilder {
        PathBuilder {
            b: LyonPath::builder(),
            min: (f32::MAX, f32::MAX),
            max: (f32::MIN, f32::MIN),
            open: false,
        }
    }

    fn touch(&mut self, x: f32, y: f32) {
        self.min = (self.min.0.min(x), self.min.1.min(y));
        self.max = (self.max.0.max(x), self.max.1.max(y));
    }

    pub fn move_to(&mut self, x: f32, y: f32) -> &mut Self {
        if self.open {
            self.b.end(false);
        }
        self.b.begin(lyon::geom::point(x, y));
        self.open = true;
        self.touch(x, y);
        self
    }

    pub fn line_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.b.line_to(lyon::geom::point(x, y));
        self.touch(x, y);
        self
    }

    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) -> &mut Self {
        self.b.quadratic_bezier_to(lyon::geom::point(cx, cy), lyon::geom::point(x, y));
        self.touch(cx, cy);
        self.touch(x, y);
        self
    }

    pub fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) -> &mut Self {
        self.b.cubic_bezier_to(
            lyon::geom::point(c1x, c1y),
            lyon::geom::point(c2x, c2y),
            lyon::geom::point(x, y),
        );
        self.touch(c1x, c1y);
        self.touch(c2x, c2y);
        self.touch(x, y);
        self
    }

    pub fn close(&mut self) -> &mut Self {
        if self.open {
            self.b.end(true);
            self.open = false;
        }
        self
    }

    pub fn finish(mut self) -> Path {
        if self.open {
            self.b.end(false);
        }
        let bbox = if self.min.0 > self.max.0 {
            Rect::ZERO
        } else {
            Rect::new(self.min.0, self.min.1, self.max.0 - self.min.0, self.max.1 - self.min.1)
        };
        Path { inner: self.b.build(), bbox }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_arithmetic() {
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!(r.right(), 110.0);
        assert_eq!(r.bottom(), 70.0);
        assert_eq!(r.centre(), Point::new(60.0, 45.0));
        assert!(r.contains(Point::new(10.0, 20.0)));
        assert!(!r.contains(Point::new(110.0, 20.0)));
        assert_eq!(r.inset(5.0), Rect::new(15.0, 25.0, 90.0, 40.0));
        assert_eq!(r.inset(1000.0).w, 0.0);
    }

    #[test]
    fn press_scaling_keeps_the_centre() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        let p = r.scale_about_centre(0.96);
        assert_eq!(p.centre(), r.centre());
        assert!((p.w - 96.0).abs() < 1e-4);
    }

    #[test]
    fn intersect_and_union() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert_eq!(a.intersect(b), Rect::new(5.0, 5.0, 5.0, 5.0));
        assert_eq!(a.union(b), Rect::new(0.0, 0.0, 15.0, 15.0));
        assert!(a.intersect(Rect::new(50.0, 50.0, 1.0, 1.0)).is_empty());
        assert_eq!(Rect::ZERO.union(b), b);
    }

    #[test]
    fn local_coordinates_span_zero_to_one() {
        let r = Rect::new(10.0, 10.0, 20.0, 40.0);
        assert_eq!(r.local(10.0, 10.0), (0.0, 0.0));
        assert_eq!(r.local(30.0, 50.0), (1.0, 1.0));
        assert_eq!(r.local(20.0, 30.0), (0.5, 0.5));
        // A degenerate rect must not divide by zero.
        assert!(Rect::new(0.0, 0.0, 0.0, 0.0).local(0.0, 0.0).0.is_finite());
    }

    #[test]
    fn a_rounded_rect_clamps_its_radius_to_the_box() {
        let p = Path::rounded_rect_px(Rect::new(0.0, 0.0, 4.0, 4.0), 6.0);
        assert_eq!(p.bbox(), Rect::new(0.0, 0.0, 4.0, 4.0));
        assert!(p.inner.iter().count() > 0);
    }

    #[test]
    fn a_built_path_tracks_its_bounding_box() {
        let p = Path::build(|b| {
            b.move_to(10.0, 10.0).line_to(30.0, 10.0).line_to(30.0, 40.0).close();
        });
        assert_eq!(p.bbox(), Rect::new(10.0, 10.0, 20.0, 30.0));
    }

    #[test]
    fn an_empty_builder_yields_an_empty_box_not_a_nan() {
        let p = PathBuilder::new().finish();
        assert_eq!(p.bbox(), Rect::ZERO);
    }

    #[test]
    fn a_circle_still_has_to_justify_itself() {
        let c = wisp_theme::Circle::status_dot(6.0);
        let p = Path::circle(Point::new(10.0, 10.0), c);
        assert_eq!(p.bbox(), Rect::new(7.0, 7.0, 6.0, 6.0));
    }
}
