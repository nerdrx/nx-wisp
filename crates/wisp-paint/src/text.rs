//! Shaping and layout via `cosmic-text`, against the theme's type ramp.
//!
//! A run is shaped once, rasterised to single-channel coverage, uploaded, and
//! cached by its content and style. The colour is **not** part of the key —
//! the coverage texture is tinted at draw time, so a label that changes from
//! `--muted` to `--text` on hover costs nothing.
//!
//! DESIGN.md §7's stack is the system stack, so this asks fontconfig for
//! `sans-serif`/`monospace` rather than shipping a font: "no webfonts" is a
//! rule about identity, not just about bytes.

use std::collections::HashMap;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, Weight, Wrap};
use wisp_theme::typography::{Case, TextStyle};

use crate::geom::Rect;
use crate::painter::Painter;
use crate::texture::Texture;

/// Padding around a raster, so an italic tail or a glyph that overhangs its
/// advance is not clipped.
const PAD: u32 = 2;

/// A shaped, rasterised run of text.
#[derive(Clone, Debug)]
pub struct TextRun {
    pub tex: Texture,
    /// Advance width, for layout. Excludes the raster padding.
    pub width: f32,
    /// Total line height, for layout.
    pub height: f32,
    /// Where the raster sits relative to the layout origin.
    pub offset: (f32, f32),
    /// Raster size in px.
    pub raster: (f32, f32),
    /// Baseline of the first line, from the layout origin.
    pub baseline: f32,
}

impl TextRun {
    /// The quad to hand [`crate::Scene::text`], for a run laid out at `x, y`.
    pub fn rect_at(&self, x: f32, y: f32) -> Rect {
        Rect::new(x + self.offset.0, y + self.offset.1, self.raster.0, self.raster.1)
    }
    /// The logical box the run occupies, for layout and hit-testing.
    pub fn bounds_at(&self, x: f32, y: f32) -> Rect {
        Rect::new(x, y, self.width, self.height)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    text: String,
    size: u32,
    line_height: u32,
    weight: u16,
    tracking: u32,
    mono: bool,
    wrap: u32,
}

/// Shapes, rasterises and caches text. One of these lives in the app; it is
/// not `Sync` because `FontSystem` is not.
pub struct TextEngine {
    fonts: FontSystem,
    swash: SwashCache,
    cache: HashMap<Key, TextRun>,
    /// Above this many cached runs the whole cache is dropped. Boring on
    /// purpose: an LRU here would be more code than it saves for a companion
    /// that says a sentence at a time.
    capacity: usize,
}

impl Default for TextEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TextEngine {
    pub fn new() -> TextEngine {
        TextEngine {
            fonts: FontSystem::new(),
            swash: SwashCache::new(),
            cache: HashMap::new(),
            capacity: 512,
        }
    }

    pub fn cached_runs(&self) -> usize {
        self.cache.len()
    }

    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Measure without rasterising. For layout passes that may not draw.
    pub fn measure(&mut self, text: &str, style: &TextStyle, wrap: Option<f32>) -> (f32, f32) {
        let text = apply_case(text, style.case);
        let mut buffer = self.shape(&text, style, wrap);
        let m = measure(&mut buffer);
        (m.0, m.1)
    }

    /// Shape, rasterise and cache. Returns a coverage texture plus metrics.
    pub fn run(
        &mut self,
        painter: &Painter,
        text: &str,
        style: &TextStyle,
        wrap: Option<f32>,
    ) -> TextRun {
        let text = apply_case(text, style.case);
        let key = Key {
            text: text.clone(),
            size: style.size_px.to_bits(),
            line_height: style.line_height_px().to_bits(),
            weight: style.weight,
            tracking: style.tracking_em.to_bits(),
            mono: matches!(style.family, wisp_theme::typography::Family::Mono),
            wrap: wrap.unwrap_or(f32::INFINITY).to_bits(),
        };
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        if self.cache.len() >= self.capacity {
            self.cache.clear();
        }

        let mut buffer = self.shape(&text, style, wrap);
        let (w, h, baseline) = measure(&mut buffer);

        let rw = w.ceil().max(1.0) as u32 + PAD * 2;
        let rh = h.ceil().max(1.0) as u32 + PAD * 2;
        let mut cov = vec![0u8; (rw * rh) as usize];

        // The callback hands back the glyph colour with coverage in its alpha;
        // we keep the coverage and throw the colour away, because tinting
        // happens on the GPU.
        buffer.draw(
            &mut self.fonts,
            &mut self.swash,
            cosmic_text::Color::rgba(255, 255, 255, 255),
            |gx, gy, gw, gh, colour| {
                let a = colour.a();
                if a == 0 {
                    return;
                }
                for dy in 0..gh as i32 {
                    let y = gy + dy + PAD as i32;
                    if y < 0 || y >= rh as i32 {
                        continue;
                    }
                    for dx in 0..gw as i32 {
                        let x = gx + dx + PAD as i32;
                        if x < 0 || x >= rw as i32 {
                            continue;
                        }
                        let i = (y as u32 * rw + x as u32) as usize;
                        cov[i] = cov[i].max(a);
                    }
                }
            },
        );

        let run = TextRun {
            tex: painter.upload_coverage(rw, rh, &cov),
            width: w,
            height: h,
            offset: (-(PAD as f32), -(PAD as f32)),
            raster: (rw as f32, rh as f32),
            baseline,
        };
        self.cache.insert(key, run.clone());
        run
    }

