//! Paints, and the GPU record they compile to.
//!
//! A [`Paint`] is the runtime, owned twin of a `wisp_theme::Gradient` (whose
//! stops are `&'static`, because a token is a constant). Everything animated —
//! a hover ramp, a progress fill, a sheen bound to the pointer — is a `Paint`.

use bytemuck::{Pod, Zeroable};
use wisp_theme::tokens::MAX_STOPS;
use wisp_theme::{Color, Gradient, GradientKind};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stop {
    pub at: f32,
    pub color: Color,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Paint {
    Solid(Color),
    /// CSS angle: 0 = to top, 90 = to right, 180 = to bottom.
    Linear { angle_deg: f32, stops: Vec<Stop> },
    /// Centre and radii in 0..1 of the shape's bounding box.
    Radial { cx: f32, cy: f32, rx: f32, ry: f32, stops: Vec<Stop> },
}

impl Paint {
    pub fn solid(c: Color) -> Paint {
        Paint::Solid(c)
    }

    /// Lift a frozen design token into a paint.
    pub fn token(g: Gradient) -> Paint {
        let stops: Vec<Stop> = g.stops.iter().map(|s| Stop { at: s.at, color: s.color }).collect();
        match g.kind {
            GradientKind::Linear { angle_deg } => {
                if stops.len() == 1 {
                    Paint::Solid(stops[0].color)
                } else {
                    Paint::Linear { angle_deg, stops }
                }
            }
            GradientKind::Radial { cx, cy, rx, ry } => Paint::Radial { cx, cy, rx, ry, stops },
        }
    }

    fn stops_mut(&mut self) -> Option<&mut Vec<Stop>> {
        match self {
            Paint::Solid(_) => None,
            Paint::Linear { stops, .. } | Paint::Radial { stops, .. } => Some(stops),
        }
    }

    /// Multiply every stop's alpha. How a disabled control gets its 40%, and
    /// how a fading toast fades.
    pub fn with_opacity(mut self, o: f32) -> Paint {
        let o = o.clamp(0.0, 1.0);
        let scale = |c: Color| c.with_alpha(c.alpha_f() * o);
        match &mut self {
            Paint::Solid(c) => *c = scale(*c),
            _ => {
                if let Some(stops) = self.stops_mut() {
                    for s in stops.iter_mut() {
                        s.color = scale(s.color);
                    }
                }
            }
        }
        self
    }

    /// Slide a gradient's stops along by `d`, clamped into 0..1. This is how
    /// the pointer-bound sheen moves: the *paint* is repositioned by a driver
    /// value, never re-timed by an animation (DESIGN.md §1).
    pub fn shifted(mut self, d: f32) -> Paint {
        if let Some(stops) = self.stops_mut() {
            for s in stops.iter_mut() {
                s.at = (s.at + d).clamp(0.0, 1.0);
            }
        }
        self
    }

    /// CPU evaluation at `t`, used by the tests and by the atlas baker's
    /// reference path.
    pub fn sample(&self, t: f32) -> Color {
        let stops = match self {
            Paint::Solid(c) => return *c,
            Paint::Linear { stops, .. } | Paint::Radial { stops, .. } => stops,
        };
        let t = t.clamp(0.0, 1.0);
        match stops.as_slice() {
            [] => Color::TRANSPARENT,
            [only] => only.color,
            s => {
                if t <= s[0].at {
                    return s[0].color;
                }
                let last = s[s.len() - 1];
                if t >= last.at {
                    return last.color;
                }
                for w in s.windows(2) {
                    if t >= w[0].at && t <= w[1].at {
                        let span = w[1].at - w[0].at;
                        let f = if span <= f32::EPSILON { 0.0 } else { (t - w[0].at) / span };
                        return w[0].color.lerp(w[1].color, f);
                    }
                }
                last.color
            }
        }
    }

    /// Is this paint guaranteed to write nothing?
    pub fn is_invisible(&self) -> bool {
        match self {
            Paint::Solid(c) => c.a == 0,
            Paint::Linear { stops, .. } | Paint::Radial { stops, .. } => {
                stops.is_empty() || stops.iter().all(|s| s.color.a == 0)
            }
        }
    }
}

impl From<Color> for Paint {
    fn from(c: Color) -> Paint {
        Paint::Solid(c)
    }
}

impl From<Gradient> for Paint {
    fn from(g: Gradient) -> Paint {
        Paint::token(g)
    }
}

pub const PAINT_SOLID: u32 = 0;
pub const PAINT_LINEAR: u32 = 1;
pub const PAINT_RADIAL: u32 = 2;

/// One paint, as the fragment shader sees it. 160 bytes, `std430`-compatible:
/// every member is 16-byte aligned so there is nothing for a driver to pad
/// differently than we expect.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct PaintGpu {
    pub kind: u32,
    pub n_stops: u32,
    pub _pad: [u32; 2],
    /// Linear: `(dx, dy, bbox_w, bbox_h)`. Radial: `(cx, cy, rx, ry)`.
    pub params: [f32; 4],
    /// Stop offsets, six of them packed into two vec4s.
    pub offsets: [[f32; 4]; 2],
    /// Premultiplied sRGB, one per stop.
    pub colors: [[f32; 4]; MAX_STOPS],
}

