//! Gradients, and the rule that gives the whole language its physicality:
//! **one light source, upper-left, in every gradient, bevel and edge.**
//!
//! Angles use the CSS convention (`0deg` points to the top, `90deg` to the
//! right) so a token can be read straight out of DESIGN.md §2 and typed in
//! here without translation. [`Gradient::direction`] converts to screen space
//! (x right, y **down**), which is what the renderer wants.

use crate::color::Color;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stop {
    /// Position along the gradient, 0..=1.
    pub at: f32,
    pub color: Color,
}

impl Stop {
    pub const fn new(at: f32, color: Color) -> Stop {
        Stop { at, color }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GradientKind {
    /// CSS angle: 0 = to top, 90 = to right, 180 = to bottom.
    Linear { angle_deg: f32 },
    /// Centre and radii in normalised box coordinates (0..1 of the shape's
    /// bounding box), which is how the nebula blobs of §3 are authored.
    Radial { cx: f32, cy: f32, rx: f32, ry: f32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gradient {
    pub kind: GradientKind,
    /// At most [`crate::tokens::MAX_STOPS`] stops. Every token in DESIGN.md §2
    /// fits; the renderer's paint record is sized to match, so this is a hard
    /// limit by design.
    pub stops: &'static [Stop],
}

/// Which way a surface faces the light. A raised surface catches the light on
/// its upper-left; a recessed one is *shadowed* there by its own lip. Both are
/// "light from the upper-left" — they just have opposite luminance ramps, and
/// conflating them is the classic way a fake-glass UI stops reading as
/// physical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightModel {
    Raised,
    Recessed,
    /// Not a face at all — the deep-space field, and the nebula in it. It has
    /// a direction but no elevation, so only the direction rule binds it.
    Ambient,
}

impl Gradient {
    pub const fn linear(angle_deg: f32, stops: &'static [Stop]) -> Gradient {
        Gradient { kind: GradientKind::Linear { angle_deg }, stops }
    }

    pub const fn radial(cx: f32, cy: f32, rx: f32, ry: f32, stops: &'static [Stop]) -> Gradient {
        Gradient { kind: GradientKind::Radial { cx, cy, rx, ry }, stops }
    }

    /// A one-colour "gradient", so solid fills and gradient fills are the same
    /// kind of thing to the renderer.
    pub const fn solid(stops: &'static [Stop]) -> Gradient {
        Gradient::linear(180.0, stops)
    }

    /// Unit direction in **screen space** (x right, y down) that the gradient
    /// travels. `180deg` (to bottom) is `(0, 1)`.
    pub fn direction(&self) -> (f32, f32) {
        match self.kind {
            GradientKind::Linear { angle_deg } => {
                let r = angle_deg.to_radians();
                (r.sin(), -r.cos())
            }
            GradientKind::Radial { .. } => (0.0, 0.0),
        }
    }

    /// Sample the gradient at `t` in 0..=1, interpolating in sRGB the way CSS
    /// does. Used by the atlas baker's CPU path and by the tests.
    pub fn sample(&self, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        match self.stops {
            [] => Color::TRANSPARENT,
            [only] => only.color,
            stops => {
                if t <= stops[0].at {
                    return stops[0].color;
                }
                let last = stops[stops.len() - 1];
                if t >= last.at {
                    return last.color;
                }
                for w in stops.windows(2) {
                    let (a, b) = (w[0], w[1]);
                    if t >= a.at && t <= b.at {
                        let span = b.at - a.at;
                        let f = if span <= f32::EPSILON { 0.0 } else { (t - a.at) / span };
                        return a.color.lerp(b.color, f);
                    }
                }
                last.color
            }
        }
    }

    pub fn first(&self) -> Color {
        self.stops.first().map(|s| s.color).unwrap_or(Color::TRANSPARENT)
    }

    pub fn last(&self) -> Color {
        self.stops.last().map(|s| s.color).unwrap_or(Color::TRANSPARENT)
    }

    /// The smallest alpha any stop carries. DESIGN.md v1.5 requires this to be
    /// ≥ 0.85 for the *body* fill of anything that floats.
    pub fn min_alpha(&self) -> f32 {
        self.stops.iter().map(|s| s.color.alpha_f()).fold(1.0, f32::min)
    }

    pub fn is_opaque(&self) -> bool {
        self.stops.iter().all(|s| s.color.is_opaque())
    }

    /// Does this gradient obey the one-light-source rule for the given face?
    ///
    /// Two conditions, both necessary:
    /// 1. it travels down and/or to the right, so "start" is the upper-left;
    /// 2. a raised face is brightest at the start, a recessed face darkest.
    pub fn obeys_light(&self, model: LightModel) -> bool {
        let (dx, dy) = match self.kind {
            GradientKind::Linear { .. } => self.direction(),
            // A radial highlight obeys the rule by sitting in the upper-left.
            GradientKind::Radial { cx, cy, .. } => return cx <= 0.5 && cy <= 0.5,
        };
        if dx < -1e-4 || dy < -1e-4 || (dx.abs() < 1e-4 && dy.abs() < 1e-4) {
            return false;
        }
        // Compare *composited over the panel* so alpha ramps count too: a
        // "highlight" that is merely less transparent at the bottom is darker,
        // not brighter.
        let base = crate::palette::PANEL;
        let a = self.first().over(base).luminance();
        let b = self.last().over(base).luminance();
        match model {
            LightModel::Raised => a >= b,
            LightModel::Recessed => a <= b,
            LightModel::Ambient => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{palette, tokens};

    #[test]
    fn css_angles_map_to_screen_space() {
        let to_bottom = Gradient::linear(180.0, &[]);
        let (dx, dy) = to_bottom.direction();
        assert!(dx.abs() < 1e-5 && (dy - 1.0).abs() < 1e-5);

        let to_right = Gradient::linear(90.0, &[]);
        let (dx, dy) = to_right.direction();
        assert!((dx - 1.0).abs() < 1e-5 && dy.abs() < 1e-5);

        // 157deg — the surface/glass angle — is down and slightly right.
        let (dx, dy) = tokens::SURFACE_1.direction();
        assert!(dx > 0.0 && dy > 0.0, "surface gradient must run toward bottom-right");
    }

    #[test]
    fn the_field_has_direction_but_no_elevation() {
        for (name, g) in tokens::AMBIENT {
            assert!(g.obeys_light(LightModel::Ambient), "{name}");
            let (dx, dy) = g.direction();
            assert!(dx >= -1e-4 && dy >= -1e-4, "{name} travels toward the light");
        }
    }

    #[test]
    fn every_raised_token_is_lit_from_the_upper_left() {
        for (name, g) in tokens::RAISED {
            assert!(g.obeys_light(LightModel::Raised), "{name} is lit from the wrong side");
        }
    }

    #[test]
    fn every_recessed_token_is_shadowed_at_its_lip() {
        for (name, g) in tokens::RECESSED {
            assert!(g.obeys_light(LightModel::Recessed), "{name} is a well and must darken upward");
        }
    }

    #[test]
    fn no_token_carries_more_than_the_paint_record_holds() {
        for (name, g) in tokens::RAISED.iter().chain(tokens::RECESSED).chain(tokens::AMBIENT) {
            assert!(
                g.stops.len() <= tokens::MAX_STOPS,
                "{name} has {} stops; the paint record holds {}",
                g.stops.len(),
                tokens::MAX_STOPS
            );
            assert!(!g.stops.is_empty(), "{name} is empty");
        }
    }

    #[test]
    fn stops_are_sorted_and_in_range() {
        for (name, g) in tokens::RAISED.iter().chain(tokens::RECESSED).chain(tokens::AMBIENT) {
            let mut prev = -1.0f32;
            for s in g.stops {
                assert!((0.0..=1.0).contains(&s.at), "{name} stop out of range");
                assert!(s.at >= prev, "{name} stops are not sorted");
                prev = s.at;
            }
        }
    }

    #[test]
    fn sampling_hits_the_endpoints_exactly() {
        let g = tokens::SURFACE_1;
        assert_eq!(g.sample(0.0), palette::SURFACE_1_TOP);
        assert_eq!(g.sample(1.0), palette::SURFACE_1_BOTTOM);
        assert_eq!(g.sample(-5.0), palette::SURFACE_1_TOP);
        assert_eq!(g.sample(5.0), palette::SURFACE_1_BOTTOM);
    }

    #[test]
    fn sampling_the_middle_actually_varies() {
        let g = tokens::SURFACE_1;
        let mid = g.sample(0.5);
        assert_ne!(mid, g.first());
        assert_ne!(mid, g.last());
    }
}
