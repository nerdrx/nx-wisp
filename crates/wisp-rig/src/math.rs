//! The only geometry primitives this crate uses.
//!
//! Deliberately tiny and dependency-free: `wisp-rig` must stay pure so it can
//! be unit-tested with no GPU and no compositor (SPEC §4), and it must not
//! borrow types from `wisp-paint` — the renderer maps these onto its own.

use serde::{Deserialize, Serialize};

pub const TAU: f32 = std::f32::consts::TAU;
pub const PI: f32 = std::f32::consts::PI;

/// Convert authored degrees (what a skin file carries) to internal radians.
#[inline]
pub fn deg_to_rad(d: f32) -> f32 {
    d * (PI / 180.0)
}

/// Convert internal radians back to authored degrees (for serialisation).
#[inline]
pub fn rad_to_deg(r: f32) -> f32 {
    r * (180.0 / PI)
}

#[inline]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[inline]
pub fn clamp(v: f32, lo: f32, hi: f32) -> f32 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

/// Wrap an angle into `(-PI, PI]` so shortest-path rotation is well defined.
#[inline]
pub fn wrap_angle(mut a: f32) -> f32 {
    while a > PI {
        a -= TAU;
    }
    while a <= -PI {
        a += TAU;
    }
    a
}

/// Shortest signed angular distance from `a` to `b`.
#[inline]
pub fn angle_delta(a: f32, b: f32) -> f32 {
    wrap_angle(b - a)
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const ZERO: Vec2 = Vec2 { x: 0.0, y: 0.0 };
    pub const ONE: Vec2 = Vec2 { x: 1.0, y: 1.0 };

    #[inline]
    pub const fn new(x: f32, y: f32) -> Self {
        Vec2 { x, y }
    }
    #[inline]
    pub fn splat(v: f32) -> Self {
        Vec2 { x: v, y: v }
    }
    #[inline]
    pub fn dot(self, o: Vec2) -> f32 {
        self.x * o.x + self.y * o.y
    }
    /// 2D cross product (the z of the 3D cross). Sign tells you which side.
    #[inline]
    pub fn cross(self, o: Vec2) -> f32 {
        self.x * o.y - self.y * o.x
    }
    #[inline]
    pub fn len_sq(self) -> f32 {
        self.dot(self)
    }
    #[inline]
    pub fn len(self) -> f32 {
        self.len_sq().sqrt()
    }
    #[inline]
    pub fn dist(self, o: Vec2) -> f32 {
        (self - o).len()
    }
    /// Returns `Vec2::ZERO` for a degenerate vector rather than NaN — every
    /// caller in this crate treats "no direction" as "make no change".
    #[inline]
    pub fn normalize(self) -> Vec2 {
        let l = self.len();
        if l <= 1e-9 {
            Vec2::ZERO
        } else {
            Vec2::new(self.x / l, self.y / l)
        }
    }
    /// Rotated 90° counter-clockwise in a y-down space.
    #[inline]
    pub fn perp(self) -> Vec2 {
        Vec2::new(-self.y, self.x)
    }
    #[inline]
    pub fn angle(self) -> f32 {
        self.y.atan2(self.x)
    }
    #[inline]
    pub fn from_angle(a: f32) -> Vec2 {
        Vec2::new(a.cos(), a.sin())
    }
    #[inline]
    pub fn rotate(self, a: f32) -> Vec2 {
        let (s, c) = a.sin_cos();
        Vec2::new(self.x * c - self.y * s, self.x * s + self.y * c)
    }
    #[inline]
    pub fn lerp(self, o: Vec2, t: f32) -> Vec2 {
        Vec2::new(lerp(self.x, o.x, t), lerp(self.y, o.y, t))
    }
    #[inline]
    pub fn min(self, o: Vec2) -> Vec2 {
        Vec2::new(self.x.min(o.x), self.y.min(o.y))
    }
    #[inline]
    pub fn max(self, o: Vec2) -> Vec2 {
        Vec2::new(self.x.max(o.x), self.y.max(o.y))
    }
    #[inline]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }

    /// Closest point on segment `a..b`, and the parameter along it.
    pub fn closest_on_segment(self, a: Vec2, b: Vec2) -> (Vec2, f32) {
        let ab = b - a;
        let l2 = ab.len_sq();
        if l2 <= 1e-12 {
            return (a, 0.0);
        }
        let t = clamp((self - a).dot(ab) / l2, 0.0, 1.0);
        (a + ab * t, t)
    }
}

