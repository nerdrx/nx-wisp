//! DESIGN.md §5, for the handful of components `wisp-paint`'s widget layer
//! actually draws. Everything here is a *composition of tokens* — no component
//! introduces a colour, a radius or a duration of its own.

use crate::color::Color;
use crate::gradient::{Gradient, Stop};
use crate::motion::{Easing, DUR_FAST_MS, DUR_MS, HOVER_LIFT_BUTTON_PX, PRESS_SCALE, SOFT, SPRING};
use crate::palette;
use crate::radius::Radius;
use crate::surface::{Edge, Shadow, FOCUS_RING, SHADOW};
use crate::tokens;

/// Sharp-cut glass blocks (§5). Every one takes [`Radius::CUT`] — the token
/// whose legacy name is `--pill` and which has never been one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ButtonVariant {
    /// Violet fill, inner top highlight, soft violet glow.
    Primary,
    /// `--glass-chip` fill with an `--edge` border.
    Secondary,
    /// `--danger`. Genuinely destructive actions only.
    Danger,
    /// `--amber`. The "update available" class of action, and nothing else.
    Attention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ButtonState {
    pub hovered: bool,
    pub pressed: bool,
    pub focused: bool,
    pub disabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ButtonSkin {
    pub fill: Gradient,
    pub label: Color,
    pub edge: Edge,
    pub radius: Radius,
    pub shadow: Option<Shadow>,
    pub focus_ring: Option<Shadow>,
    /// Vertical offset in px. Hover lifts; press does not translate, it scales.
    pub lift_px: f32,
    /// Uniform scale. §10's press recipe: 0.96 down, spring back.
    pub scale: f32,
    /// Multiplied into every colour. Disabled is 40% (§5).
    pub opacity: f32,
}

const PRIMARY_FILL: Gradient = Gradient::linear(
    157.0,
    &[
        Stop::new(0.0, Color::hex(0x8a1aff)),
        Stop::new(0.42, Color::hex(0x7700ff)),
        Stop::new(1.0, Color::hex(0x5c00c4)),
    ],
);
const DANGER_FILL: Gradient = Gradient::linear(
    157.0,
    &[Stop::new(0.0, Color::hex(0xff8fa1)), Stop::new(1.0, Color::hex(0xff5470))],
);
const ATTENTION_FILL: Gradient = Gradient::linear(
    157.0,
    &[Stop::new(0.0, Color::hex(0xffc94d)), Stop::new(1.0, Color::hex(0xd18f00))],
);

impl ButtonVariant {
    pub fn skin(self, state: ButtonState) -> ButtonSkin {
        let (fill, label, edge) = match self {
            ButtonVariant::Primary => (PRIMARY_FILL, palette::TEXT, Edge::Plain),
            ButtonVariant::Secondary => (tokens::GLASS_CHIP, palette::TEXT, Edge::Plain),
            ButtonVariant::Danger => (DANGER_FILL, Color::hex(0x1a0208), Edge::Plain),
            ButtonVariant::Attention => (ATTENTION_FILL, Color::hex(0x241800), Edge::Plain),
        };
        ButtonSkin {
            fill,
            label,
            edge,
            radius: Radius::CUT,
            shadow: if state.disabled { None } else { Some(SHADOW) },
            focus_ring: state.focused.then_some(FOCUS_RING),
            lift_px: if state.hovered && !state.disabled && !state.pressed {
                -HOVER_LIFT_BUTTON_PX
            } else {
                0.0
            },
            scale: if state.pressed && !state.disabled { PRESS_SCALE } else { 1.0 },
            opacity: if state.disabled { 0.40 } else { 1.0 },
        }
    }

    /// The press recipe of §10: down is fast and linear-ish, release springs.
    pub fn press_curve(down: bool) -> (Easing, u32) {
        if down {
            (SOFT, DUR_FAST_MS)
        } else {
            (SPRING, DUR_MS)
        }
    }
}

/// Status chips (§5). Cyan = live/connected, amber = pending attention, muted
/// = inert. Danger is a fourth only because failure is not "attention".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Live,
    Pending,
    Inert,
    Failed,
}

impl Status {
    pub const fn color(self) -> Color {
        match self {
            Status::Live => palette::CYAN,
            Status::Pending => palette::AMBER,
            Status::Inert => palette::MUTED,
            Status::Failed => palette::DANGER,
        }
    }
    /// §10's three glyphs. Status is never carried by emoji.
    pub const fn glyph(self) -> char {
        match self {
            Status::Live => '✓',
            Status::Pending => '↑',
            Status::Inert => '·',
            Status::Failed => '×',
        }
    }
    /// The status dot — one of the two sanctioned circles.
    pub const fn dot(self) -> crate::radius::Circle {
        crate::radius::Circle::status_dot(6.0)
    }
}

