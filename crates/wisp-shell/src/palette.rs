//! The summon palette (F41): a glass command bar at her side.
//!
//! Click her — a click, not a drag — and it opens. Type, press Enter, and the
//! line is published as `Observation::Speech { final_: true }`: typed words
//! ARE speech, so the whole existing pipeline answers — the mind observes, the
//! budget approves, the bubble shows, the voice speaks. This module adds no
//! new way for anything to reach the operator; it is an *input*.
//!
//! Pure state + scene building, like `bubble`: no clock, no surface, no
//! keyboard — the host feeds it events and paints what it returns, so all of
//! the editing behaviour tests headless.

use wisp_paint::{Painter, Point, Rect, Scene, TextEngine};
use wisp_theme::{palette as pal, typography::Role, Floating, Radius};

use crate::bubble::{Live, TextSink};

/// Width of the bar in surface pixels; height follows the type ramp.
const WIDTH_PX: f32 = 380.0;
const HEIGHT_PX: f32 = 44.0;
const PAD_X: f32 = 12.0;
const GAP_FROM_HER: f32 = 14.0;
const MAX_LEN: usize = 400;

/// One editing action, decoded by the host from the compositor's key events.
/// The palette knows nothing about keysyms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(String),
    Backspace,
    /// Ctrl+Backspace / Ctrl+W.
    DeleteWord,
    Left,
    Right,
    Home,
    End,
    Submit,
    Dismiss,
}

/// What a key did, so the host knows when to act rather than repaint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Edited,
    /// The trimmed line; the palette has closed itself.
    Submitted(String),
    Dismissed,
    Ignored,
}

#[derive(Debug, Default)]
pub struct Palette {
    open: bool,
    text: String,
    /// Caret, as a byte offset that always sits on a char boundary.
    caret: usize,
}

impl Palette {
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn open(&mut self) {
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.text.clear();
        self.caret = 0;
    }

    pub fn key(&mut self, k: Key) -> Outcome {
        if !self.open {
            return Outcome::Ignored;
        }
        match k {
            Key::Char(s) => {
                // Control characters arrive as empty or C0 strings from xkb;
                // never let one into the line.
                let printable: String = s.chars().filter(|c| !c.is_control()).collect();
                if printable.is_empty() || self.text.len() + printable.len() > MAX_LEN {
                    return Outcome::Ignored;
                }
                self.text.insert_str(self.caret, &printable);
                self.caret += printable.len();
                Outcome::Edited
            }
            Key::Backspace => {
                if self.caret == 0 {
                    return Outcome::Ignored;
                }
                let prev = prev_boundary(&self.text, self.caret);
                self.text.replace_range(prev..self.caret, "");
                self.caret = prev;
                Outcome::Edited
            }
            Key::DeleteWord => {
                if self.caret == 0 {
                    return Outcome::Ignored;
                }
                let start = word_start(&self.text, self.caret);
                self.text.replace_range(start..self.caret, "");
                self.caret = start;
                Outcome::Edited
            }
            Key::Left => {
                if self.caret > 0 {
                    self.caret = prev_boundary(&self.text, self.caret);
                }
                Outcome::Edited
            }
            Key::Right => {
                if self.caret < self.text.len() {
                    self.caret = next_boundary(&self.text, self.caret);
                }
                Outcome::Edited
            }
            Key::Home => {
                self.caret = 0;
                Outcome::Edited
            }
            Key::End => {
                self.caret = self.text.len();
                Outcome::Edited
            }
            Key::Submit => {
                let line = self.text.trim().to_string();
                self.close();
                if line.is_empty() {
                    Outcome::Dismissed
                } else {
                    Outcome::Submitted(line)
                }
            }
            Key::Dismiss => {
                self.close();
                Outcome::Dismissed
            }
        }
    }

    /// Where the bar sits for a given anchor, kept inside the surface.
    pub fn rect(&self, anchor: Point, her_size: f32, bounds: Rect) -> Rect {
        let w = WIDTH_PX.min(bounds.w - 16.0);
        let x = (anchor.x - w * 0.5).clamp(8.0, (bounds.w - w - 8.0).max(8.0));
        // Above her by default; below when she is high on the surface.
        let above = anchor.y - her_size * 0.5 - GAP_FROM_HER - HEIGHT_PX;
        let y = if above >= 8.0 { above } else { anchor.y + her_size * 0.5 + GAP_FROM_HER };
        Rect::new(x, y.min(bounds.h - HEIGHT_PX - 8.0).max(8.0), w, HEIGHT_PX)
    }