impl std::ops::Add for Vec2 {
    type Output = Vec2;
    #[inline]
    fn add(self, o: Vec2) -> Vec2 {
        Vec2::new(self.x + o.x, self.y + o.y)
    }
}
impl std::ops::Sub for Vec2 {
    type Output = Vec2;
    #[inline]
    fn sub(self, o: Vec2) -> Vec2 {
        Vec2::new(self.x - o.x, self.y - o.y)
    }
}
impl std::ops::Mul<f32> for Vec2 {
    type Output = Vec2;
    #[inline]
    fn mul(self, s: f32) -> Vec2 {
        Vec2::new(self.x * s, self.y * s)
    }
}
impl std::ops::Div<f32> for Vec2 {
    type Output = Vec2;
    #[inline]
    fn div(self, s: f32) -> Vec2 {
        Vec2::new(self.x / s, self.y / s)
    }
}
impl std::ops::Neg for Vec2 {
    type Output = Vec2;
    #[inline]
    fn neg(self) -> Vec2 {
        Vec2::new(-self.x, -self.y)
    }
}
impl std::ops::AddAssign for Vec2 {
    #[inline]
    fn add_assign(&mut self, o: Vec2) {
        *self = *self + o;
    }
}
impl std::ops::SubAssign for Vec2 {
    #[inline]
    fn sub_assign(&mut self, o: Vec2) {
        *self = *self - o;
    }
}

/// A 2D affine transform, stored as the top two rows of a 3x3 matrix.
///
/// ```text
/// x' = a*x + c*y + tx
/// y' = b*x + d*y + ty
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Default for Affine {
    fn default() -> Self {
        Affine::IDENTITY
    }
}

impl Affine {
    pub const IDENTITY: Affine =
        Affine { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: 0.0 };

    #[inline]
    pub fn translate(t: Vec2) -> Affine {
        Affine { tx: t.x, ty: t.y, ..Affine::IDENTITY }
    }

    #[inline]
    pub fn scale(s: Vec2) -> Affine {
        Affine { a: s.x, d: s.y, ..Affine::IDENTITY }
    }

    #[inline]
    pub fn rotate(r: f32) -> Affine {
        let (s, c) = r.sin_cos();
        Affine { a: c, b: s, c: -s, d: c, tx: 0.0, ty: 0.0 }
    }

    /// Translate ∘ Rotate ∘ Scale — the order every bone transform uses.
    pub fn from_trs(t: Vec2, r: f32, s: Vec2) -> Affine {
        let (sn, cs) = r.sin_cos();
        Affine {
            a: cs * s.x,
            b: sn * s.x,
            c: -sn * s.y,
            d: cs * s.y,
            tx: t.x,
            ty: t.y,
        }
    }

    /// Compose: `self * other`, with `other` applied first.
    ///
    /// Deliberately an inherent `mul` rather than `std::ops::Mul`. Transform
    /// composition is not commutative and reads badly as an operator when the
    /// argument order carries the meaning, and `a.mul(b)` puts "a then b" in
    /// the order they are written.
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn mul(self, o: Affine) -> Affine {
        Affine {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            tx: self.a * o.tx + self.c * o.ty + self.tx,
            ty: self.b * o.tx + self.d * o.ty + self.ty,
        }
    }

    #[inline]
    pub fn apply(self, p: Vec2) -> Vec2 {
        Vec2::new(
            self.a * p.x + self.c * p.y + self.tx,
            self.b * p.x + self.d * p.y + self.ty,
        )
    }

    /// Transform a direction — ignores translation.
    #[inline]
    pub fn apply_vec(self, v: Vec2) -> Vec2 {
        Vec2::new(self.a * v.x + self.c * v.y, self.b * v.x + self.d * v.y)
    }

    #[inline]
    pub fn origin(self) -> Vec2 {
        Vec2::new(self.tx, self.ty)
    }

    #[inline]
    pub fn det(self) -> f32 {
        self.a * self.d - self.b * self.c
    }

    /// Rotation of the x-axis. Meaningful for the uniform/near-uniform scales
    /// bones actually use; a sheared transform has no single angle.
    #[inline]
    pub fn rotation(self) -> f32 {
        self.b.atan2(self.a)
    }

    /// Inverse, or identity for a singular matrix. A bone collapsed to zero
    /// scale must not poison the whole pose with NaNs.
    pub fn inverse(self) -> Affine {
        let det = self.det();
        if det.abs() <= 1e-12 {
            return Affine::IDENTITY;
        }
        let inv = 1.0 / det;
        let a = self.d * inv;
        let b = -self.b * inv;
        let c = -self.c * inv;
        let d = self.a * inv;
        Affine {
            a,
            b,
            c,
            d,
            tx: -(a * self.tx + c * self.ty),
            ty: -(b * self.tx + d * self.ty),
        }
    }

