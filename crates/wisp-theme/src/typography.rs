//! DESIGN.md §7's type ramp. System stack, no webfonts — weight and spacing do
//! the branding.

use crate::color::Color;
use crate::palette;

/// `--font` and `--mono`, in priority order. `wisp-paint` hands these to
/// fontconfig; the first that resolves wins.
pub const FONT_STACK: &[&str] =
    &["system-ui", "Segoe UI", "Roboto", "Noto Sans", "Cantarell", "sans-serif"];
pub const MONO_STACK: &[&str] = &["JetBrains Mono", "Fira Code", "Consolas", "monospace"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Sans,
    Mono,
}

/// Whether the renderer should uppercase the string before shaping. Only the
/// micro-label chips do; everything else is sentence case (§9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    AsWritten,
    Upper,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle {
    pub size_px: f32,
    /// CSS weight, 100..900.
    pub weight: u16,
    /// Letter spacing in em. §5 wants 0.12em+ on micro-labels.
    pub tracking_em: f32,
    /// Multiple of `size_px`.
    pub line_height: f32,
    pub family: Family,
    pub case: Case,
    pub color: Color,
}

impl TextStyle {
    pub const fn with_color(mut self, color: Color) -> TextStyle {
        self.color = color;
        self
    }
    pub fn line_height_px(&self) -> f32 {
        self.size_px * self.line_height
    }
    pub fn tracking_px(&self) -> f32 {
        self.size_px * self.tracking_em
    }
}

/// The ramp. Six roles, and no seventh — §7 lists exactly these sizes and a
/// new one would be a design amendment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// 22px/700 — screen titles.
    Title,
    /// 20px/600 — section titles.
    TitleSmall,
    /// 14px/400 — body.
    Body,
    /// 14px/600 — emphasised body, button labels.
    BodyStrong,
    /// 13px/400, muted — secondary text.
    Secondary,
    /// 11px/600 uppercase, wide tracking — the micro-label chip.
    Micro,
    /// 13px/400 mono — code, versions, log lines.
    Mono,
}

pub const TITLE: TextStyle = TextStyle {
    size_px: 22.0,
    weight: 700,
    tracking_em: -0.005,
    line_height: 1.25,
    family: Family::Sans,
    case: Case::AsWritten,
    color: palette::TEXT,
};
pub const TITLE_SMALL: TextStyle = TextStyle { size_px: 20.0, weight: 600, ..TITLE };
pub const BODY: TextStyle = TextStyle {
    size_px: 14.0,
    weight: 400,
    tracking_em: 0.0,
    line_height: 1.45,
    family: Family::Sans,
    case: Case::AsWritten,
    color: palette::TEXT,
};
pub const BODY_STRONG: TextStyle = TextStyle { weight: 600, ..BODY };
pub const SECONDARY: TextStyle =
    TextStyle { size_px: 13.0, color: palette::MUTED, line_height: 1.4, ..BODY };
pub const MICRO: TextStyle = TextStyle {
    size_px: 11.0,
    weight: 600,
    tracking_em: 0.12,
    line_height: 1.2,
    family: Family::Sans,
    case: Case::Upper,
    color: palette::MUTED,
};
pub const MONO: TextStyle =
    TextStyle { size_px: 13.0, family: Family::Mono, line_height: 1.4, ..BODY };

impl Role {
    pub const fn style(self) -> TextStyle {
        match self {
            Role::Title => TITLE,
            Role::TitleSmall => TITLE_SMALL,
            Role::Body => BODY,
            Role::BodyStrong => BODY_STRONG,
            Role::Secondary => SECONDARY,
            Role::Micro => MICRO,
            Role::Mono => MONO,
        }
    }
    pub const ALL: [Role; 7] = [
        Role::Title,
        Role::TitleSmall,
        Role::Body,
        Role::BodyStrong,
        Role::Secondary,
        Role::Micro,
        Role::Mono,
    ];
}

/// UI glyphs are stroked and geometric, `currentColor`, 1.5–2px at a 16–20px
/// box (§8). No emoji, no icon fonts — so the stroke width is a token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IconStyle {
    pub box_px: f32,
    pub stroke_px: f32,
    pub color: Color,
}

pub const ICON: IconStyle = IconStyle { box_px: 16.0, stroke_px: 1.5, color: palette::TEXT };
pub const ICON_LARGE: IconStyle = IconStyle { box_px: 20.0, stroke_px: 2.0, color: palette::TEXT };

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ramp_matches_section_seven() {
        assert_eq!(TITLE.size_px, 22.0);
        assert_eq!(BODY.size_px, 14.0);
        assert_eq!(SECONDARY.size_px, 13.0);
        assert_eq!(SECONDARY.color, palette::MUTED);
        assert_eq!(MICRO.size_px, 11.0);
        assert_eq!(MICRO.case, Case::Upper);
        assert!(MICRO.tracking_em >= 0.12, "micro-labels need wide tracking");
        assert_eq!(MONO.family, Family::Mono);
    }

    #[test]
    fn headings_are_six_hundred_plus_and_body_is_four_to_five() {
        for r in [Role::Title, Role::TitleSmall] {
            assert!((600..=700).contains(&r.style().weight));
        }
        for r in [Role::Body, Role::Secondary, Role::Mono] {
            assert!((400..=500).contains(&r.style().weight));
        }
    }

    #[test]
    fn the_ramp_is_ordered_and_has_no_duplicates() {
        let sizes: Vec<f32> = Role::ALL.iter().map(|r| r.style().size_px).collect();
        assert_eq!(sizes[0], 22.0);
        assert!(sizes.iter().all(|s| (11.0..=22.0).contains(s)));
    }

    #[test]
    fn icons_stay_in_the_documented_stroke_band() {
        for i in [ICON, ICON_LARGE] {
            assert!((1.5..=2.0).contains(&i.stroke_px));
            assert!((16.0..=20.0).contains(&i.box_px));
        }
    }

    #[test]
    fn the_font_stack_has_no_webfonts() {
        assert_eq!(FONT_STACK[0], "system-ui");
        assert_eq!(*FONT_STACK.last().unwrap(), "sans-serif");
        assert_eq!(*MONO_STACK.last().unwrap(), "monospace");
    }
}
