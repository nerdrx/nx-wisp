//! Colour and paint description — this crate's *output* types.
//!
//! `wisp-rig` deliberately does not depend on `wisp-paint`: the rig computes
//! geometry and describes how it should be filled, and the renderer maps these
//! small plain structs onto vello brushes. That keeps the whole crate pure and
//! testable with no GPU (SPEC §4), and it means neither crate blocks the other.

use crate::math::Vec2;

/// Straight (non-premultiplied) linear-ish sRGB with an alpha channel, all in
/// `0.0..=1.0`. Values are the sRGB byte values divided by 255 — the renderer
/// decides on colour management, not the rig.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
    pub const WHITE: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };

    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Rgba { r, g, b, a }
    }

    pub const fn rgb8(r: u8, g: u8, b: u8) -> Self {
        Rgba {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        }
    }

    pub fn with_alpha(self, a: f32) -> Self {
        Rgba { a: crate::math::clamp(a, 0.0, 1.0), ..self }
    }

    /// Multiply the alpha channel — how shape opacity and layer weight are
    /// applied without touching the authored colour.
    pub fn scale_alpha(self, k: f32) -> Self {
        Rgba { a: crate::math::clamp(self.a * k, 0.0, 1.0), ..self }
    }

    pub fn lerp(self, o: Rgba, t: f32) -> Rgba {
        use crate::math::lerp;
        Rgba {
            r: lerp(self.r, o.r, t),
            g: lerp(self.g, o.g, t),
            b: lerp(self.b, o.b, t),
            a: lerp(self.a, o.a, t),
        }
    }

    pub fn to_rgba8(self) -> [u8; 4] {
        let q = |v: f32| (crate::math::clamp(v, 0.0, 1.0) * 255.0).round() as u8;
        [q(self.r), q(self.g), q(self.b), q(self.a)]
    }

    /// `#rgb`, `#rrggbb`, `#rrggbbaa`, with or without the leading `#`.
    pub fn parse_hex(s: &str) -> Result<Rgba, ColorError> {
        let h = s.strip_prefix('#').unwrap_or(s);
        if !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ColorError(s.to_string()));
        }
        let nib = |c: u8| (c as char).to_digit(16).unwrap() as f32 / 15.0;
        let byte = |a: u8, b: u8| {
            ((a as char).to_digit(16).unwrap() * 16 + (b as char).to_digit(16).unwrap()) as f32
                / 255.0
        };
        let h = h.as_bytes();
        Ok(match h.len() {
            3 => Rgba::new(nib(h[0]), nib(h[1]), nib(h[2]), 1.0),
            4 => Rgba::new(nib(h[0]), nib(h[1]), nib(h[2]), nib(h[3])),
            6 => Rgba::new(byte(h[0], h[1]), byte(h[2], h[3]), byte(h[4], h[5]), 1.0),
            8 => Rgba::new(
                byte(h[0], h[1]),
                byte(h[2], h[3]),
                byte(h[4], h[5]),
                byte(h[6], h[7]),
            ),
            _ => return Err(ColorError(s.to_string())),
        })
    }

    pub fn to_hex(self) -> String {
        let [r, g, b, a] = self.to_rgba8();
        if a == 255 {
            format!("#{r:02x}{g:02x}{b:02x}")
        } else {
            format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("'{0}' is not a colour — expected #rgb, #rrggbb or #rrggbbaa")]
pub struct ColorError(pub String);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradientStop {
    /// Position along the gradient, `0.0..=1.0`.
    pub at: f32,
    pub color: Rgba,
}

/// How a gradient behaves outside `0..=1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Extend {
    #[default]
    Pad,
    Repeat,
    Reflect,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinearGradient {
    pub start: Vec2,
    pub end: Vec2,
    pub extend: Extend,
    pub stops: Vec<GradientStop>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RadialGradient {
    /// Where the light appears to come *from*. Distinct from `center` this is
    /// a two-point conical gradient, which is what gives glass its off-axis
    /// highlight instead of a flat bullseye.
    pub focus: Vec2,
    pub center: Vec2,
    pub radius: f32,
    pub extend: Extend,
    pub stops: Vec<GradientStop>,
}

/// A resolved paint, ready for the renderer. Geometry here is already in the
/// frame's coordinate space — gradients that follow a bone have had their
/// centres moved before you see them.
#[derive(Debug, Clone, PartialEq)]
pub enum Paint {
    Solid(Rgba),
    Linear(LinearGradient),
    Radial(RadialGradient),
}

impl Paint {
    /// Multiply every colour's alpha. Used for shape opacity and for fading a
    /// shape out with an expression.
    pub fn scale_alpha(&mut self, k: f32) {
        if k >= 1.0 {
            return;
        }
        match self {
            Paint::Solid(c) => *c = c.scale_alpha(k),
            Paint::Linear(g) => {
                for s in &mut g.stops {
                    s.color = s.color.scale_alpha(k);
                }
            }
            Paint::Radial(g) => {
                for s in &mut g.stops {
                    s.color = s.color.scale_alpha(k);
                }
            }
        }
    }

    /// The colour at a normalised position, for tests and for the sprite-atlas
    /// baker's average-colour heuristics. Not a rendering path.
    pub fn sample(&self, t: f32) -> Rgba {
        let stops = match self {
            Paint::Solid(c) => return *c,
            Paint::Linear(g) => &g.stops,
            Paint::Radial(g) => &g.stops,
        };
        sample_stops(stops, t)
    }
}

pub fn sample_stops(stops: &[GradientStop], t: f32) -> Rgba {
    if stops.is_empty() {
        return Rgba::TRANSPARENT;
    }
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
            let k = if span <= 1e-6 { 0.0 } else { (t - a.at) / span };
            return a.color.lerp(b.color, k);
        }
    }
    last.color
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    pub paint: Paint,
    /// In canvas units — scaled with the rig, so a 1px lit edge stays 1px
    /// relative to her at any size (F75).
    pub width: f32,
    pub cap: Cap,
    pub join: Join,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Cap {
    #[default]
    Butt,
    Round,
    Square,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Join {
    #[default]
    Miter,
    Round,
    Bevel,
}

/// Fill rule for a path with self-intersections or holes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FillRule {
    #[default]
    NonZero,
    EvenOdd,
}

/// NX design language anchors, so the default skin and any test can name them
/// instead of repeating hex (DESIGN.md §1).
pub mod nx {
    use super::Rgba;
    pub const VIOLET: Rgba = Rgba::rgb8(0x77, 0x00, 0xff);
    pub const VIOLET_SOFT: Rgba = Rgba::rgb8(0x9a, 0x3c, 0xff);
    pub const CYAN: Rgba = Rgba::rgb8(0x00, 0xe5, 0xff);
    pub const AMBER: Rgba = Rgba::rgb8(0xff, 0xb3, 0x00);
    pub const DANGER: Rgba = Rgba::rgb8(0xff, 0x54, 0x70);
    pub const TEXT: Rgba = Rgba::rgb8(0xef, 0xea, 0xff);
    pub const MUTED: Rgba = Rgba::rgb8(0x9a, 0x8f, 0xc0);
    pub const PANEL: Rgba = Rgba::rgb8(0x17, 0x10, 0x28);
    pub const PANEL_2: Rgba = Rgba::rgb8(0x1d, 0x14, 0x33);
    pub const BG_TOP: Rgba = Rgba::rgb8(0x0a, 0x07, 0x14);
    pub const BG_BOTTOM: Rgba = Rgba::rgb8(0x12, 0x09, 0x1f);
    pub const LINE: Rgba = Rgba::rgb8(0x2a, 0x1f, 0x45);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_hex_length() {
        assert_eq!(Rgba::parse_hex("#fff").unwrap(), Rgba::WHITE);
        assert_eq!(Rgba::parse_hex("ffffff").unwrap(), Rgba::WHITE);
        let v = Rgba::parse_hex("#7700FF").unwrap();
        assert_eq!(v, nx::VIOLET);
        let a = Rgba::parse_hex("#7700ff80").unwrap();
        assert!((a.a - 128.0 / 255.0).abs() < 1e-5);
        assert_eq!(Rgba::parse_hex("#f00f").unwrap().a, 1.0);
    }

    #[test]
    fn rejects_nonsense_colours() {
        assert!(Rgba::parse_hex("#gg0000").is_err());
        assert!(Rgba::parse_hex("#12345").is_err());
        assert!(Rgba::parse_hex("violet").is_err());
    }

    #[test]
    fn hex_round_trips() {
        for s in ["#7700ff", "#00e5ff", "#ffb300", "#12091f80"] {
            assert_eq!(Rgba::parse_hex(s).unwrap().to_hex(), s);
        }
    }

    #[test]
    fn brand_anchors_match_design_md() {
        // If these drift, the skin's violet is no longer NX violet.
        assert_eq!(nx::VIOLET.to_hex(), "#7700ff");
        assert_eq!(nx::CYAN.to_hex(), "#00e5ff");
        assert_eq!(nx::TEXT.to_hex(), "#efeaff");
        assert_eq!(nx::AMBER.to_hex(), "#ffb300");
    }

    #[test]
    fn gradient_sampling_interpolates_and_clamps() {
        let stops = vec![
            GradientStop { at: 0.0, color: nx::CYAN },
            GradientStop { at: 1.0, color: nx::VIOLET },
        ];
        assert_eq!(sample_stops(&stops, -1.0), nx::CYAN);
        assert_eq!(sample_stops(&stops, 2.0), nx::VIOLET);
        let mid = sample_stops(&stops, 0.5);
        assert!((mid.r - (nx::CYAN.r + nx::VIOLET.r) / 2.0).abs() < 1e-5);
    }

    #[test]
    fn coincident_stops_do_not_divide_by_zero() {
        let stops = vec![
            GradientStop { at: 0.5, color: nx::CYAN },
            GradientStop { at: 0.5, color: nx::VIOLET },
        ];
        let c = sample_stops(&stops, 0.5);
        assert!(c.r.is_finite() && c.g.is_finite() && c.b.is_finite());
    }

    #[test]
    fn scale_alpha_touches_every_stop() {
        let mut p = Paint::Linear(LinearGradient {
            start: Vec2::ZERO,
            end: Vec2::new(1.0, 0.0),
            extend: Extend::Pad,
            stops: vec![
                GradientStop { at: 0.0, color: nx::CYAN },
                GradientStop { at: 1.0, color: nx::VIOLET },
            ],
        });
        p.scale_alpha(0.5);
        if let Paint::Linear(g) = &p {
            assert!(g.stops.iter().all(|s| (s.color.a - 0.5).abs() < 1e-5));
        } else {
            unreachable!()
        }
    }

    #[test]
    fn empty_stop_list_samples_transparent() {
        assert_eq!(sample_stops(&[], 0.5), Rgba::TRANSPARENT);
    }
}