impl PaintGpu {
    pub fn transparent() -> PaintGpu {
        PaintGpu {
            kind: PAINT_SOLID,
            n_stops: 1,
            _pad: [0; 2],
            params: [0.0; 4],
            offsets: [[0.0; 4]; 2],
            colors: [[0.0; 4]; MAX_STOPS],
        }
    }

    /// Compile a paint for a shape with the given bounding box.
    pub fn encode(paint: &Paint, bbox: crate::geom::Rect) -> PaintGpu {
        let mut out = PaintGpu::transparent();
        let stops: Vec<Stop> = match paint {
            Paint::Solid(c) => {
                out.kind = PAINT_SOLID;
                vec![Stop { at: 0.0, color: *c }]
            }
            Paint::Linear { angle_deg, stops } => {
                out.kind = PAINT_LINEAR;
                let r = angle_deg.to_radians();
                out.params = [r.sin(), -r.cos(), bbox.w.max(1e-3), bbox.h.max(1e-3)];
                stops.clone()
            }
            Paint::Radial { cx, cy, rx, ry, stops } => {
                out.kind = PAINT_RADIAL;
                out.params = [*cx, *cy, rx.max(1e-4), ry.max(1e-4)];
                stops.clone()
            }
        };
        let n = stops.len().min(MAX_STOPS);
        out.n_stops = n as u32;
        for (i, s) in stops.iter().take(n).enumerate() {
            out.offsets[i / 4][i % 4] = s.at;
            out.colors[i] = s.color.premul_srgb();
        }
        // Clamp the tail so a shader read past n_stops still yields the last
        // colour rather than transparent black.
        for i in n..MAX_STOPS {
            out.offsets[i / 4][i % 4] = 1.0;
            out.colors[i] = out.colors[n.saturating_sub(1)];
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;
    use wisp_theme::{palette, tokens};

    #[test]
    fn a_token_round_trips_into_a_paint() {
        let p = Paint::token(tokens::SURFACE_1);
        assert_eq!(p.sample(0.0), palette::SURFACE_1_TOP);
        assert_eq!(p.sample(1.0), palette::SURFACE_1_BOTTOM);
        assert_eq!(p.sample(0.44), palette::SURFACE_1_MID);
    }

    #[test]
    fn a_single_stop_token_collapses_to_a_solid() {
        let g = wisp_theme::Surface::Recessed(wisp_theme::Recessed::Input).edge().gradient().unwrap();
        assert!(matches!(Paint::token(g), Paint::Solid(_)));
    }

    #[test]
    fn opacity_multiplies_every_stop() {
        let p = Paint::token(tokens::SURFACE_1).with_opacity(0.5);
        assert_eq!(p.sample(0.0).a, 128);
        assert_eq!(Paint::solid(palette::VIOLET).with_opacity(0.0).sample(0.0).a, 0);
        assert!(Paint::solid(palette::VIOLET).with_opacity(0.0).is_invisible());
    }

    #[test]
    fn shifting_a_sheen_moves_its_stops_and_nothing_else() {
        let base = Paint::token(tokens::SHEEN);
        let moved = base.clone().shifted(0.2);
        match (&base, &moved) {
            (Paint::Linear { stops: a, .. }, Paint::Linear { stops: b, .. }) => {
                assert_eq!(a.len(), b.len());
                assert!((b[0].at - (a[0].at + 0.2)).abs() < 1e-5);
                assert_eq!(a[0].color, b[0].color);
            }
            _ => panic!("the sheen must stay a linear gradient"),
        }
    }

    #[test]
    fn encoding_a_solid_produces_one_premultiplied_stop() {
        let g = PaintGpu::encode(&Paint::solid(palette::VIOLET), Rect::from_size(10.0, 10.0));
        assert_eq!(g.kind, PAINT_SOLID);
        assert_eq!(g.n_stops, 1);
        assert_eq!(g.colors[0], palette::VIOLET.premul_srgb());
    }

    #[test]
    fn encoding_a_linear_gradient_carries_the_direction_and_the_box() {
        let g = PaintGpu::encode(&Paint::token(tokens::FIELD), Rect::new(0.0, 0.0, 40.0, 80.0));
        assert_eq!(g.kind, PAINT_LINEAR);
        assert_eq!(g.n_stops, 2);
        assert!(g.params[0].abs() < 1e-5, "180deg has no horizontal component");
        assert!((g.params[1] - 1.0).abs() < 1e-5, "180deg points down");
        assert_eq!(g.params[2], 40.0);
        assert_eq!(g.params[3], 80.0);
    }

    #[test]
    fn the_five_stop_hairline_fits_the_record() {
        let g = PaintGpu::encode(&Paint::token(tokens::HAIRLINE), Rect::from_size(100.0, 1.0));
        assert_eq!(g.n_stops, 5);
        assert_eq!(g.offsets[1][0], 1.0, "the fifth stop lands in the second vec4");
        // The unused sixth slot repeats the last colour, never transparent.
        assert_eq!(g.colors[5], g.colors[4]);
    }

    #[test]
    fn the_record_is_the_size_the_shader_expects() {
        assert_eq!(std::mem::size_of::<PaintGpu>(), 160);
        assert_eq!(std::mem::align_of::<PaintGpu>(), 4);
    }

    #[test]
    fn a_degenerate_bbox_does_not_produce_a_zero_divisor() {
        let g = PaintGpu::encode(&Paint::token(tokens::FIELD), Rect::ZERO);
        assert!(g.params[2] > 0.0 && g.params[3] > 0.0);
    }
}