    fn shape(&mut self, text: &str, style: &TextStyle, wrap: Option<f32>) -> Buffer {
        let metrics = Metrics::new(style.size_px, style.line_height_px());
        let mut buffer = Buffer::new(&mut self.fonts, metrics);
        buffer.set_wrap(if wrap.is_some() { Wrap::WordOrGlyph } else { Wrap::None });
        buffer.set_size(wrap, None);
        let family = match style.family {
            wisp_theme::typography::Family::Sans => Family::SansSerif,
            wisp_theme::typography::Family::Mono => Family::Monospace,
        };
        let mut attrs = Attrs::new().family(family).weight(Weight(style.weight));
        if style.tracking_em.abs() > f32::EPSILON {
            attrs = attrs.letter_spacing(style.tracking_em);
        }
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        buffer
    }
}

fn apply_case(text: &str, case: Case) -> String {
    match case {
        Case::AsWritten => text.to_string(),
        // §5's micro-labels. Uppercasing at shaping time rather than in the
        // source string keeps the copy sentence-case where §9 wants it.
        Case::Upper => text.to_uppercase(),
    }
}

/// (width, height, first baseline)
fn measure(buffer: &mut Buffer) -> (f32, f32, f32) {
    let mut w: f32 = 0.0;
    let mut h: f32 = 0.0;
    let mut baseline = 0.0;
    let mut first = true;
    for run in buffer.layout_runs() {
        w = w.max(run.line_w);
        h = h.max(run.line_top + run.line_height);
        if first {
            baseline = run.line_y;
            first = false;
        }
    }
    (w, h, baseline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_theme::typography;

    #[test]
    fn uppercase_only_happens_for_micro_labels() {
        assert_eq!(apply_case("Update available", Case::AsWritten), "Update available");
        assert_eq!(apply_case("live", Case::Upper), "LIVE");
    }

    #[test]
    fn measuring_scales_with_the_ramp() {
        let mut e = TextEngine::new();
        let small = e.measure("Wisp", &typography::SECONDARY, None);
        let big = e.measure("Wisp", &typography::TITLE, None);
        assert!(big.0 > small.0, "22px must be wider than 13px: {big:?} vs {small:?}");
        assert!(big.1 > small.1);
    }

    #[test]
    fn empty_text_measures_to_nothing_without_panicking() {
        let mut e = TextEngine::new();
        let (w, _h) = e.measure("", &typography::BODY, None);
        assert_eq!(w, 0.0);
    }

    #[test]
    fn wrapping_makes_a_long_line_taller_and_narrower() {
        let mut e = TextEngine::new();
        let long = "She costs nothing when it matters, and she is honest about what she saw.";
        let free = e.measure(long, &typography::BODY, None);
        let wrapped = e.measure(long, &typography::BODY, Some(120.0));
        assert!(wrapped.0 <= 120.0 + 1.0, "wrapped width was {}", wrapped.0);
        assert!(wrapped.1 > free.1, "wrapping must add lines");
    }

    #[test]
    fn the_cache_key_ignores_colour_but_not_size() {
        let a = Key {
            text: "x".into(),
            size: 14f32.to_bits(),
            line_height: 20f32.to_bits(),
            weight: 400,
            tracking: 0f32.to_bits(),
            mono: false,
            wrap: f32::INFINITY.to_bits(),
        };
        let mut b = a.clone();
        assert_eq!(a, b);
        b.size = 22f32.to_bits();
        assert_ne!(a, b);
    }
}