    /// Component-wise blend. Used only for weighted skinning where the bones
    /// involved are near-rigid; not a rotation-correct interpolation.
    #[inline]
    pub fn lerp(self, o: Affine, t: f32) -> Affine {
        Affine {
            a: lerp(self.a, o.a, t),
            b: lerp(self.b, o.b, t),
            c: lerp(self.c, o.c, t),
            d: lerp(self.d, o.d, t),
            tx: lerp(self.tx, o.tx, t),
            ty: lerp(self.ty, o.ty, t),
        }
    }
}

/// An axis-aligned box. `Rect::EMPTY` is the identity for `union_point`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub min: Vec2,
    pub max: Vec2,
}

impl Default for Rect {
    /// The empty rect, which is the identity for `union_point`.
    fn default() -> Self {
        Rect::EMPTY
    }
}

impl Rect {
    pub const EMPTY: Rect = Rect {
        min: Vec2 { x: f32::INFINITY, y: f32::INFINITY },
        max: Vec2 { x: f32::NEG_INFINITY, y: f32::NEG_INFINITY },
    };

    pub fn new(min: Vec2, max: Vec2) -> Rect {
        Rect { min, max }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.min.x > self.max.x || self.min.y > self.max.y
    }

    #[inline]
    pub fn union_point(&mut self, p: Vec2) {
        self.min = self.min.min(p);
        self.max = self.max.max(p);
    }

    #[inline]
    pub fn width(&self) -> f32 {
        (self.max.x - self.min.x).max(0.0)
    }
    #[inline]
    pub fn height(&self) -> f32 {
        (self.max.y - self.min.y).max(0.0)
    }

    #[inline]
    pub fn contains(&self, p: Vec2) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }

    pub fn inflate(&self, by: f32) -> Rect {
        if self.is_empty() {
            return *self;
        }
        Rect {
            min: self.min - Vec2::splat(by),
            max: self.max + Vec2::splat(by),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn affine_compose_matches_sequential_application() {
        let r = Affine::rotate(0.7);
        let t = Affine::translate(Vec2::new(10.0, -3.0));
        let s = Affine::scale(Vec2::new(2.0, 0.5));
        let p = Vec2::new(4.0, 9.0);
        let composed = t.mul(r).mul(s);
        let sequential = t.apply(r.apply(s.apply(p)));
        let direct = composed.apply(p);
        assert!(close(direct.x, sequential.x) && close(direct.y, sequential.y));
    }

    #[test]
    fn from_trs_equals_explicit_composition() {
        let t = Vec2::new(3.0, -7.0);
        let r = -1.2;
        let s = Vec2::new(1.4, 0.6);
        let a = Affine::from_trs(t, r, s);
        let b = Affine::translate(t).mul(Affine::rotate(r)).mul(Affine::scale(s));
        let p = Vec2::new(-2.0, 5.0);
        assert!(close(a.apply(p).x, b.apply(p).x));
        assert!(close(a.apply(p).y, b.apply(p).y));
    }

    #[test]
    fn inverse_round_trips() {
        let m = Affine::from_trs(Vec2::new(11.0, 2.0), 0.9, Vec2::new(1.7, 2.2));
        let p = Vec2::new(6.0, -4.0);
        let q = m.inverse().apply(m.apply(p));
        assert!(close(p.x, q.x) && close(p.y, q.y));
    }

    #[test]
    fn singular_inverse_is_identity_not_nan() {
        let m = Affine::scale(Vec2::ZERO);
        let inv = m.inverse();
        assert_eq!(inv, Affine::IDENTITY);
    }

    #[test]
    fn normalize_of_zero_is_zero_not_nan() {
        let n = Vec2::ZERO.normalize();
        assert!(n.is_finite());
        assert_eq!(n, Vec2::ZERO);
    }

    #[test]
    fn wrap_angle_is_in_range() {
        for i in -20..20 {
            let a = i as f32 * 1.1;
            let w = wrap_angle(a);
            assert!(w > -PI - 1e-5 && w <= PI + 1e-5, "{a} -> {w}");
        }
    }

    #[test]
    fn closest_on_segment_clamps_to_ends() {
        let a = Vec2::new(0.0, 0.0);
        let b = Vec2::new(10.0, 0.0);
        assert_eq!(Vec2::new(-5.0, 3.0).closest_on_segment(a, b).0, a);
        assert_eq!(Vec2::new(50.0, 3.0).closest_on_segment(a, b).0, b);
        let (p, t) = Vec2::new(5.0, 3.0).closest_on_segment(a, b);
        assert!(close(p.x, 5.0) && close(t, 0.5));
    }

    #[test]
    fn rotation_of_pure_rotation_recovers_angle() {
        let m = Affine::from_trs(Vec2::new(1.0, 1.0), 0.6, Vec2::ONE);
        assert!(close(m.rotation(), 0.6));
    }
}
