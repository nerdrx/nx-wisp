//! **wisp-theme** — the NX design language (DESIGN.md v1.5) as Rust.
//!
//! SPEC.md §2 gives this crate the DESIGN.md token port: palette, radii,
//! easings, glass tiers, type ramp. It is also the reference *native*
//! implementation of the language — every other NX project has expressed it in
//! CSS, and this is the version with a type system underneath it.
//!
//! Three rules are load-bearing enough that they are enforced by types or by
//! tests rather than by review:
//!
//! 1. **v1.5's opacity rule** (§4, §12). Structural surfaces are opaque
//!    elevation steps; only genuinely floating layers may be translucent, and
//!    only they may carry a real blur. See [`surface`] — a card's material
//!    struct has no `blur` field to fill in.
//! 2. **Angular, never rounded** (§1). [`radius::Radius`] cannot hold a value
//!    outside 3..=6, and a perfect circle can only be built by naming one of
//!    the two sanctioned uses.
//! 3. **One light source, upper-left** (§1). Every gradient token is checked
//!    against [`gradient::LightModel`] in the test suite.
//!
//! Nothing here touches a GPU, a compositor or the filesystem: it is constants
//! and pure functions, so `cargo test -p wisp-theme` runs anywhere.

pub mod color;
pub mod component;
pub mod gradient;
pub mod motion;
pub mod palette;
pub mod radius;
pub mod space;
pub mod surface;
pub mod tokens;
pub mod typography;

pub use color::Color;
pub use gradient::{Gradient, GradientKind, LightModel, Stop};
pub use motion::{Easing, Motion, Sheen};
pub use radius::{Circle, CircleUse, Radius};
pub use space::{Insets, Space};
pub use surface::{
    Blur, Edge, Floating, GlassMaterial, InsetMaterial, OpaqueMaterial, Recessed, Shadow, Structural,
    Surface,
};
pub use typography::{IconStyle, Role, TextStyle};

/// Something that would have broken the design language, refused at the point
/// it was expressed rather than caught in a screenshot review.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ThemeViolation {
    /// Below the 3px floor — that is a square corner, not a radius.
    RadiusTooSharp(u8),
    /// Above the 6px ceiling. Large radii read as a toy; pills are banned.
    RadiusTooRound(u8),
    /// Off the 8px grid (§7).
    OffGrid(u16),
    /// A structural surface tried to be see-through (v1.5 §4).
    TranslucentStructuralSurface(f32),
    /// A floating layer's body fell below the 0.85 alpha floor (v1.5 §4).
    FloatingFillTooSheer(f32),
    /// A gradient that shades a face is brightest on the wrong side (§1).
    LightFromTheWrongSide,
}

impl core::fmt::Display for ThemeViolation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ThemeViolation::RadiusTooSharp(px) => {
                write!(f, "{px}px is below the 3px radius floor — use a square corner instead")
            }
            ThemeViolation::RadiusTooRound(px) => {
                write!(f, "{px}px is above the 6px radius ceiling — pills are banned")
            }
            ThemeViolation::OffGrid(px) => {
                write!(f, "{px}px is off the 8px grid — use Space::units or Space::snap")
            }
            ThemeViolation::TranslucentStructuralSurface(a) => write!(
                f,
                "structural surfaces are opaque elevation steps; this one has alpha {a:.2}"
            ),
            ThemeViolation::FloatingFillTooSheer(a) => write!(
                f,
                "a floating layer's body must be at least 0.85 alpha; this one is {a:.2}"
            ),
            ThemeViolation::LightFromTheWrongSide => {
                write!(f, "light comes from the upper-left, in every gradient and edge")
            }
        }
    }
}

impl core::error::Error for ThemeViolation {}

/// The version of DESIGN.md this port tracks. Bump it deliberately, with the
/// diff, exactly like the document itself.
pub const DESIGN_VERSION: &str = "1.5";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whole_language_validates() {
        for s in surface::all_surfaces() {
            s.validate().unwrap_or_else(|e| panic!("{s:?}: {e}"));
        }
    }

    #[test]
    fn violations_say_what_to_do_next() {
        // §9: errors say what happened *and what to do next*. That applies to
        // the ones we show ourselves too.
        for v in [
            ThemeViolation::RadiusTooRound(20),
            ThemeViolation::OffGrid(13),
            ThemeViolation::FloatingFillTooSheer(0.5),
        ] {
            let s = v.to_string();
            assert!(!s.is_empty());
            assert!(!s.contains('!'), "no exclamation marks (§9): {s}");
        }
    }
}