    /// Paint the bar. `phase` (0..1, host-driven) blinks the caret; the
    /// palette owns no clock.
    pub fn paint(
        &self,
        anchor: Point,
        her_size: f32,
        bounds: Rect,
        phase: f32,
        painter: &Painter,
        engine: &mut TextEngine,
        scene: &mut Scene,
    ) {
        if !self.open {
            return;
        }
        let r = self.rect(anchor, her_size, bounds);
        scene.floating(r, Floating::Menu);

        let mut sink = Live::new(painter, engine);
        let inner = Rect::new(r.x + PAD_X, r.y, r.w - 2.0 * PAD_X, r.h);

        // The line, or the hint. The hint is muted; the operator's words are not.
        let mut style = Role::Body.style();
        let shown = if self.text.is_empty() {
            style = Role::Secondary.style();
            "ask me something".to_string()
        } else {
            self.text.clone()
        };
        let (_, th) = sink.measure(&shown, &style, None);
        let ty = r.y + (r.h - th).max(0.0) * 0.5;
        if let Some(run) = sink.run(&shown, &style, None) {
            scene.text(run.rect_at(inner.x, ty), run.tex.clone(), style.color);
        }

        // Caret at the byte offset, blinking on the host's phase.
        if phase < 0.5 {
            let upto = &self.text[..self.caret];
            let cx = if upto.is_empty() {
                inner.x
            } else {
                inner.x + sink.measure(upto, &style, None).0
            };
            scene.fill_rect(
                Rect::new(cx.min(inner.x + inner.w), ty, 2.0, th.max(style.size_px)),
                Radius::XS,
                pal::VIOLET,
            );
        }
    }
}

fn prev_boundary(s: &str, at: usize) -> usize {
    let mut i = at - 1;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_boundary(s: &str, at: usize) -> usize {
    let mut i = at + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn word_start(s: &str, at: usize) -> usize {
    let head = &s[..at];
    let trimmed = head.trim_end();
    match trimmed.rfind(char::is_whitespace) {
        Some(i) => i + 1,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> Palette {
        let mut p = Palette::default();
        p.open();
        p
    }

    #[test]
    fn typing_editing_and_submitting_a_line() {
        let mut p = open();
        for c in ["h", "i", " ", "w", "i", "s", "p"] {
            assert_eq!(p.key(Key::Char(c.into())), Outcome::Edited);
        }
        p.key(Key::Backspace);
        assert_eq!(p.text(), "hi wis");
        p.key(Key::DeleteWord);
        assert_eq!(p.text(), "hi ");
        assert_eq!(p.key(Key::Submit), Outcome::Submitted("hi".into()));
        assert!(!p.is_open(), "submit closes it");
        assert_eq!(p.text(), "", "and clears it");
    }

    #[test]
    fn the_caret_respects_utf8_boundaries() {
        let mut p = open();
        p.key(Key::Char("héllo".into()));
        p.key(Key::Home);
        p.key(Key::Right);
        p.key(Key::Right); // now past the é
        p.key(Key::Backspace); // deletes the é, not half of it
        assert_eq!(p.text(), "hllo");
        p.key(Key::End);
        p.key(Key::Backspace);
        assert_eq!(p.text(), "hll");
    }

    #[test]
    fn control_characters_never_enter_the_line() {
        let mut p = open();
        assert_eq!(p.key(Key::Char("\u{7}".into())), Outcome::Ignored);
        p.key(Key::Char("ok\u{0}ok".into()));
        assert_eq!(p.text(), "okok");
    }

    #[test]
    fn an_empty_submit_is_a_dismiss_and_escape_always_is() {
        let mut p = open();
        p.key(Key::Char("   ".into()));
        assert_eq!(p.key(Key::Submit), Outcome::Dismissed);
        let mut p = open();
        p.key(Key::Char("draft".into()));
        assert_eq!(p.key(Key::Dismiss), Outcome::Dismissed);
        assert_eq!(p.text(), "", "a dismissed draft does not survive");
    }

    #[test]
    fn closed_palettes_ignore_keys_and_the_line_is_capped() {
        let mut p = Palette::default();
        assert_eq!(p.key(Key::Char("x".into())), Outcome::Ignored);
        let mut p = open();
        // An insert that would blow the cap is refused wholesale…
        assert_eq!(p.key(Key::Char("a".repeat(MAX_LEN + 10))), Outcome::Ignored);
        assert_eq!(p.text(), "");
        // …and a full line refuses even one more character.
        p.key(Key::Char("a".repeat(MAX_LEN)));
        assert_eq!(p.key(Key::Char("b".into())), Outcome::Ignored, "the cap holds");
    }

    #[test]
    fn the_bar_stays_inside_the_surface_from_every_anchor() {
        let p = open();
        let bounds = Rect::from_size(620.0, 620.0);
        for (x, y) in [(0.0, 0.0), (620.0, 0.0), (0.0, 620.0), (620.0, 620.0), (310.0, 310.0)] {
            let r = p.rect(Point { x, y }, 150.0, bounds);
            assert!(r.x >= 0.0 && r.x + r.w <= 620.0, "x for anchor ({x},{y}): {r:?}");
            assert!(r.y >= 0.0 && r.y + r.h <= 620.0, "y for anchor ({x},{y}): {r:?}");
        }
    }
}
