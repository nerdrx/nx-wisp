//! DESIGN.md §2, transcribed. Every gradient here is the CSS token with the
//! same name, stop for stop, angle for angle.
//!
//! Two of these are marked **legacy**: `GLASS_BAR` and `GLASS_2` predate the
//! v1.5 opacity rule and their body alpha is below the 0.85 floor §4 now
//! demands of a floating layer. They stay, verbatim, because DESIGN.md still
//! lists them; but [`crate::surface::Floating`] does **not** hand them out —
//! it returns the v1.5-conformant bodies in [`glass`]. See
//! `surface::tests::floating_bodies_meet_the_v15_alpha_floor`.

use crate::color::Color;
use crate::gradient::{Gradient, Stop};
use crate::palette::*;

/// The paint record on the GPU holds this many stops. `--hairline` needs five.
pub const MAX_STOPS: usize = 6;

// ---------------------------------------------------------------- the field
/// `--bg-top → --bg-bottom`. Never flat black.
pub const FIELD: Gradient =
    Gradient::linear(180.0, &[Stop::new(0.0, BG_TOP), Stop::new(1.0, BG_BOTTOM)]);

/// §3 nebula blobs — enormous, very low alpha, drifting on 60–110s periods.
/// These are *ambient field light*, not surface shading, so they are exempt
/// from the upper-left rule that binds every bevel and edge.
pub const NEBULA_VIOLET: Gradient = Gradient::radial(
    0.18,
    0.12,
    0.85,
    0.85,
    &[Stop::new(0.0, Color::hexa(0x7700ff, 0.20)), Stop::new(1.0, Color::hexa(0x7700ff, 0.0))],
);
pub const NEBULA_CYAN: Gradient = Gradient::radial(
    0.86,
    0.84,
    0.75,
    0.75,
    &[Stop::new(0.0, Color::hexa(0x00e5ff, 0.10)), Stop::new(1.0, Color::hexa(0x00e5ff, 0.0))],
);
pub const NEBULA_MAGENTA: Gradient = Gradient::radial(
    0.62,
    0.30,
    0.60,
    0.60,
    &[Stop::new(0.0, Color::hexa(0xc02bff, 0.07)), Stop::new(1.0, Color::hexa(0xc02bff, 0.0))],
);

// -------------------------------------------------- structural (v1.5 opaque)
/// `--surface-1` — the opaque elevation step every card and tile takes.
pub const SURFACE_1: Gradient = Gradient::linear(
    157.0,
    &[
        Stop::new(0.0, SURFACE_1_TOP),
        Stop::new(0.44, SURFACE_1_MID),
        Stop::new(1.0, SURFACE_1_BOTTOM),
    ],
);
/// `--surface-1-hover`.
pub const SURFACE_1_HOVER: Gradient = Gradient::linear(
    157.0,
    &[
        Stop::new(0.0, SURFACE_1_HOVER_TOP),
        Stop::new(0.44, SURFACE_1_HOVER_MID),
        Stop::new(1.0, SURFACE_1_HOVER_BOTTOM),
    ],
);
/// The rail / sidebar step, one notch below a card.
pub const SURFACE_0: Gradient =
    Gradient::linear(157.0, &[Stop::new(0.0, PANEL_2), Stop::new(1.0, PANEL)]);

// --------------------------------------------------------------- glass fills
/// **Legacy.** `--glass-bar` as written in §2 — body alpha 0.62..0.72, below
/// the v1.5 floor.
pub const GLASS_BAR_LEGACY: Gradient = Gradient::linear(
    180.0,
    &[
        Stop::new(0.0, Color::rgba(46, 30, 78, crate::color::af(0.62))),
        Stop::new(1.0, Color::rgba(18, 11, 34, crate::color::af(0.72))),
    ],
);
/// `--glass-1`. Kept for legacy/edge uses; new structural surfaces take
/// [`SURFACE_1`].
pub const GLASS_1: Gradient = Gradient::linear(
    157.0,
    &[
        Stop::new(0.0, Color::hexa(0xffffff, 0.09)),
        Stop::new(0.34, Color::hexa(0xffffff, 0.026)),
        Stop::new(1.0, Color::rgba(23, 16, 40, crate::color::af(0.34))),
    ],
);
/// **Legacy.** `--glass-2` as written in §2 — body alpha 0.66.
pub const GLASS_2_LEGACY: Gradient = Gradient::linear(
    158.0,
    &[
        Stop::new(0.0, Color::hexa(0xffffff, 0.10)),
        Stop::new(0.30, Color::hexa(0xffffff, 0.03)),
        Stop::new(1.0, Color::rgba(19, 12, 34, crate::color::af(0.66))),
    ],
);
/// `--glass-chip`.
pub const GLASS_CHIP: Gradient = Gradient::linear(
    180.0,
    &[Stop::new(0.0, Color::hexa(0xffffff, 0.09)), Stop::new(1.0, Color::hexa(0xffffff, 0.028))],
);