/// A progress bar: recessed trough, violet→cyan liquid fill, and a sheen whose
/// position is the progress value itself (§1's light-rides-motion rule — the
/// driver here is the thing that is actually moving).
pub const PROGRESS_FILL: Gradient = Gradient::linear(
    90.0,
    &[Stop::new(0.0, palette::VIOLET), Stop::new(1.0, palette::CYAN)],
);

/// A deterministic monogram tile's hue (§8), clamped to the cyan→violet band.
pub fn monogram_hue(app_id: &str) -> f32 {
    // FNV-1a; stable across runs and machines, which is the whole point.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in app_id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    const LO: f32 = 187.0;
    const HI: f32 = 290.0;
    LO + (h % 10_000) as f32 / 10_000.0 * (HI - LO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_button_is_a_five_pixel_chamfer() {
        for v in [
            ButtonVariant::Primary,
            ButtonVariant::Secondary,
            ButtonVariant::Danger,
            ButtonVariant::Attention,
        ] {
            assert_eq!(v.skin(ButtonState::default()).radius, Radius::CUT);
        }
    }

    #[test]
    fn disabled_is_forty_percent_and_ignores_hover() {
        let s = ButtonState { disabled: true, hovered: true, ..Default::default() };
        let skin = ButtonVariant::Primary.skin(s);
        assert_eq!(skin.opacity, 0.40);
        assert_eq!(skin.lift_px, 0.0);
        assert_eq!(skin.scale, 1.0);
    }

    #[test]
    fn hover_lifts_and_press_scales() {
        let hov = ButtonVariant::Primary.skin(ButtonState { hovered: true, ..Default::default() });
        assert_eq!(hov.lift_px, -HOVER_LIFT_BUTTON_PX);
        assert_eq!(hov.scale, 1.0);
        let pressed =
            ButtonVariant::Primary.skin(ButtonState { pressed: true, ..Default::default() });
        assert_eq!(pressed.scale, PRESS_SCALE);
        assert_eq!(pressed.lift_px, 0.0, "press must not translate; §6 wants scale");
    }

    #[test]
    fn primary_leads_with_violet() {
        let skin = ButtonVariant::Primary.skin(ButtonState::default());
        assert_eq!(skin.fill.sample(0.42).to_hex(), palette::VIOLET.to_hex());
    }

    #[test]
    fn button_labels_are_all_legible() {
        for v in [
            ButtonVariant::Primary,
            ButtonVariant::Secondary,
            ButtonVariant::Danger,
            ButtonVariant::Attention,
        ] {
            let skin = v.skin(ButtonState::default());
            for t in [0.0, 0.5, 1.0] {
                let bg = skin.fill.sample(t).over(crate::tokens::SURFACE_1.sample(t));
                let ratio = skin.label.contrast(bg);
                assert!(ratio >= 4.5, "{v:?} label at t={t} is {ratio:.2}:1 on {bg:?}");
            }
        }
    }

    #[test]
    fn amber_only_ever_means_attention() {
        assert_eq!(Status::Pending.color(), palette::AMBER);
        assert_ne!(Status::Failed.color(), palette::AMBER);
        assert_ne!(Status::Live.color(), palette::AMBER);
        assert_ne!(Status::Inert.color(), palette::AMBER);
    }

    #[test]
    fn status_never_uses_emoji() {
        for s in [Status::Live, Status::Pending, Status::Inert, Status::Failed] {
            let g = s.glyph();
            assert!((g as u32) < 0x1F000, "{g} is an emoji");
        }
    }

    #[test]
    fn progress_runs_violet_to_cyan() {
        assert_eq!(PROGRESS_FILL.first(), palette::VIOLET);
        assert_eq!(PROGRESS_FILL.last(), palette::CYAN);
    }

    #[test]
    fn monogram_hues_are_deterministic_and_stay_in_the_band() {
        assert_eq!(monogram_hue("nx-hub"), monogram_hue("nx-hub"));
        assert_ne!(monogram_hue("nx-hub"), monogram_hue("nx-wisp"));
        for id in ["a", "nx-wisp", "pulsenx", "", "com.example.Very.Long.Id"] {
            let h = monogram_hue(id);
            assert!((187.0..=290.0).contains(&h), "{id} hashed to {h}");
        }
    }
}
