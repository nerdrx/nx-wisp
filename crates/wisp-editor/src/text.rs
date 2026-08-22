//! The text half of drawing, split so the editor lays out with no GPU.
//!
//! This is `wisp-shell`'s `bubble::TextSink` pattern, and it is here for the
//! same reason it is there: **shaping does not need a device, rasterising
//! does**. A [`Dry`] sink shapes and measures for real — so wrapping, widths
//! and eliding are the real ones — and returns `None` instead of a raster, so
//! no `Cmd::Text` is emitted and everything else in the scene is byte-identical
//! to the live run.
//!
//! That is what lets `tests/panels.rs` assert on the timeline's ruler, the
//! graph's node boxes and the properties list on a machine with no Vulkan, and
//! it is what SPEC §4 asks of a pure module.
//!
//! The panels themselves are built on `wisp_paint::widget::Ui`, whose `layout`
//! already takes a bare `TextEngine` and needs no painter. This trait covers
//! the rest: the parts of the editor that are drawn directly rather than as
//! widgets — the timeline ruler, the graph, the canvas labels.

use wisp_paint::painter::Painter;
use wisp_paint::text::{TextEngine, TextRun};
use wisp_theme::typography::TextStyle;

/// Measure and (optionally) rasterise a string.
pub trait TextSink {
    fn measure(&mut self, text: &str, style: &TextStyle, wrap: Option<f32>) -> (f32, f32);
    /// `None` means "measure only": there is no raster, so the caller emits no
    /// text command.
    fn run(&mut self, text: &str, style: &TextStyle, wrap: Option<f32>) -> Option<TextRun>;
}

/// The real sink: shapes, rasterises and uploads through the painter.
pub struct Live<'a> {
    pub painter: &'a Painter,
    pub engine: &'a mut TextEngine,
}

impl<'a> Live<'a> {
    pub fn new(painter: &'a Painter, engine: &'a mut TextEngine) -> Live<'a> {
        Live { painter, engine }
    }
}

impl TextSink for Live<'_> {
    fn measure(&mut self, text: &str, style: &TextStyle, wrap: Option<f32>) -> (f32, f32) {
        self.engine.measure(text, style, wrap)
    }
    fn run(&mut self, text: &str, style: &TextStyle, wrap: Option<f32>) -> Option<TextRun> {
        Some(self.engine.run(self.painter, text, style, wrap))
    }
}

/// A measure-only sink that records every string it was asked to draw, in
/// order. No device, real shaping.
pub struct Dry<'a> {
    engine: &'a mut TextEngine,
    /// What would have been painted, in paint order.
    pub drawn: Vec<String>,
}

impl<'a> Dry<'a> {
    pub fn new(engine: &'a mut TextEngine) -> Dry<'a> {
        Dry { engine, drawn: Vec::new() }
    }

    /// Did the panel put this string on screen?
    pub fn drew(&self, s: &str) -> bool {
        self.drawn.iter().any(|d| d == s)
    }

    /// Did the panel put a string *containing* this on screen? For labels that
    /// carry a number the test does not want to spell out.
    pub fn drew_containing(&self, s: &str) -> bool {
        self.drawn.iter().any(|d| d.contains(s))
    }

    pub fn clear(&mut self) {
        self.drawn.clear();
    }
}

impl TextSink for Dry<'_> {
    fn measure(&mut self, text: &str, style: &TextStyle, wrap: Option<f32>) -> (f32, f32) {
        self.engine.measure(text, style, wrap)
    }
    fn run(&mut self, text: &str, style: &TextStyle, wrap: Option<f32>) -> Option<TextRun> {
        // Shape it anyway: the caller is about to lay something out against
        // the width, and a dry run that returned a different width would be
        // testing a layout the operator never sees.
        let _ = self.engine.measure(text, style, wrap);
        self.drawn.push(text.to_string());
        None
    }
}
