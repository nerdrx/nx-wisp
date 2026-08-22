//! **Angular, never rounded.** Radii live in the 3–6px band; pills are banned;
//! perfect circles are reserved for status dots and spinners.
//!
//! That rule is enforced by the type system, not by review: [`Radius`] has a
//! private field and no `From<u8>`, so the only radii that exist are the four
//! tokens and whatever [`Radius::new`] lets through — and it lets nothing
//! through outside 3..=6. A circle can only be built by naming one of the two
//! sanctioned uses.

use crate::ThemeViolation;

/// A corner radius, guaranteed to be in the 3–6px band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Radius(u8);

impl Radius {
    /// Smallest radius the band allows. Anything sharper is a square corner.
    pub const MIN_PX: u8 = 3;
    /// Largest radius the band allows. Anything softer reads as a toy.
    pub const MAX_PX: u8 = 6;

    /// `--radius-xs` — chips, code.
    pub const XS: Radius = Radius(3);
    /// `--radius-sm` — rows, wells, inputs.
    pub const SM: Radius = Radius(4);
    /// `--pill` — the legacy token name. It is a **5px chamfer**, cut sharp;
    /// it has never been a pill and the name is kept only so a DESIGN.md
    /// reader can find it.
    pub const CUT: Radius = Radius(5);
    /// `--radius` — cards, sheets.
    pub const CARD: Radius = Radius(6);

    /// The only fallible constructor. Refuses anything outside the band, which
    /// is how "no pills" survives contact with a later contributor.
    pub const fn new(px: u8) -> Result<Radius, ThemeViolation> {
        if px < Radius::MIN_PX {
            Err(ThemeViolation::RadiusTooSharp(px))
        } else if px > Radius::MAX_PX {
            Err(ThemeViolation::RadiusTooRound(px))
        } else {
            Ok(Radius(px))
        }
    }

    pub const fn px(self) -> f32 {
        self.0 as f32
    }

    pub const fn px_u8(self) -> u8 {
        self.0
    }

    /// Clamp into the band. For layout code that derives a radius from a size
    /// and must not fail — a tiny chip still gets a 3px cut, never a circle.
    pub const fn clamped(px: u8) -> Radius {
        if px < Radius::MIN_PX {
            Radius(Radius::MIN_PX)
        } else if px > Radius::MAX_PX {
            Radius(Radius::MAX_PX)
        } else {
            Radius(px)
        }
    }

    /// The four tokens, for enumeration and for the "no pills" test.
    pub const ALL: [Radius; 4] = [Radius::XS, Radius::SM, Radius::CUT, Radius::CARD];
}

/// The two — and only two — things allowed to be a perfect circle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircleUse {
    /// A status dot: cyan = live, amber = pending, muted = inert.
    StatusDot,
    /// A spinner, for indeterminate progress.
    Spinner,
}

/// A perfect circle. Constructible only by naming a sanctioned use, so a
/// rounded button cannot become one by accident.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Circle {
    pub diameter_px: f32,
    pub purpose: CircleUse,
}

impl Circle {
    pub const fn status_dot(diameter_px: f32) -> Circle {
        Circle { diameter_px, purpose: CircleUse::StatusDot }
    }
    pub const fn spinner(diameter_px: f32) -> Circle {
        Circle { diameter_px, purpose: CircleUse::Spinner }
    }
    pub const fn radius_px(self) -> f32 {
        self.diameter_px * 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_band_is_three_to_six() {
        assert!(Radius::new(2).is_err());
        assert!(Radius::new(3).is_ok());
        assert!(Radius::new(6).is_ok());
        assert!(Radius::new(7).is_err());
        assert!(Radius::new(255).is_err());
    }

    #[test]
    fn pills_cannot_be_expressed() {
        // A pill is half the height of its box. The tallest thing a 3–6px
        // radius could round into a pill is 12px, which is below the smallest
        // control in the type ramp — so no composition of these tokens is one.
        for r in Radius::ALL {
            assert!(r.px_u8() <= Radius::MAX_PX);
        }
        assert_eq!(Radius::CUT.px(), 5.0, "--pill is a 5px chamfer, nothing more");
    }

    #[test]
    fn clamping_never_leaves_the_band() {
        assert_eq!(Radius::clamped(0), Radius::XS);
        assert_eq!(Radius::clamped(4), Radius::SM);
        assert_eq!(Radius::clamped(200), Radius::CARD);
    }

    #[test]
    fn circles_carry_their_justification() {
        let dot = Circle::status_dot(6.0);
        assert_eq!(dot.purpose, CircleUse::StatusDot);
        assert_eq!(dot.radius_px(), 3.0);
        assert_eq!(Circle::spinner(16.0).purpose, CircleUse::Spinner);
    }
}
