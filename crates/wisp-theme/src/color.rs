//! Colour, in the space the NX design language is actually written in.
//!
//! DESIGN.md is a CSS document. CSS composites in **sRGB**, and every token in
//! §2 was tuned by eye against a browser doing sRGB blending. So this crate
//! stores colours as 8-bit sRGB with straight alpha and hands the GPU
//! *premultiplied sRGB* floats — blending in that space reproduces the tokens
//! exactly, and `#7700FF` reads back from a render target as `0x77,0x00,0xFF`.
//!
//! Linear-light values are still available ([`Color::linear`]) because WCAG
//! contrast is defined on linear luminance, not on the code values.

/// An 8-bit sRGB colour with straight (non-premultiplied) alpha.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// CSS `rgba(_, _, _, x)` alpha → 8-bit, rounded the way browsers round.
pub const fn af(x: f32) -> u8 {
    let v = x * 255.0 + 0.5;
    if v <= 0.0 {
        0
    } else if v >= 255.0 {
        255
    } else {
        v as u8
    }
}

impl Color {
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };

    /// `Color::hex(0x7700ff)` — opaque, exactly as written in DESIGN.md §2.
    pub const fn hex(rgb: u32) -> Color {
        Color {
            r: ((rgb >> 16) & 0xff) as u8,
            g: ((rgb >> 8) & 0xff) as u8,
            b: (rgb & 0xff) as u8,
            a: 0xff,
        }
    }

    /// `Color::hexa(0x7700ff, 0.34)` — the CSS `rgba()` form.
    pub const fn hexa(rgb: u32, alpha: f32) -> Color {
        Color { a: af(alpha), ..Color::hex(rgb) }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a }
    }

    /// The `0xRRGGBB` this colour round-trips through.
    pub const fn to_hex(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }

    /// `0xAARRGGBB`, the packing `cosmic-text` uses.
    pub const fn to_argb(self) -> u32 {
        ((self.a as u32) << 24) | self.to_hex()
    }

    pub const fn with_alpha(self, alpha: f32) -> Color {
        Color { a: af(alpha), ..self }
    }

    /// Alpha as 0..=1.
    pub fn alpha_f(self) -> f32 {
        self.a as f32 / 255.0
    }

    /// Straight sRGB floats, 0..=1. What CSS calls the colour's channels.
    pub fn srgb(self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.a as f32 / 255.0,
        ]
    }

    /// **Premultiplied sRGB** — what goes into a vertex buffer. The render
    /// targets are `Rgba8Unorm` (not `-Srgb`), so the shader's output value is
    /// taken literally as the 8-bit code, and blending happens in sRGB exactly
    /// as a browser would do it.
    pub fn premul_srgb(self) -> [f32; 4] {
        let [r, g, b, a] = self.srgb();
        [r * a, g * a, b * a, a]
    }

    /// Linear-light floats (sRGB EOTF applied). Used for luminance only.
    pub fn linear(self) -> [f32; 4] {
        let f = |c: f32| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let [r, g, b, a] = self.srgb();
        [f(r), f(g), f(b), a]
    }

    /// WCAG 2.1 relative luminance. Alpha is ignored — composite first.
    pub fn luminance(self) -> f32 {
        let [r, g, b, _] = self.linear();
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// WCAG 2.1 contrast ratio, 1.0..=21.0. Both colours must be opaque;
    /// use [`Color::over`] to flatten translucency first.
    pub fn contrast(self, other: Color) -> f32 {
        let (a, b) = (self.luminance(), other.luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Source-over composite of `self` on top of `under`, in sRGB space —
    /// the same arithmetic the compositor and the browser do.
    pub fn over(self, under: Color) -> Color {
        let sa = self.alpha_f();
        let ua = under.alpha_f();
        let out_a = sa + ua * (1.0 - sa);
        if out_a <= f32::EPSILON {
            return Color::TRANSPARENT;
        }
        let mix = |s: u8, u: u8| {
            let v = (s as f32 * sa + u as f32 * ua * (1.0 - sa)) / out_a;
            (v + 0.5).clamp(0.0, 255.0) as u8
        };
        Color {
            r: mix(self.r, under.r),
            g: mix(self.g, under.g),
            b: mix(self.b, under.b),
            a: (out_a * 255.0 + 0.5).clamp(0.0, 255.0) as u8,
        }
    }

    /// Straight linear interpolation in sRGB space (CSS gradient behaviour).
    pub fn lerp(self, other: Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t + 0.5) as u8;
        Color {
            r: m(self.r, other.r),
            g: m(self.g, other.g),
            b: m(self.b, other.b),
            a: m(self.a, other.a),
        }
    }

    pub const fn is_opaque(self) -> bool {
        self.a == 0xff
    }
}

impl core::fmt::Debug for Color {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.is_opaque() {
            write!(f, "#{:06x}", self.to_hex())
        } else {
            write!(f, "#{:06x}@{:.2}", self.to_hex(), self.a as f32 / 255.0)
        }
    }
}

