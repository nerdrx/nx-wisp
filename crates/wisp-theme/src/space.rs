//! DESIGN.md §7: **everything sits on the 8px grid.**

use crate::ThemeViolation;

/// A distance on the 8px grid. Private field, so an off-grid 13px gap cannot
/// be typed by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Space(u16);

/// The grid unit. Every space is a whole multiple of this.
pub const GRID_PX: u16 = 8;

impl Space {
    pub const ZERO: Space = Space(0);
    /// `--sp-1`
    pub const S1: Space = Space(8);
    /// `--sp-2` — the default padding *inside* anything.
    pub const S2: Space = Space(16);
    /// `--sp-3` — between siblings, and a card's internal padding.
    pub const S3: Space = Space(24);
    /// `--sp-4` — between sections.
    pub const S4: Space = Space(32);

    /// `n` grid units.
    pub const fn units(n: u16) -> Space {
        Space(n * GRID_PX)
    }

    /// Refuses anything off the grid.
    pub const fn px(px: u16) -> Result<Space, ThemeViolation> {
        if px % GRID_PX == 0 {
            Ok(Space(px))
        } else {
            Err(ThemeViolation::OffGrid(px))
        }
    }

    /// Snap to the nearest grid step. For layout code deriving a space from a
    /// measured size.
    pub const fn snap(px: u16) -> Space {
        Space(((px + GRID_PX / 2) / GRID_PX) * GRID_PX)
    }

    pub const fn get(self) -> f32 {
        self.0 as f32
    }

    pub const fn get_u16(self) -> u16 {
        self.0
    }

    pub const ALL: [Space; 4] = [Space::S1, Space::S2, Space::S3, Space::S4];
}

/// Padding, all four sides, each on the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Insets {
    pub top: Space,
    pub right: Space,
    pub bottom: Space,
    pub left: Space,
}

impl Insets {
    pub const ZERO: Insets =
        Insets { top: Space::ZERO, right: Space::ZERO, bottom: Space::ZERO, left: Space::ZERO };

    pub const fn all(s: Space) -> Insets {
        Insets { top: s, right: s, bottom: s, left: s }
    }
    pub const fn xy(x: Space, y: Space) -> Insets {
        Insets { top: y, right: x, bottom: y, left: x }
    }
    /// §7's default: 16 inside.
    pub const CONTENT: Insets = Insets::all(Space::S2);
    /// §5: a card takes `--sp-3` of internal padding.
    pub const CARD: Insets = Insets::all(Space::S3);

    pub const fn horizontal(self) -> f32 {
        self.left.get() + self.right.get()
    }
    pub const fn vertical(self) -> f32 {
        self.top.get() + self.bottom.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_lands_on_the_grid() {
        for s in Space::ALL {
            assert_eq!(s.get_u16() % GRID_PX, 0, "{s:?} is off the 8px grid");
        }
        assert_eq!(Space::S1.get(), 8.0);
        assert_eq!(Space::S4.get(), 32.0);
    }

    #[test]
    fn off_grid_values_are_refused() {
        assert!(Space::px(13).is_err());
        assert!(Space::px(24).is_ok());
        assert_eq!(Space::snap(13), Space::S2);
        assert_eq!(Space::snap(11), Space::S1);
        assert_eq!(Space::snap(5), Space::S1);
        assert_eq!(Space::snap(3), Space::ZERO);
        assert_eq!(Space::snap(0), Space::ZERO);
    }

    #[test]
    fn card_padding_is_sp3() {
        assert_eq!(Insets::CARD.left, Space::S3);
        assert_eq!(Insets::CARD.horizontal(), 48.0);
    }
}
