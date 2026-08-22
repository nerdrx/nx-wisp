//! What the rig hands to the renderer.
//!
//! These are this crate's own output types. `wisp-rig` does **not** depend on
//! `wisp-paint`: it produces geometry and a description of how that geometry
//! should be filled, and the renderer maps a [`DrawShape`] onto a vello path
//! and brush. The two crates can then be built, changed and tested completely
//! independently, and every test in this crate runs with no GPU (SPEC §4).
//!
//! A [`RigFrame`] is **retained and reused**. Shape count, verb lists, names
//! and gradient stop arrays are set up once, and only the point arrays and
//! gradient geometry are written each frame — so a 60 fps rig allocates
//! nothing.
//!
//! Coordinates are surface pixels, y down, already scaled and positioned.

use crate::math::{Rect, Vec2};
use crate::paint::{FillRule, Paint, Stroke};
use crate::path::{flatten_into, segments, Segments, Verb};

#[derive(Debug, Clone, PartialEq)]
pub struct DrawShape {
    pub name: Box<str>,
    /// Paint order. The frame's shapes are already sorted by it.
    pub z: i32,
    /// Shape opacity times the inherited bone alpha, `0..=1`.
    pub opacity: f32,
    /// Does this shape count towards the click-through outline (F2)?
    pub silhouette: bool,
    pub fill_rule: FillRule,
    /// Static — set once when the frame is built for a skin.
    pub verbs: Vec<Verb>,
    /// Deformed, in surface pixels. Overwritten every frame.
    pub points: Vec<Vec2>,
    pub fill: Option<Paint>,
    pub stroke: Option<Stroke>,
}

impl DrawShape {
    pub fn segments(&self) -> Segments<'_> {
        segments(&self.verbs, &self.points)
    }

    /// Flatten to polylines for hit testing or contour tracing.
    pub fn flatten_into(&self, per_curve: usize, out: &mut Vec<Vec<Vec2>>) {
        flatten_into(&self.verbs, &self.points, per_curve, out)
    }

    pub fn bounds(&self) -> Rect {
        let mut r = Rect::EMPTY;
        for p in &self.points {
            r.union_point(*p);
        }
        r
    }

    /// Would the renderer draw anything at all for this shape?
    pub fn is_visible(&self) -> bool {
        self.opacity > 0.004
            && self.points.len() >= 2
            && (self.fill.is_some() || self.stroke.is_some())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RigFrame {
    /// The size she was asked to render at, in surface pixels (F75).
    pub size_px: f32,
    /// Surface pixels per canvas unit.
    pub scale: f32,
    /// Where the canvas anchor sits, in surface pixels.
    pub anchor: Vec2,
    /// Back to front.
    pub shapes: Vec<DrawShape>,
    /// Union of every visible shape's bounds, in surface pixels.
    pub bounds: Rect,
}

impl RigFrame {
    pub fn shape(&self, name: &str) -> Option<&DrawShape> {
        self.shapes.iter().find(|s| &*s.name == name)
    }

    /// Polylines of every silhouette shape — the input to
    /// [`crate::contour::trace`].
    pub fn silhouette_rings(&self, per_curve: usize) -> Vec<Vec<Vec2>> {
        let mut rings = Vec::new();
        let mut buf = Vec::new();
        for s in &self.shapes {
            if !s.silhouette || !s.is_visible() {
                continue;
            }
            s.flatten_into(per_curve, &mut buf);
            rings.append(&mut buf);
        }
        rings
    }

    pub fn recompute_bounds(&mut self) {
        let mut r = Rect::EMPTY;
        for s in &self.shapes {
            if !s.is_visible() {
                continue;
            }
            for p in &s.points {
                r.union_point(*p);
            }
        }
        self.bounds = r;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paint::{nx, Rgba};

    fn shape(name: &str, silhouette: bool, opacity: f32) -> DrawShape {
        DrawShape {
            name: name.into(),
            z: 0,
            opacity,
            silhouette,
            fill_rule: FillRule::NonZero,
            verbs: vec![Verb::Move, Verb::Line, Verb::Line, Verb::Close],
            points: vec![
                Vec2::new(0.0, 0.0),
                Vec2::new(10.0, 0.0),
                Vec2::new(10.0, 10.0),
            ],
            fill: Some(Paint::Solid(nx::VIOLET)),
            stroke: None,
        }
    }

    #[test]
    fn bounds_cover_every_visible_shape() {
        let mut f = RigFrame::default();
        f.shapes.push(shape("a", true, 1.0));
        let mut b = shape("b", true, 1.0);
        b.points = vec![
            Vec2::new(100.0, 100.0),
            Vec2::new(110.0, 100.0),
            Vec2::new(110.0, 110.0),
        ];
        f.shapes.push(b);
        f.recompute_bounds();
        assert_eq!(f.bounds.min, Vec2::new(0.0, 0.0));
        assert_eq!(f.bounds.max, Vec2::new(110.0, 110.0));
    }

    #[test]
    fn invisible_shapes_are_excluded_from_bounds() {
        let mut f = RigFrame::default();
        f.shapes.push(shape("a", true, 1.0));
        let mut ghost = shape("ghost", true, 0.0);
        ghost.points = vec![
            Vec2::new(900.0, 900.0),
            Vec2::new(910.0, 900.0),
            Vec2::new(910.0, 910.0),
        ];
        f.shapes.push(ghost);
        f.recompute_bounds();
        assert_eq!(f.bounds.max, Vec2::new(10.0, 10.0));
    }

    #[test]
    fn a_shape_with_no_paint_is_not_visible() {
        let mut s = shape("a", true, 1.0);
        s.fill = None;
        assert!(!s.is_visible());
        s.stroke = Some(Stroke {
            paint: Paint::Solid(Rgba::WHITE),
            width: 1.0,
            cap: Default::default(),
            join: Default::default(),
        });
        assert!(s.is_visible());
    }

    #[test]
    fn silhouette_rings_skip_non_silhouette_and_invisible_shapes() {
        let mut f = RigFrame::default();
        f.shapes.push(shape("body", true, 1.0));
        f.shapes.push(shape("aura", false, 1.0));
        f.shapes.push(shape("hidden", true, 0.0));
        assert_eq!(f.silhouette_rings(4).len(), 1);
    }

    #[test]
    fn shapes_are_addressable_by_name() {
        let mut f = RigFrame::default();
        f.shapes.push(shape("shell", true, 1.0));
        assert!(f.shape("shell").is_some());
        assert!(f.shape("nothing").is_none());
    }

    #[test]
    fn segments_walk_the_deformed_points() {
        let s = shape("a", true, 1.0);
        let segs: Vec<_> = s.segments().collect();
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[0].1[0], Vec2::new(0.0, 0.0));
    }
}
