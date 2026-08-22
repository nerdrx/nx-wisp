//! DESIGN.md §1 and the brand block of §2, verbatim. **Frozen — never
//! restyle these.**

use crate::color::Color;

// ---- the field -------------------------------------------------------------
/// Top of the deep-space field. Never flat black.
pub const BG_TOP: Color = Color::hex(0x0a0714);
/// Bottom of the deep-space field.
pub const BG_BOTTOM: Color = Color::hex(0x12091f);

// ---- structural violet ramp ------------------------------------------------
pub const PANEL: Color = Color::hex(0x171028);
pub const PANEL_2: Color = Color::hex(0x1d1433);
/// The only line colour in the system, and even it is used as a gradient.
pub const LINE: Color = Color::hex(0x2a1f45);

// ---- brand -----------------------------------------------------------------
/// NX Violet. Actions, focus, identity. Violet **dominates**.
pub const VIOLET: Color = Color::hex(0x7700ff);
pub const VIOLET_SOFT: Color = Color::hex(0x9a3cff);
/// Light *inside* materials: edges, live status, progress. Subordinate to
/// violet, and never a competing surface colour.
pub const CYAN: Color = Color::hex(0x00e5ff);
/// Update / attention. Nothing else.
pub const AMBER: Color = Color::hex(0xffb300);
/// Danger only — genuinely destructive actions and failures.
pub const DANGER: Color = Color::hex(0xff5470);

// ---- type ------------------------------------------------------------------
pub const TEXT: Color = Color::hex(0xefeaff);
pub const MUTED: Color = Color::hex(0x9a8fc0);

// ---- elevation ramp (v1.5 §12: structural surfaces are OPAQUE) -------------
/// `--surface-1` stop 0. The lit upper-left corner of a card.
pub const SURFACE_1_TOP: Color = Color::hex(0x221739);
pub const SURFACE_1_MID: Color = Color::hex(0x1a1130);
pub const SURFACE_1_BOTTOM: Color = Color::hex(0x140c24);
pub const SURFACE_1_HOVER_TOP: Color = Color::hex(0x271b41);
pub const SURFACE_1_HOVER_MID: Color = Color::hex(0x1d1335);
pub const SURFACE_1_HOVER_BOTTOM: Color = Color::hex(0x170e29);

/// Every frozen brand anchor, for the round-trip test and for tooling that
/// wants to enumerate the palette (the settings panel's about page).
pub const ALL: &[(&str, &Color)] = &[
    ("bg-top", &BG_TOP),
    ("bg-bottom", &BG_BOTTOM),
    ("panel", &PANEL),
    ("panel-2", &PANEL_2),
    ("line", &LINE),
    ("violet", &VIOLET),
    ("violet-soft", &VIOLET_SOFT),
    ("cyan", &CYAN),
    ("amber", &AMBER),
    ("danger", &DANGER),
    ("text", &TEXT),
    ("muted", &MUTED),
    ("surface-1-top", &SURFACE_1_TOP),
    ("surface-1-mid", &SURFACE_1_MID),
    ("surface-1-bottom", &SURFACE_1_BOTTOM),
    ("surface-1-hover-top", &SURFACE_1_HOVER_TOP),
    ("surface-1-hover-mid", &SURFACE_1_HOVER_MID),
    ("surface-1-hover-bottom", &SURFACE_1_HOVER_BOTTOM),
];

/// The one app-specific colour an NX app is permitted (DESIGN.md §1). It marks
/// *only* its domain signal — never an action, never danger, never generic
/// status. Wisp does not currently claim one; the type exists so that when she
/// does, the constraint is written down next to the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityAccent {
    pub color: Color,
    /// What single signal this colour is allowed to mark.
    pub signal: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn violet_is_darker_than_cyan_so_it_can_lead_without_shouting() {
        // Cyan is light inside materials; if it out-massed violet as a fill the
        // hierarchy of §1 would invert.
        assert!(CYAN.luminance() > VIOLET.luminance());
    }

    #[test]
    fn the_field_is_never_flat_black() {
        assert!(BG_TOP.luminance() > 0.0);
        assert_ne!(BG_TOP, BG_BOTTOM);
        // ...and it darkens upward-to-downward by only a whisper.
        assert!(BG_BOTTOM.luminance() > BG_TOP.luminance());
    }

    #[test]
    fn the_elevation_ramp_steps_up_on_hover() {
        assert!(SURFACE_1_HOVER_TOP.luminance() > SURFACE_1_TOP.luminance());
        assert!(SURFACE_1_HOVER_MID.luminance() > SURFACE_1_MID.luminance());
        assert!(SURFACE_1_HOVER_BOTTOM.luminance() > SURFACE_1_BOTTOM.luminance());
    }
}