/// v1.5-conformant floating bodies: the legacy ramps with their alpha lifted
/// past the 0.85 floor, so nothing behind a bar, sheet, menu or toast shows
/// through its body. Blur is finish; the fill is the mechanism.
pub mod glass {
    use super::*;

    /// Body of a bar. `--glass-bar`'s hues, ≥0.94 alpha.
    pub const BAR_BODY: Gradient = Gradient::linear(
        180.0,
        &[
            Stop::new(0.0, Color::rgba(46, 30, 78, crate::color::af(0.94))),
            Stop::new(1.0, Color::rgba(18, 11, 34, crate::color::af(0.97))),
        ],
    );
    /// Body of a sheet, menu or toast. `--glass-2`'s hue, ≥0.93 alpha.
    pub const SHEET_BODY: Gradient = Gradient::linear(
        158.0,
        &[
            Stop::new(0.0, Color::rgba(34, 23, 57, crate::color::af(0.93))),
            Stop::new(1.0, Color::rgba(19, 12, 34, crate::color::af(0.96))),
        ],
    );
    /// Body of a floating chip / tooltip.
    pub const CHIP_BODY: Gradient = Gradient::linear(
        180.0,
        &[
            Stop::new(0.0, Color::rgba(40, 27, 66, crate::color::af(0.93))),
            Stop::new(1.0, Color::rgba(23, 16, 40, crate::color::af(0.96))),
        ],
    );

    /// The white top-left specular that rides on top of a glass body. Drawn as
    /// a second layer, which is exactly why the body alpha can be honest.
    pub const HIGHLIGHT: Gradient = Gradient::linear(
        157.0,
        &[
            Stop::new(0.0, Color::hexa(0xffffff, 0.10)),
            Stop::new(0.30, Color::hexa(0xffffff, 0.03)),
            Stop::new(1.0, Color::hexa(0xffffff, 0.0)),
        ],
    );
}

// ---------------------------------------------------------------------- wells
/// `--well`. Recessed: darkest at the lip the light cannot reach.
pub const WELL: Gradient = Gradient::linear(
    180.0,
    &[
        Stop::new(0.0, Color::rgba(7, 4, 16, crate::color::af(0.50))),
        Stop::new(1.0, Color::rgba(7, 4, 16, crate::color::af(0.32))),
    ],
);
/// `--well-deep`.
pub const WELL_DEEP: Gradient = Gradient::linear(
    180.0,
    &[
        Stop::new(0.0, Color::rgba(4, 2, 10, crate::color::af(0.62))),
        Stop::new(1.0, Color::rgba(4, 2, 10, crate::color::af(0.46))),
    ],
);

// ------------------------------------------------------------------ lit edges
/// `--edge` — the 1px gradient border. Bright top-left → dark bottom-right.
pub const EDGE: Gradient = Gradient::linear(
    147.0,
    &[
        Stop::new(0.0, Color::hexa(0xffffff, 0.34)),
        Stop::new(0.24, Color::hexa(0xffffff, 0.09)),
        Stop::new(0.52, Color::hexa(0xffffff, 0.015)),
        Stop::new(1.0, Color::hexa(0x000000, 0.34)),
    ],
);
/// `--edge-lit` — the sheet edge, where violet and cyan live *in* the glass.
pub const EDGE_LIT: Gradient = Gradient::linear(
    147.0,
    &[
        Stop::new(0.0, Color::rgba(226, 200, 255, crate::color::af(0.62))),
        Stop::new(0.30, Color::rgba(154, 60, 255, crate::color::af(0.28))),
        Stop::new(0.58, Color::rgba(0, 229, 255, crate::color::af(0.10))),
        Stop::new(1.0, Color::hexa(0x000000, 0.30)),
    ],
);
/// `--edge-top` — the single lit hairline along the top of a bar.
pub const EDGE_TOP: Color = Color::hexa(0xffffff, 0.18);
/// `--hairline` — a divider that fades out at both ends. There are no solid
/// grey lines anywhere in this language.
pub const HAIRLINE: Gradient = Gradient::linear(
    90.0,
    &[
        Stop::new(0.0, Color::hexa(0xffffff, 0.0)),
        Stop::new(0.18, Color::hexa(0xffffff, 0.09)),
        Stop::new(0.50, Color::hexa(0xffffff, 0.13)),
        Stop::new(0.82, Color::hexa(0xffffff, 0.09)),
        Stop::new(1.0, Color::hexa(0xffffff, 0.0)),
    ],
);
/// `--sheen` — the specular band. **Position-driven, never time-triggered**
/// (§1's light-rides-motion rule): the renderer slides this by writing a
/// normalised driver value, it never plays it as an animation.
pub const SHEEN: Gradient = Gradient::linear(
    112.0,
    &[
        Stop::new(0.30, Color::hexa(0xffffff, 0.0)),
        Stop::new(0.45, Color::hexa(0xffffff, 0.085)),
        Stop::new(0.52, Color::rgba(214, 190, 255, crate::color::af(0.05))),
        Stop::new(0.68, Color::hexa(0xffffff, 0.0)),
    ],
);

