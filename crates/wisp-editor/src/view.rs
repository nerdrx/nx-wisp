//! The canvas viewport: pan, zoom, and the two coordinate spaces.
//!
//! A skin authors **canvas units** and knows nothing about pixels (that is
//! what makes her resolution independent — F75). The editor draws into a
//! rectangle of **surface pixels**. Everything the operator points at arrives
//! in pixels and every edit must land in canvas units, so exactly one place is
//! allowed to convert, and this is it.
//!
//! Zoom is *pixels per canvas unit*, which makes the tolerance rule fall out:
//! a grab radius the operator feels is a constant number of **pixels**, so in
//! canvas units it is `pixels / zoom`. Halve the zoom and the same click grabs
//! a point twice as far away in the document — which is what "the handles look
//! the same size at every zoom" means when you write it down.

use wisp_paint::geom::{Point, Rect};
use wisp_rig::math::Vec2;

/// How close, in **pixels**, the pointer has to be to grab a handle. Half of
/// the 16px icon box: a handle is drawn at 7px across, so this is "inside the
/// handle, plus a forgiving ring".
pub const GRAB_PX: f32 = 8.0;

/// Zoom limits. Below the floor the whole rig is a smudge; above the ceiling
/// one canvas unit is a screenful and the handles overlap into mush.
pub const MIN_ZOOM: f32 = 0.05;
pub const MAX_ZOOM: f32 = 64.0;

/// One step of the scroll wheel. `2^(1/4)` — four notches doubles the zoom,
/// which is the ratio that feels right at a trackpad's resolution.
pub const ZOOM_STEP: f32 = 1.189_207_1;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Where the canvas is drawn, in surface pixels.
    pub rect: Rect,
    /// The canvas coordinate shown at `rect`'s top-left corner.
    pub origin: Vec2,
    /// Pixels per canvas unit.
    pub zoom: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport { rect: Rect::from_size(640.0, 480.0), origin: Vec2::ZERO, zoom: 1.0 }
    }
}

impl Viewport {
    pub fn new(rect: Rect) -> Viewport {
        Viewport { rect, origin: Vec2::ZERO, zoom: 1.0 }
    }

    /// Canvas units to surface pixels.
    pub fn to_screen(&self, p: Vec2) -> Point {
        Point::new(
            self.rect.x + (p.x - self.origin.x) * self.zoom,
            self.rect.y + (p.y - self.origin.y) * self.zoom,
        )
    }

    /// Surface pixels to canvas units.
    pub fn to_canvas(&self, p: Point) -> Vec2 {
        Vec2::new(
            self.origin.x + (p.x - self.rect.x) / self.zoom,
            self.origin.y + (p.y - self.rect.y) / self.zoom,
        )
    }

    /// A length in pixels, expressed in canvas units. The grab radius uses it.
    pub fn to_canvas_len(&self, px: f32) -> f32 {
        if self.zoom.abs() < 1e-9 {
            f32::INFINITY
        } else {
            px / self.zoom
        }
    }

    /// The default grab radius, in canvas units, at this zoom.
    pub fn grab_radius(&self) -> f32 {
        self.to_canvas_len(GRAB_PX)
    }

    /// The part of the canvas that is currently on screen.
    pub fn visible(&self) -> (Vec2, Vec2) {
        let br = self.to_canvas(Point::new(self.rect.right(), self.rect.bottom()));
        (self.origin, br)
    }

    /// Drag the canvas under the pointer by a pixel delta.
    pub fn pan_by_px(&mut self, dx: f32, dy: f32) {
        if self.zoom.abs() < 1e-9 {
            return;
        }
        self.origin.x -= dx / self.zoom;
        self.origin.y -= dy / self.zoom;
    }

    /// Zoom about a fixed point on screen — the canvas point under the cursor
    /// does not move, which is the only zoom that does not feel like a lurch.
    pub fn zoom_about(&mut self, anchor: Point, factor: f32) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let before = self.to_canvas(anchor);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let after = self.to_canvas(anchor);
        self.origin.x += before.x - after.x;
        self.origin.y += before.y - after.y;
    }

    /// One wheel notch. Positive is towards the operator: zoom in.
    pub fn wheel(&mut self, anchor: Point, notches: f32) {
        self.zoom_about(anchor, ZOOM_STEP.powf(notches));
    }

    /// Frame a canvas of `size` units inside the viewport, with a margin in
    /// pixels. Total: the result is always inside the limits.
    pub fn fit(&mut self, size: Vec2, margin_px: f32) {
        let w = (self.rect.w - margin_px * 2.0).max(1.0);
        let h = (self.rect.h - margin_px * 2.0).max(1.0);
        let sx = w / size.x.max(1e-3);
        let sy = h / size.y.max(1e-3);
        self.zoom = sx.min(sy).clamp(MIN_ZOOM, MAX_ZOOM);
        // Centre it.
        let used_w = size.x * self.zoom;
        let used_h = size.y * self.zoom;
        self.origin = Vec2::new(
            -((self.rect.w - used_w) * 0.5) / self.zoom,
            -((self.rect.h - used_h) * 0.5) / self.zoom,
        );
    }

    /// Move the viewport so `p` sits in the middle of it.
    pub fn centre_on(&mut self, p: Vec2) {
        self.origin = Vec2::new(
            p.x - (self.rect.w * 0.5) / self.zoom,
            p.y - (self.rect.h * 0.5) / self.zoom,
        );
    }

    /// Is this canvas point currently on screen?
    pub fn contains(&self, p: Vec2) -> bool {
        self.rect.contains(self.to_screen(p))
    }
}