/// 24-bit SGR foreground escape, DESIGN.md §10's terminal mapping. Not a
/// fallback — the CLI half of the language uses exactly these.
pub fn sgr_fg(c: Color) -> String {
    format!("\x1b[38;2;{};{};{}m", c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette;

    #[test]
    fn palette_round_trips_through_hex() {
        for (name, c) in palette::ALL {
            assert!(c.is_opaque(), "{name} is a brand anchor and must be opaque");
            assert_eq!(Color::hex(c.to_hex()), **c, "{name} did not round-trip");
        }
    }

    #[test]
    fn the_anchors_are_the_documented_bytes() {
        assert_eq!(palette::VIOLET, Color::rgba(0x77, 0x00, 0xff, 0xff));
        assert_eq!(palette::CYAN, Color::rgba(0x00, 0xe5, 0xff, 0xff));
        assert_eq!(palette::AMBER, Color::rgba(0xff, 0xb3, 0x00, 0xff));
        assert_eq!(palette::DANGER, Color::rgba(0xff, 0x54, 0x70, 0xff));
        assert_eq!(palette::TEXT, Color::rgba(0xef, 0xea, 0xff, 0xff));
        assert_eq!(palette::MUTED, Color::rgba(0x9a, 0x8f, 0xc0, 0xff));
        assert_eq!(palette::BG_TOP, Color::rgba(0x0a, 0x07, 0x14, 0xff));
        assert_eq!(palette::BG_BOTTOM, Color::rgba(0x12, 0x09, 0x1f, 0xff));
    }

    #[test]
    fn css_alpha_rounds_like_a_browser() {
        assert_eq!(af(0.0), 0);
        assert_eq!(af(1.0), 255);
        assert_eq!(af(0.09), 23); // 22.95
        assert_eq!(af(0.5), 128); // 127.5
        assert_eq!(af(0.34), 87);
    }

    #[test]
    fn premultiplication_is_exact_at_the_ends() {
        assert_eq!(palette::VIOLET.premul_srgb()[3], 1.0);
        let half = palette::VIOLET.with_alpha(0.5);
        let p = half.premul_srgb();
        assert!((p[0] - (0x77 as f32 / 255.0) * p[3]).abs() < 1e-6);
        assert!(p[1].abs() < 1e-6);
    }

    #[test]
    fn compositing_a_transparent_colour_is_a_no_op() {
        let under = palette::PANEL;
        assert_eq!(Color::TRANSPARENT.over(under), under);
        assert_eq!(palette::VIOLET.over(under), palette::VIOLET);
    }

    #[test]
    fn contrast_is_symmetric_and_bounded() {
        let white = Color::hex(0xffffff);
        let black = Color::hex(0x000000);
        assert!((white.contrast(black) - 21.0).abs() < 0.01);
        assert!((black.contrast(white) - 21.0).abs() < 0.01);
        assert!((white.contrast(white) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn sgr_matches_the_documented_escapes() {
        assert_eq!(sgr_fg(palette::VIOLET), "\x1b[38;2;119;0;255m");
        assert_eq!(sgr_fg(palette::CYAN), "\x1b[38;2;0;229;255m");
        assert_eq!(sgr_fg(palette::AMBER), "\x1b[38;2;255;179;0m");
        assert_eq!(sgr_fg(palette::MUTED), "\x1b[38;2;154;143;192m");
        assert_eq!(sgr_fg(palette::TEXT), "\x1b[38;2;239;234;255m");
        assert_eq!(sgr_fg(palette::DANGER), "\x1b[38;2;255;84;112m");
    }
}