/// Every gradient that shades a **raised** face and must therefore be brightest
/// at its upper-left.
pub const RAISED: &[(&str, Gradient)] = &[
    ("surface-0", SURFACE_0),
    ("surface-1", SURFACE_1),
    ("surface-1-hover", SURFACE_1_HOVER),
    ("glass-bar-legacy", GLASS_BAR_LEGACY),
    ("glass-1", GLASS_1),
    ("glass-2-legacy", GLASS_2_LEGACY),
    ("glass-chip", GLASS_CHIP),
    ("glass::bar-body", glass::BAR_BODY),
    ("glass::sheet-body", glass::SHEET_BODY),
    ("glass::chip-body", glass::CHIP_BODY),
    ("glass::highlight", glass::HIGHLIGHT),
    ("edge", EDGE),
    ("edge-lit", EDGE_LIT),
];

/// The field, which is behind everything and shades nothing.
pub const AMBIENT: &[(&str, Gradient)] = &[("field", FIELD)];

/// Every gradient that shades a **recessed** face and must therefore be
/// darkest at its upper lip.
pub const RECESSED: &[(&str, Gradient)] = &[("well", WELL), ("well-deep", WELL_DEEP)];

/// Overlays: they have a direction but no elevation, so only the direction
/// rule binds them.
pub const OVERLAY: &[(&str, Gradient)] = &[("hairline", HAIRLINE), ("sheen", SHEEN)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_legacy_glass_tokens_are_the_ones_v15_outgrew() {
        // Recorded as a fact, not an accident: this is the DESIGN.md v1.5
        // internal inconsistency (§4's table still points Bar at --glass-bar
        // while §4's prose demands ≥0.85). wisp-theme resolves it in favour of
        // the prose; the legacy tokens remain reachable and clearly named.
        assert!(GLASS_BAR_LEGACY.min_alpha() < 0.85);
        assert!(GLASS_2_LEGACY.min_alpha() < 0.85);
    }

    #[test]
    fn no_token_exceeds_the_paint_records_stop_count() {
        for (name, g) in RAISED.iter().chain(RECESSED).chain(OVERLAY) {
            assert!(g.stops.len() <= MAX_STOPS, "{name}");
        }
        assert_eq!(HAIRLINE.stops.len(), 5, "the hairline is why MAX_STOPS is not 4");
    }

    #[test]
    fn overlays_still_travel_down_and_right() {
        for (name, g) in OVERLAY {
            let (dx, dy) = g.direction();
            assert!(dx >= -1e-4 && dy >= -1e-4, "{name} travels toward the light");
        }
    }

    #[test]
    fn the_hairline_fades_at_both_ends() {
        assert_eq!(HAIRLINE.first().a, 0);
        assert_eq!(HAIRLINE.last().a, 0);
        assert!(HAIRLINE.sample(0.5).a > 0);
    }

    #[test]
    fn nebula_blobs_are_invisible_from_across_the_room() {
        for g in [NEBULA_VIOLET, NEBULA_CYAN, NEBULA_MAGENTA] {
            assert!(g.first().alpha_f() <= 0.20, "halve it");
            assert_eq!(g.last().a, 0, "a blob must fade to nothing");
        }
    }
}
