//! The speech bubble: what she says, where it goes, and how long it stays.
//!
//! This module is **pure**. It owns no clock, no GPU state and no timers: it
//! turns `(text, urgency, where she is, how big the surface is, tier, reveal)`
//! into a flat list of [`Scene`] commands, exactly like `wisp-attn` turns a
//! budget into a verdict without ever asking what time it is. The host drives
//! `reveal` and `shown_ms`; [`Lifetime`] tells it what the numbers should be.
//!
//! Three decisions are worth reading before changing anything here.
//!
//! ## 1. The bubble spends **no** `BlurBackdrop`
//!
//! DESIGN.md §4 makes a real backdrop blur a budget rather than a default, and
//! v1.5 raises a floating layer's body to ≥0.85 alpha. The bubble's body is
//! `glass::SHEET_BODY`, whose minimum alpha is **0.93** — seven percent of the
//! backdrop survives it, and blurring seven percent is below the perceptual
//! floor while still costing a full-target texture copy plus two Gaussian
//! passes *per frame the bubble is up*.
//!
//! On a layer-shell surface the argument is stronger than "not worth it".
//! `Cmd::BlurBackdrop` blurs the painter's own scratch texture — the pixels
//! this surface has drawn so far. The desktop underneath the Wayland surface is
//! **not** in that texture and never can be. So a blur here would frost
//! transparent black: it would produce no visible change at all and charge full
//! price for it. §0.1 says she costs nothing when it matters, so she does not
//! buy that.
//!
//! Legibility therefore comes from the fill, which is what §4 asks for anyway:
//! *"menus and toasts carry legibility in their fill alpha (≥0.9); blur is
//! finish, not the mechanism."*
//!
//! ## 2. Placement is a pure function of (anchor, size, bounds)
//!
//! [`place`] picks a [`Side`] and a rect, and it is total: for any anchor and
//! any bounds large enough to hold a one-line bubble, the returned rect is
//! inside `bounds`. Height is bounded by truncating the wrap to the number of
//! lines that fit, so "the text was long" can never push her off the output.
//!
//! ## 3. T3 is flat, T4 is nothing
//!
//! At `Tier::Lobotomised` the bubble draws with no blur, no gradients and no
//! specular — flat solid fills and simple triangles only, which is exactly the
//! shape `wisp_paint::atlas::bake` can take. At `Tier::Dormant` it draws
//! nothing at all. See the note on `Painter::draw_mode` in the T3 test.

use wisp_paint::geom::{Path, Point, Rect};
use wisp_paint::paint::{Paint, Stop};
use wisp_paint::scene::Scene;
use wisp_paint::text::{TextEngine, TextRun};
use wisp_paint::Painter;
use wisp_proto::{Tier, Urgency};
use wisp_theme::surface::Floating;
use wisp_theme::typography::{Role, TextStyle};
use wisp_theme::{palette, Color, Insets, Radius, Space};

// ----------------------------------------------------------------- constants

/// The widest a bubble ever gets. 320px of 14px body is roughly 55 characters
/// a line, which is inside the comfortable measure and lands on the 8px grid.
pub const MAX_WIDTH_PX: f32 = 320.0;
/// The narrowest, so "yes" does not become a sliver.
pub const MIN_WIDTH_PX: f32 = 128.0;
/// §7's default padding: 16 inside.
pub const PADDING: Insets = Insets::CONTENT;
/// Clearance kept between the bubble and the edge of the surface.
pub const MARGIN_PX: f32 = Space::S1.get();
/// Clearance between the tail's point and her silhouette.
pub const GAP_PX: f32 = Space::S1.get();
/// How wide the tail is where it meets the body.
pub const TAIL_BASE_PX: f32 = Space::S2.get();
/// How far the tail reaches out of the body. A tail is cut geometry, like a
/// radius — §7's 8px grid governs rhythm, not the shape of a chamfer.
pub const TAIL_LEN_PX: f32 = 12.0;
/// She never says more than this many lines at once; the rest is elided.
pub const MAX_LINES: usize = 8;

/// How far the tail's base is pushed back into the body, so its fill covers the
/// body's lit edge where the two meet.
const TAIL_OVERLAP_PX: f32 = 2.0;
/// Widest angle the tail may lean away from its edge normal, as `dt/dn`.
/// `0.70` is a touch under 35°: enough to visibly aim at her when she is off to
/// one side, not enough to look broken.
const TAIL_MAX_LEAN: f32 = 0.70;
/// The urgency rule down the bubble's leading edge.
const RULE_W_PX: f32 = 3.0;
/// Vertical space the alarm wedge takes above the first line.
const WARN_ROW_PX: f32 = Space::S3.get();
const WARN_BOX_PX: f32 = 16.0;
const MIN_CONTENT_PX: f32 = 32.0;

// -------------------------------------------------------------------- text

/// The text half of drawing, factored out so a bubble can be laid out and its
/// command list asserted on with **no GPU** — SPEC §4 wants pure modules unit
/// tested without one, and shaping does not need a device even though
/// rasterising does.
pub trait TextSink {
    fn measure(&mut self, text: &str, style: &TextStyle, wrap: Option<f32>) -> (f32, f32);
    /// `None` means "measure only": no raster exists, so no `Cmd::Text` is
    /// emitted and everything else in the scene is unchanged.
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

/// A measure-only sink that records every string it was asked to *draw*.
/// Shaping is real, so wrapping and widths are the real ones; nothing is
/// rasterised, so no device is needed. This is how the typewriter is tested.
pub struct Dry<'a> {
    engine: &'a mut TextEngine,
    /// The revealed strings, in the order they would have been painted.
    pub drawn: Vec<String>,
}

impl<'a> Dry<'a> {
    pub fn new(engine: &'a mut TextEngine) -> Dry<'a> {
        Dry { engine, drawn: Vec::new() }
    }
    /// Total characters that would actually have appeared on screen.
    pub fn drawn_chars(&self) -> usize {
        self.drawn.iter().map(|s| s.chars().count()).sum()
    }
}

impl TextSink for Dry<'_> {
    fn measure(&mut self, text: &str, style: &TextStyle, wrap: Option<f32>) -> (f32, f32) {
        self.engine.measure(text, style, wrap)
    }
    fn run(&mut self, text: &str, _style: &TextStyle, _wrap: Option<f32>) -> Option<TextRun> {
        self.drawn.push(text.to_string());
        None
    }
}

// ------------------------------------------------------------------ urgency

/// What an [`Urgency`] looks like *before you have read a word of it*.
///
/// DESIGN.md §1 keeps the signal colours in their lanes: amber means attention
/// and nothing else, `#ff5470` means danger and nothing else. So urgency is
/// carried on the edge and on a marker, never by recolouring the body — a
/// bubble is always the same violet glass, and the accent tells you how much it
/// wants you.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Signal {
    /// The accent rule down the leading edge. `None` for chatter.
    pub rule: Option<Color>,
    /// Which step of §7's ramp the words take.
    pub role: Role,
    /// Repaint the whole lit edge in this colour. Alarms only.
    pub edge: Option<Color>,
    /// Draw the warning wedge above the first line. Alarms only.
    pub wedge: bool,
}

/// The urgency ladder, as a look.
pub const fn signal(urgency: Urgency) -> Signal {
    match urgency {
        // Chatter. No accent at all, and quieter type — she is thinking out
        // loud, not addressing you.
        Urgency::Whim => {
            Signal { rule: None, role: Role::Secondary, edge: None, wedge: false }
        }
        // Something she noticed. Amber is §1's attention colour.
        Urgency::Notable => {
            Signal { rule: Some(palette::AMBER), role: Role::Body, edge: None, wedge: false }
        }
        // You asked. Violet is identity and focus, not attention: you are
        // already looking.
        Urgency::Answer => {
            Signal { rule: Some(palette::VIOLET_SOFT), role: Role::Body, edge: None, wedge: false }
        }
        // Waiting would be worse than interrupting.
        Urgency::Alarm => Signal {
            rule: Some(palette::DANGER),
            role: Role::BodyStrong,
            edge: Some(palette::DANGER),
            wedge: true,
        },
    }
}

// ----------------------------------------------------------------- lifetime

/// How long a bubble of a given urgency lives, as data. No clock: the host
/// keeps `shown_ms` and asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lifetime {
    /// Typewriter speed, milliseconds per character.
    pub per_char_ms: u32,
    /// Held after the last character lands, before the fade begins.
    pub hold_ms: u32,
    /// The fade out. §6 caps anything interactive at 320ms.
    pub fade_ms: u32,
    /// Ceiling on the whole life, however long the text is.
    pub max_ms: u32,
    /// Never dismissed by the clock — the operator has to see it.
    pub sticky: bool,
}

impl Lifetime {
    /// When the last character has landed.
    pub fn reveal_ms(&self, chars: usize) -> u32 {
        self.per_char_ms.saturating_mul(chars as u32)
    }
    /// The whole life, fade included.
    pub fn total_ms(&self, chars: usize) -> u32 {
        self.reveal_ms(chars)
            .saturating_add(self.hold_ms)
            .saturating_add(self.fade_ms)
            .min(self.max_ms)
    }
    /// Typewriter progress at `shown_ms`, for [`Layout::paint`].
    pub fn reveal_at(&self, chars: usize, shown_ms: u64) -> f32 {
        let span = self.reveal_ms(chars);
        if span == 0 {
            return 1.0;
        }
        (shown_ms as f32 / span as f32).clamp(0.0, 1.0)
    }
    /// Whole-bubble opacity at `shown_ms`, so the host can fade it out without
    /// this module knowing what a frame is.
    pub fn opacity_at(&self, chars: usize, shown_ms: u64) -> f32 {
        if self.sticky || self.fade_ms == 0 {
            return 1.0;
        }
        let total = self.total_ms(chars) as u64;
        let fade = self.fade_ms as u64;
        if shown_ms >= total {
            return 0.0;
        }
        let start = total.saturating_sub(fade);
        if shown_ms <= start {
            return 1.0;
        }
        1.0 - (shown_ms - start) as f32 / fade as f32
    }
    /// Is it done? A sticky bubble never is.
    pub fn should_dismiss(&self, chars: usize, shown_ms: u64) -> bool {
        !self.sticky && shown_ms >= self.total_ms(chars) as u64
    }
}

/// The lifetime table. `Alarm` is sticky: SPEC §0.3's sibling rule — something
/// that is wrong stays on screen until the operator has actually seen it.
pub const fn lifetime(urgency: Urgency) -> Lifetime {
    match urgency {
        Urgency::Whim => Lifetime {
            per_char_ms: 22,
            hold_ms: 1_800,
            fade_ms: 240,
            max_ms: 8_000,
            sticky: false,
        },
        Urgency::Notable => Lifetime {
            per_char_ms: 22,
            hold_ms: 3_000,
            fade_ms: 240,
            max_ms: 12_000,
            sticky: false,
        },
        Urgency::Answer => Lifetime {
            per_char_ms: 18,
            hold_ms: 5_000,
            fade_ms: 240,
            max_ms: 30_000,
            sticky: false,
        },
        Urgency::Alarm => Lifetime {
            per_char_ms: 14,
            hold_ms: 8_000,
            fade_ms: 320,
            max_ms: 45_000,
            sticky: true,
        },
    }
}

// ---------------------------------------------------------------- placement

/// Which side of *her* the bubble sits on. The tail is always on the bubble
/// edge facing her, so this also names the tail's edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Above,
    Below,
    Right,
    Left,
}

impl Side {
    /// Tried in this order. Above first, because a bubble over her head is the
    /// one arrangement that never covers what she is pointing at.
    pub const PREFERENCE: [Side; 4] = [Side::Above, Side::Below, Side::Right, Side::Left];

    /// `(normal pointing from the bubble toward her, tangent along the edge)`.
    pub fn axes(self) -> ((f32, f32), (f32, f32)) {
        match self {
            Side::Above => ((0.0, 1.0), (1.0, 0.0)),
            Side::Below => ((0.0, -1.0), (1.0, 0.0)),
            Side::Right => ((-1.0, 0.0), (0.0, 1.0)),
            Side::Left => ((1.0, 0.0), (0.0, 1.0)),
        }
    }
}

/// The tail, as three points. `base_a`/`base_b` are pushed slightly *into* the
/// body so the fill covers the lit edge where they cross it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tail {
    pub base_a: Point,
    pub base_b: Point,
    pub apex: Point,
    /// Where the tail leaves the body, on the boundary itself.
    pub root: Point,
}

/// Her silhouette, as a box: `size` square, centred on `anchor`.
pub fn her_box(anchor: Point, size: f32) -> Rect {
    let s = size.max(0.0);
    Rect::new(anchor.x - s * 0.5, anchor.y - s * 0.5, s, s)
}

/// Total clearance the tail needs between the body and her silhouette.
fn stand() -> f32 {
    GAP_PX + TAIL_LEN_PX
}

fn clamp_span(v: f32, len: f32, lo: f32, hi: f32) -> f32 {
    if len >= hi - lo {
        lo
    } else {
        v.clamp(lo, hi - len)
    }
}

/// **The flip.** Pure function of `(anchor, her size, bubble size, bounds)`.
///
/// Picks the first side in [`Side::PREFERENCE`] where the body genuinely fits
/// between her and the edge of the surface; if none does, the side with the
/// most room. The cross axis is then centred on her and clamped, so a bubble
/// beside a corner slides along rather than hanging off. The returned rect is
/// inside `bounds` whenever `bounds` can hold it at all.
pub fn place(anchor: Point, her_size: f32, size: (f32, f32), bounds: Rect) -> (Rect, Side) {
    let (w, h) = size;
    let her = her_box(anchor, her_size);
    let stand = stand();
    // The body keeps a margin off the surface edge; the tail may enter it.
    let inner = bounds.inset(MARGIN_PX);

    let room = |s: Side| match s {
        Side::Above => (her.y - stand) - inner.y,
        Side::Below => inner.bottom() - (her.bottom() + stand),
        Side::Right => inner.right() - (her.right() + stand),
        Side::Left => (her.x - stand) - inner.x,
    };
    let need = |s: Side| match s {
        Side::Above | Side::Below => h,
        Side::Right | Side::Left => w,
    };

    let mut side = Side::PREFERENCE[0];
    let mut best = f32::NEG_INFINITY;
    for s in Side::PREFERENCE {
        let slack = room(s) - need(s);
        if slack >= 0.0 {
            // The first side she genuinely fits on wins; ties go to the
            // earlier preference, which is what makes this deterministic.
            side = s;
            break;
        }
        if slack > best {
            best = slack;
            side = s;
        }
    }

    // Main axis: hard against her, one tail's length away.
    let (mut x, mut y) = match side {
        Side::Above => (0.0, her.y - stand - h),
        Side::Below => (0.0, her.bottom() + stand),
        Side::Right => (her.right() + stand, 0.0),
        Side::Left => (her.x - stand - w, 0.0),
    };
    // Cross axis: centred on her, then slid back inside.
    match side {
        Side::Above | Side::Below => x = anchor.x - w * 0.5,
        Side::Right | Side::Left => y = anchor.y - h * 0.5,
    }

    // Clamp both axes. Prefer the margin-inset box; fall back to the raw
    // surface when the bubble is larger than the inset box on that axis.
    let (lox, hix) = if w <= inner.w { (inner.x, inner.right()) } else { (bounds.x, bounds.right()) };
    let (loy, hiy) = if h <= inner.h { (inner.y, inner.bottom()) } else { (bounds.y, bounds.bottom()) };
    x = clamp_span(x, w, lox, hix);
    y = clamp_span(y, h, loy, hiy);

    (Rect::new(x, y, w, h), side)
}

/// The tail for a placed bubble. The apex is aimed at `anchor`, leaning up to
/// [`TAIL_MAX_LEAN`] off the edge normal so it still visibly points at her when
/// the body has been slid along the edge to stay on screen.
pub fn tail(rect: Rect, side: Side, anchor: Point, radius: Radius) -> Tail {
    let (n, t) = side.axes();
    let r = radius.px();
    let half = TAIL_BASE_PX * 0.5;

    // Where on the edge the base sits: as close to her as the corners allow.
    let (root, along_lo, along_hi, along_v) = match side {
        Side::Above => (rect.bottom(), rect.x + r + half, rect.right() - r - half, anchor.x),
        Side::Below => (rect.y, rect.x + r + half, rect.right() - r - half, anchor.x),
        Side::Right => (rect.x, rect.y + r + half, rect.bottom() - r - half, anchor.y),
        Side::Left => (rect.right(), rect.y + r + half, rect.bottom() - r - half, anchor.y),
    };
    let along = if along_lo > along_hi {
        (along_lo + along_hi) * 0.5
    } else {
        along_v.clamp(along_lo, along_hi)
    };
    let base_c = match side {
        Side::Above | Side::Below => Point::new(along, root),
        Side::Right | Side::Left => Point::new(root, along),
    };

    // Aim at her, then limit the lean.
    let (mut dx, mut dy) = (anchor.x - base_c.x, anchor.y - base_c.y);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-4 {
        dx = n.0;
        dy = n.1;
    } else {
        dx /= len;
        dy /= len;
    }
    let mut dn = dx * n.0 + dy * n.1;
    let mut dt = dx * t.0 + dy * t.1;
    if dn <= 1e-3 {
        // She is somehow behind the bubble. Point straight out rather than
        // folding the tail back through the body.
        dn = 1.0;
        dt = 0.0;
    }
    dt = dt.clamp(-TAIL_MAX_LEAN * dn, TAIL_MAX_LEAN * dn);
    let m = (dn * dn + dt * dt).sqrt().max(1e-6);
    dn /= m;
    dt /= m;
    let dir = (n.0 * dn + t.0 * dt, n.1 * dn + t.1 * dt);

    let back = Point::new(base_c.x - n.0 * TAIL_OVERLAP_PX, base_c.y - n.1 * TAIL_OVERLAP_PX);
    Tail {
        base_a: Point::new(back.x + t.0 * half, back.y + t.1 * half),
        base_b: Point::new(back.x - t.0 * half, back.y - t.1 * half),
        apex: Point::new(base_c.x + dir.0 * TAIL_LEN_PX, base_c.y + dir.1 * TAIL_LEN_PX),
        root: base_c,
    }
}

// ------------------------------------------------------------------- layout

/// Widest the *content* may be, given the surface.
pub fn max_content_width(bounds: Rect) -> f32 {
    let by_bounds = bounds.w - 2.0 * MARGIN_PX - PADDING.horizontal();
    (MAX_WIDTH_PX - PADDING.horizontal()).min(by_bounds).max(MIN_CONTENT_PX)
}

/// A wrapped, placed bubble. Build it once when the utterance starts and paint
/// it every frame with a different `reveal` — the wrap is deliberately computed
/// from the **whole** text so the lines never reflow as characters appear.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub urgency: Urgency,
    /// The wrapped lines of the full text, elided if they did not all fit.
    pub lines: Vec<String>,
    /// Characters across all lines — what `reveal` is a fraction of.
    pub chars: usize,
    /// The body, excluding the tail.
    pub rect: Rect,
    pub side: Side,
    pub tail: Tail,
    /// Where the first line's origin sits.
    pub text_origin: Point,
    pub line_h: f32,
}

impl Layout {
    /// Wrap, size and place. `anchor` is her centre and `her_size` her drawn
    /// size, both in surface pixels.
    pub fn new(
        text: &str,
        urgency: Urgency,
        anchor: Point,
        her_size: f32,
        bounds: Rect,
        sink: &mut impl TextSink,
    ) -> Layout {
        let sg = signal(urgency);
        let style = sg.role.style();
        let line_h = style.line_height_px().ceil();
        let warn_row = if sg.wedge { WARN_ROW_PX } else { 0.0 };

        let content_max = max_content_width(bounds);
        let vertical = bounds.h - 2.0 * MARGIN_PX - PADDING.vertical() - warn_row;
        let fits = ((vertical / line_h).floor().max(1.0)) as usize;
        let max_lines = fits.min(MAX_LINES);

        let lines = wrap(text, content_max, &style, sink, max_lines);
        let chars = lines.iter().map(|l| l.chars().count()).sum();

        let widest = lines
            .iter()
            .map(|l| sink.measure(l, &style, None).0)
            .fold(0.0f32, f32::max);
        let content_w = widest.clamp(MIN_WIDTH_PX - PADDING.horizontal(), content_max);
        let w = (content_w + PADDING.horizontal()).min(bounds.w);
        let h = lines.len() as f32 * line_h + warn_row + PADDING.vertical();

        let (rect, side) = place(anchor, her_size, (w, h), bounds);
        let tail = tail(rect, side, anchor, Floating::Bubble.material().radius);
        let inner = rect.inset_by(PADDING);
        Layout {
            urgency,
            lines,
            chars,
            rect,
            side,
            tail,
            text_origin: Point::new(inner.x, inner.y + warn_row),
            line_h,
        }
    }

    /// Everything the bubble covers, tail included — what the shell wants for
    /// its damage rect and its input region.
    pub fn bounds(&self) -> Rect {
        let t = self.tail;
        let xs = [t.base_a.x, t.base_b.x, t.apex.x];
        let ys = [t.base_a.y, t.base_b.y, t.apex.y];
        let x0 = xs.iter().copied().fold(self.rect.x, f32::min);
        let y0 = ys.iter().copied().fold(self.rect.y, f32::min);
        let x1 = xs.iter().copied().fold(self.rect.right(), f32::max);
        let y1 = ys.iter().copied().fold(self.rect.bottom(), f32::max);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn lifetime(&self) -> Lifetime {
        lifetime(self.urgency)
    }
    /// The dismiss question, asked with a clock the caller owns.
    pub fn should_dismiss(&self, shown_ms: u64) -> bool {
        self.lifetime().should_dismiss(self.chars, shown_ms)
    }
    pub fn reveal_at(&self, shown_ms: u64) -> f32 {
        self.lifetime().reveal_at(self.chars, shown_ms)
    }
    pub fn opacity_at(&self, shown_ms: u64) -> f32 {
        self.lifetime().opacity_at(self.chars, shown_ms)
    }

    /// Emit the bubble. `reveal` is 0..1 typewriter progress; `tier` decides
    /// how much of the material survives.
    pub fn paint(
        &self,
        tier: Tier,
        reveal: f32,
        sink: &mut impl TextSink,
        scene: &mut Scene,
    ) {
        if tier >= Tier::Dormant {
            return;
        }
        // T3 is "sprite quads only, no compute passes": no blur, no gradient,
        // no specular. What is left is exactly what the atlas baker can take.
        let flat = tier >= Tier::Lobotomised;
        let sg = signal(self.urgency);
        let m = Floating::Bubble.material();
        let r = m.radius;
        let rect = self.rect;

        // 1. Body. **No `BlurBackdrop`** — see the module docs.
        if flat {
            scene.fill_rect(rect, r, Paint::solid(m.body.sample(0.5)));
        } else {
            scene.fill_rect(rect, r, Paint::token(m.body));
            scene.fill_rect(rect, r, Paint::token(m.highlight));
        }

        // 2. The urgency rule, down the leading edge inside the corners.
        if let Some(c) = sg.rule {
            let bar = Rect::new(rect.x, rect.y + r.px(), RULE_W_PX, (rect.h - r.px() * 2.0).max(0.0));
            let paint = if flat {
                Paint::solid(c)
            } else {
                Paint::Linear {
                    angle_deg: 180.0,
                    stops: vec![
                        Stop { at: 0.0, color: c },
                        Stop { at: 1.0, color: c.with_alpha(c.alpha_f() * 0.35) },
                    ],
                }
            };
            scene.fill(Path::rect(bar), paint);
        }

        // 3. The lit edge, repainted in the signal colour for an alarm.
        match sg.edge {
            Some(c) => {
                scene.stroke(Path::rounded_rect(rect.inset(0.5), r), Paint::solid(c), 1.0);
            }
            None if flat => {
                if let Some(g) = m.edge.gradient() {
                    scene.stroke(
                        Path::rounded_rect(rect.inset(0.5), r),
                        Paint::solid(g.sample(0.25)),
                        1.0,
                    );
                }
            }
            None => {
                scene.lit_edge(rect, r, m.edge);
            }
        }

        // 4. The tail, over the edge it crosses, then its own two outer sides.
        let t = self.tail;
        let tail_t = match self.side {
            Side::Above => 1.0,
            Side::Below => 0.0,
            Side::Right | Side::Left => 0.5,
        };
        let body_here = m.body.sample(tail_t);
        let tri = Path::build(|p| {
            p.move_to(t.base_a.x, t.base_a.y)
                .line_to(t.apex.x, t.apex.y)
                .line_to(t.base_b.x, t.base_b.y)
                .close();
        });
        scene.fill(tri, Paint::solid(body_here));
        let outline = sg.edge.unwrap_or_else(|| {
            m.edge.gradient().map(|g| g.sample(0.25)).unwrap_or(Color::TRANSPARENT)
        });
        let sides = Path::build(|p| {
            p.move_to(t.base_a.x, t.base_a.y)
                .line_to(t.apex.x, t.apex.y)
                .line_to(t.base_b.x, t.base_b.y);
        });
        scene.stroke(sides, Paint::solid(outline), 1.0);

        // 5. The alarm wedge. Shape as well as colour, so it is legible to an
        //    operator who cannot tell amber from red.
        if sg.wedge {
            let x = self.text_origin.x;
            let y = self.text_origin.y - WARN_ROW_PX;
            let b = WARN_BOX_PX;
            let wedge = Path::build(|p| {
                p.move_to(x + b * 0.5, y)
                    .line_to(x + b, y + b * 0.86)
                    .line_to(x, y + b * 0.86)
                    .close();
            });
            scene.fill(wedge, Paint::solid(palette::DANGER));
        }

        // 6. The words, as far as they have arrived.
        let style = sg.role.style();
        let mut left = visible_chars(self.chars, reveal);
        for (i, line) in self.lines.iter().enumerate() {
            if left == 0 {
                break;
            }
            let n = line.chars().count();
            let slice = if left >= n { line.as_str() } else { prefix(line, left) };
            left = left.saturating_sub(n);
            if slice.is_empty() {
                continue;
            }
            let y = self.text_origin.y + i as f32 * self.line_h;
            if let Some(run) = sink.run(slice, &style, None) {
                scene.text(run.rect_at(self.text_origin.x, y), run.tex.clone(), style.color);
            }
        }
    }
}

/// Lay out and paint in one call — the signature SPEC's caller wants when it
/// is not caching a [`Layout`] between frames.
#[allow(clippy::too_many_arguments)]
pub fn build(
    text: &str,
    urgency: Urgency,
    anchor: Point,
    her_size: f32,
    bounds: Rect,
    tier: Tier,
    reveal: f32,
    sink: &mut impl TextSink,
    scene: &mut Scene,
) -> Layout {
    let layout = Layout::new(text, urgency, anchor, her_size, bounds, sink);
    layout.paint(tier, reveal, sink, scene);
    layout
}

// ------------------------------------------------------------------ helpers

/// How many characters of the whole utterance are on screen at `reveal`.
pub fn visible_chars(total: usize, reveal: f32) -> usize {
    if !reveal.is_finite() {
        return total;
    }
    let n = (total as f32 * reveal.clamp(0.0, 1.0)).round() as usize;
    n.min(total)
}

fn prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Greedy word wrap against real shaped widths.
///
/// The wrap is over the *full* text and the result is fixed for the life of the
/// bubble, which is what stops the typewriter from reflowing a line every time
/// a character lands.
fn wrap(
    text: &str,
    max_w: f32,
    style: &TextStyle,
    sink: &mut impl TextSink,
    max_lines: usize,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if line.is_empty() || sink.measure(&candidate, style, None).0 <= max_w {
                line = candidate;
                // A single word too long for the measure is broken by hand;
                // cosmic-text would do it at draw time and we would not know
                // where, which would break the reveal.
                while sink.measure(&line, style, None).0 > max_w && line.chars().count() > 1 {
                    let keep = fit_chars(&line, max_w, style, sink);
                    let cut = byte_at(&line, keep);
                    let rest = line.split_off(cut);
                    out.push(std::mem::take(&mut line));
                    line = rest;
                }
            } else {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    if out.is_empty() {
        return out;
    }
    if out.len() > max_lines {
        out.truncate(max_lines.max(1));
        elide(out.last_mut().expect("non-empty"), max_w, style, sink);
    }
    out
}

/// The most characters of `s` that fit in `max_w`. At least one.
fn fit_chars(s: &str, max_w: f32, style: &TextStyle, sink: &mut impl TextSink) -> usize {
    let total = s.chars().count();
    let mut best = 1;
    for n in 1..=total {
        let end = byte_at(s, n);
        if sink.measure(&s[..end], style, None).0 <= max_w {
            best = n;
        } else {
            break;
        }
    }
    best
}

fn byte_at(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

fn elide(line: &mut String, max_w: f32, style: &TextStyle, sink: &mut impl TextSink) {
    line.push('…');
    while sink.measure(line, style, None).0 > max_w && line.chars().count() > 1 {
        line.pop();
        line.pop();
        line.push('…');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_paint::scene::Cmd;

    const URGENCIES: [Urgency; 4] =
        [Urgency::Whim, Urgency::Notable, Urgency::Answer, Urgency::Alarm];

    const LONG: &str = "The GPU has been over ninety percent for six minutes \
and the deliberate model is still resident, so I am dropping it before the \
compositor starts missing frames.";

    fn engine() -> TextEngine {
        TextEngine::new()
    }

    fn hd() -> Rect {
        Rect::from_size(1920.0, 1080.0)
    }

    /// Every anchor a corner or an edge can produce, for a 1920×1080 output.
    fn anchors(b: Rect, her: f32) -> Vec<(&'static str, Point)> {
        let m = her * 0.5 + 4.0;
        vec![
            ("top-left", Point::new(b.x + m, b.y + m)),
            ("top-right", Point::new(b.right() - m, b.y + m)),
            ("bottom-left", Point::new(b.x + m, b.bottom() - m)),
            ("bottom-right", Point::new(b.right() - m, b.bottom() - m)),
            ("top-edge", Point::new(b.centre().x, b.y + m)),
            ("bottom-edge", Point::new(b.centre().x, b.bottom() - m)),
            ("left-edge", Point::new(b.x + m, b.centre().y)),
            ("right-edge", Point::new(b.right() - m, b.centre().y)),
            ("centre", b.centre()),
        ]
    }

    fn contains(outer: Rect, inner: Rect) -> bool {
        inner.x >= outer.x - 0.01
            && inner.y >= outer.y - 0.01
            && inner.right() <= outer.right() + 0.01
            && inner.bottom() <= outer.bottom() + 0.01
    }

    // --------------------------------------------------------------- placing

    #[test]
    fn the_bubble_never_leaves_the_surface_from_any_corner_or_edge() {
        let mut e = engine();
        let bounds = hd();
        for her in [64.0f32, 160.0, 320.0] {
            for (name, a) in anchors(bounds, her) {
                for u in URGENCIES {
                    let mut dry = Dry::new(&mut e);
                    let l = Layout::new(LONG, u, a, her, bounds, &mut dry);
                    assert!(
                        contains(bounds, l.rect),
                        "{name} at her={her} {u:?}: body {:?} escaped {bounds:?}",
                        l.rect
                    );
                    assert!(
                        contains(bounds, l.bounds()),
                        "{name} at her={her} {u:?}: tail {:?} escaped {bounds:?}",
                        l.bounds()
                    );
                }
            }
        }
    }

    #[test]
    fn the_tail_still_points_at_her_from_every_corner() {
        let mut e = engine();
        let bounds = hd();
        let her = 160.0;
        for (name, a) in anchors(bounds, her) {
            let mut dry = Dry::new(&mut e);
            let l = Layout::new(LONG, Urgency::Answer, a, her, bounds, &mut dry);
            let t = l.tail;
            let out = (t.apex.x - t.root.x, t.apex.y - t.root.y);
            let her_dir = (a.x - t.root.x, a.y - t.root.y);
            let dot = out.0 * her_dir.0 + out.1 * her_dir.1;
            assert!(dot > 0.0, "{name}: the tail points away from her (dot {dot})");

            // …and it leaves the body on the side she is on.
            let (n, _) = l.side.axes();
            let along = out.0 * n.0 + out.1 * n.1;
            assert!(along > 0.0, "{name}: the tail folded back into the body");
            // The root is on the boundary of the body.
            let on_edge = match l.side {
                Side::Above => (t.root.y - l.rect.bottom()).abs(),
                Side::Below => (t.root.y - l.rect.y).abs(),
                Side::Right => (t.root.x - l.rect.x).abs(),
                Side::Left => (t.root.x - l.rect.right()).abs(),
            };
            assert!(on_edge < 0.01, "{name}: the tail root is not on the body edge");
        }
    }

    #[test]
    fn near_the_top_she_speaks_downward_and_near_the_bottom_upward() {
        let mut e = engine();
        let bounds = hd();
        let her = 160.0;
        let top = Point::new(900.0, 90.0);
        let bottom = Point::new(900.0, 1000.0);
        let mut dry = Dry::new(&mut e);
        let hi = Layout::new("Small thing.", Urgency::Notable, top, her, bounds, &mut dry);
        let lo = Layout::new("Small thing.", Urgency::Notable, bottom, her, bounds, &mut dry);
        assert_eq!(hi.side, Side::Below, "no room above her at y=90");
        assert_eq!(lo.side, Side::Above, "there is room above her at y=1000");
        assert!(hi.rect.y > top.y, "a Below bubble sits under her");
        assert!(lo.rect.bottom() < bottom.y, "an Above bubble sits over her");
    }

    #[test]
    fn near_the_right_edge_the_body_slides_left_of_her() {
        let mut e = engine();
        let bounds = hd();
        let mut dry = Dry::new(&mut e);
        let right = Point::new(1880.0, 540.0);
        let l = Layout::new(LONG, Urgency::Answer, right, 160.0, bounds, &mut dry);
        assert!(l.rect.centre().x < right.x, "the bubble must fall back toward the middle");
        assert!(contains(bounds, l.rect));
    }

    #[test]
    fn a_surface_barely_larger_than_the_bubble_still_contains_it() {
        let mut e = engine();
        // A single small output: every side fails, so `place` takes the least
        // bad one and clamps. Containment must survive that path.
        let bounds = Rect::from_size(420.0, 300.0);
        for (_, a) in anchors(bounds, 160.0) {
            let mut dry = Dry::new(&mut e);
            let l = Layout::new(LONG, Urgency::Alarm, a, 160.0, bounds, &mut dry);
            assert!(contains(bounds, l.rect), "{:?} escaped {bounds:?}", l.rect);
        }
    }

    // --------------------------------------------------------------- wrapping

    #[test]
    fn long_text_wraps_and_no_line_overflows_the_measure() {
        let mut e = engine();
        let bounds = hd();
        let mut dry = Dry::new(&mut e);
        let l = Layout::new(LONG, Urgency::Answer, bounds.centre(), 160.0, bounds, &mut dry);
        assert!(l.lines.len() > 1, "that sentence has to wrap");
        let style = Role::Body.style();
        let max = max_content_width(bounds);
        for line in &l.lines {
            let w = dry.measure(line, &style, None).0;
            assert!(w <= max + 0.5, "line {line:?} is {w}px, over the {max}px measure");
        }
        assert!(l.rect.w <= MAX_WIDTH_PX + 0.01);
    }

    #[test]
    fn a_single_unbreakable_word_is_broken_rather_than_overflowing() {
        let mut e = engine();
        let bounds = hd();
        let mut dry = Dry::new(&mut e);
        let word = "supercalifragilisticexpialidocious".repeat(4);
        let l = Layout::new(&word, Urgency::Notable, bounds.centre(), 160.0, bounds, &mut dry);
        let style = Role::Body.style();
        let max = max_content_width(bounds);
        for line in &l.lines {
            assert!(dry.measure(line, &style, None).0 <= max + 0.5, "{line:?} overflowed");
        }
    }

    #[test]
    fn an_essay_is_elided_rather_than_pushed_off_the_output() {
        let mut e = engine();
        let bounds = hd();
        let mut dry = Dry::new(&mut e);
        let essay = LONG.repeat(12);
        let l = Layout::new(&essay, Urgency::Answer, bounds.centre(), 160.0, bounds, &mut dry);
        assert!(l.lines.len() <= MAX_LINES);
        assert!(l.lines.last().expect("lines").ends_with('…'), "elision must be visible");
        assert!(contains(bounds, l.rect));
    }

    #[test]
    fn empty_text_produces_an_empty_wrap_and_still_places() {
        let mut e = engine();
        let bounds = hd();
        let mut dry = Dry::new(&mut e);
        let l = Layout::new("   ", Urgency::Whim, bounds.centre(), 160.0, bounds, &mut dry);
        assert!(l.lines.is_empty());
        assert_eq!(l.chars, 0);
        assert!(contains(bounds, l.rect));
    }

    // ------------------------------------------------------------- typewriter

    #[test]
    fn reveal_uncovers_strictly_more_characters() {
        let mut e = engine();
        let bounds = hd();
        let l = {
            let mut dry = Dry::new(&mut e);
            Layout::new(LONG, Urgency::Answer, bounds.centre(), 160.0, bounds, &mut dry)
        };
        let mut seen = Vec::new();
        for r in [0.0f32, 0.5, 1.0] {
            let mut dry = Dry::new(&mut e);
            let mut scene = Scene::new();
            l.paint(Tier::Full, r, &mut dry, &mut scene);
            seen.push(dry.drawn_chars());
        }
        assert_eq!(seen[0], 0, "nothing is typed at reveal 0");
        assert!(seen[1] > seen[0], "half way must show something: {seen:?}");
        assert!(seen[2] > seen[1], "the end must show more than half: {seen:?}");
        assert_eq!(seen[2], l.chars, "reveal 1 shows the whole utterance");
    }

    #[test]
    fn the_wrap_does_not_reflow_while_it_types() {
        let mut e = engine();
        let bounds = hd();
        let l = {
            let mut dry = Dry::new(&mut e);
            Layout::new(LONG, Urgency::Answer, bounds.centre(), 160.0, bounds, &mut dry)
        };
        // Whatever the reveal, the painted prefixes must be prefixes of the
        // *same* fixed lines — that is the whole reason wrapping lives here.
        for step in 0..=20 {
            let r = step as f32 / 20.0;
            let mut dry = Dry::new(&mut e);
            let mut scene = Scene::new();
            l.paint(Tier::Full, r, &mut dry, &mut scene);
            for (i, painted) in dry.drawn.iter().enumerate() {
                assert!(
                    l.lines[i].starts_with(painted.as_str()),
                    "line {i} at reveal {r}: {painted:?} is not a prefix of {:?}",
                    l.lines[i]
                );
            }
        }
    }

    #[test]
    fn visible_chars_is_monotonic_and_clamped() {
        assert_eq!(visible_chars(10, -1.0), 0);
        assert_eq!(visible_chars(10, 0.0), 0);
        assert_eq!(visible_chars(10, 0.5), 5);
        assert_eq!(visible_chars(10, 1.0), 10);
        assert_eq!(visible_chars(10, 4.0), 10);
        assert_eq!(visible_chars(0, 0.5), 0);
        let mut prev = 0;
        for i in 0..=100 {
            let n = visible_chars(37, i as f32 / 100.0);
            assert!(n >= prev);
            prev = n;
        }
    }

    #[test]
    fn a_multibyte_utterance_never_splits_a_character() {
        let mut e = engine();
        let bounds = hd();
        let text = "Ich höre nichts — außer dem Lüfter, der ständig läuft.";
        let l = {
            let mut dry = Dry::new(&mut e);
            Layout::new(text, Urgency::Whim, bounds.centre(), 160.0, bounds, &mut dry)
        };
        for step in 0..=40 {
            let mut dry = Dry::new(&mut e);
            let mut scene = Scene::new();
            l.paint(Tier::Full, step as f32 / 40.0, &mut dry, &mut scene);
        }
    }

    // ----------------------------------------------------------------- tiers

    #[test]
    fn no_tier_ever_spends_a_blur() {
        let mut e = engine();
        let bounds = hd();
        let l = {
            let mut dry = Dry::new(&mut e);
            Layout::new(LONG, Urgency::Answer, bounds.centre(), 160.0, bounds, &mut dry)
        };
        for t in [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant] {
            let mut dry = Dry::new(&mut e);
            let mut scene = Scene::new();
            l.paint(t, 1.0, &mut dry, &mut scene);
            assert_eq!(scene.blur_count(), 0, "{t:?} bought a backdrop blur");
            assert!(
                !scene.cmds().iter().any(|c| matches!(c, Cmd::Backdrop { .. })),
                "{t:?} sampled a backdrop it cannot have"
            );
        }
    }

    #[test]
    fn t3_is_flat_fills_only() {
        let mut e = engine();
        let bounds = hd();
        let l = {
            let mut dry = Dry::new(&mut e);
            Layout::new(LONG, Urgency::Alarm, bounds.centre(), 160.0, bounds, &mut dry)
        };
        let mut dry = Dry::new(&mut e);
        let mut scene = Scene::new();
        l.paint(Tier::Lobotomised, 1.0, &mut dry, &mut scene);
        assert!(!scene.is_empty(), "T3 still draws — she is quieter, not gone");
        for c in scene.cmds() {
            match c {
                Cmd::Fill { paint, .. } | Cmd::Stroke { paint, .. } => assert!(
                    matches!(paint, Paint::Solid(_)),
                    "T3 emitted a gradient: {paint:?}"
                ),
                Cmd::BlurBackdrop { .. } | Cmd::Backdrop { .. } => {
                    panic!("T3 emitted {c:?}")
                }
                _ => {}
            }
        }
    }

    #[test]
    fn t4_draws_nothing_at_all() {
        let mut e = engine();
        let bounds = hd();
        for u in URGENCIES {
            let l = {
                let mut dry = Dry::new(&mut e);
                Layout::new(LONG, u, bounds.centre(), 160.0, bounds, &mut dry)
            };
            let mut dry = Dry::new(&mut e);
            let mut scene = Scene::new();
            l.paint(Tier::Dormant, 1.0, &mut dry, &mut scene);
            assert!(scene.is_empty(), "{u:?} drew at T4");
            assert!(dry.drawn.is_empty(), "{u:?} rasterised text at T4");
        }
    }

    #[test]
    fn t1_uses_the_glass_body_and_its_specular() {
        let mut e = engine();
        let bounds = hd();
        let l = {
            let mut dry = Dry::new(&mut e);
            Layout::new("Hello.", Urgency::Whim, bounds.centre(), 160.0, bounds, &mut dry)
        };
        let mut dry = Dry::new(&mut e);
        let mut scene = Scene::new();
        l.paint(Tier::Full, 1.0, &mut dry, &mut scene);
        let gradients = scene
            .cmds()
            .iter()
            .filter(|c| {
                matches!(c, Cmd::Fill { paint, .. } if matches!(paint, Paint::Linear { .. }))
            })
            .count();
        assert!(gradients >= 2, "the body and the specular are both gradients");
    }

    // -------------------------------------------------------------- urgency

    #[test]
    fn every_urgency_looks_different_in_the_command_list() {
        let mut e = engine();
        let bounds = hd();
        let mut shapes: Vec<(Urgency, String)> = Vec::new();
        for u in URGENCIES {
            let l = {
                let mut dry = Dry::new(&mut e);
                Layout::new("Something happened.", u, bounds.centre(), 160.0, bounds, &mut dry)
            };
            let mut dry = Dry::new(&mut e);
            let mut scene = Scene::new();
            l.paint(Tier::Full, 1.0, &mut dry, &mut scene);
            shapes.push((u, format!("{:?}", scene.cmds())));
        }
        for i in 0..shapes.len() {
            for j in (i + 1)..shapes.len() {
                assert_ne!(
                    shapes[i].1, shapes[j].1,
                    "{:?} and {:?} draw identically",
                    shapes[i].0, shapes[j].0
                );
            }
        }
    }

    #[test]
    fn the_signal_colours_stay_in_their_lanes() {
        // §1: amber is attention, #ff5470 is danger, and neither leaks.
        assert_eq!(signal(Urgency::Whim).rule, None);
        assert_eq!(signal(Urgency::Notable).rule, Some(palette::AMBER));
        assert_eq!(signal(Urgency::Answer).rule, Some(palette::VIOLET_SOFT));
        assert_eq!(signal(Urgency::Alarm).rule, Some(palette::DANGER));
        assert_eq!(signal(Urgency::Alarm).edge, Some(palette::DANGER));
        assert_eq!(palette::DANGER.to_hex(), 0xff5470);
        for u in [Urgency::Whim, Urgency::Notable, Urgency::Answer] {
            assert!(signal(u).edge.is_none(), "{u:?} must not repaint the edge");
            assert!(!signal(u).wedge, "{u:?} is not an alarm");
        }
    }

    #[test]
    fn only_an_alarm_draws_danger_and_it_never_recolours_the_body() {
        let mut e = engine();
        let bounds = hd();
        for u in URGENCIES {
            let l = {
                let mut dry = Dry::new(&mut e);
                Layout::new("Check it.", u, bounds.centre(), 160.0, bounds, &mut dry)
            };
            let mut dry = Dry::new(&mut e);
            let mut scene = Scene::new();
            l.paint(Tier::Full, 1.0, &mut dry, &mut scene);
            let danger = scene
                .cmds()
                .iter()
                .filter(|c| match c {
                    Cmd::Fill { paint, .. } | Cmd::Stroke { paint, .. } => {
                        matches!(paint, Paint::Solid(c) if *c == palette::DANGER)
                    }
                    _ => false,
                })
                .count();
            if u == Urgency::Alarm {
                assert!(danger >= 2, "an alarm shows danger on the edge and the wedge");
            } else {
                assert_eq!(danger, 0, "{u:?} used the danger colour");
            }
            // The body is always the same glass, whatever the urgency.
            let body = Floating::Bubble.material().body;
            assert!(
                matches!(&scene.cmds()[0], Cmd::Fill { paint, .. } if *paint == Paint::token(body)),
                "{u:?} recoloured the body"
            );
        }
    }

    #[test]
    fn an_alarm_reserves_a_row_for_its_wedge() {
        let mut e = engine();
        let bounds = hd();
        let mut dry = Dry::new(&mut e);
        let calm = Layout::new("Same words.", Urgency::Notable, bounds.centre(), 160.0, bounds, &mut dry);
        let loud = Layout::new("Same words.", Urgency::Alarm, bounds.centre(), 160.0, bounds, &mut dry);
        assert!(loud.rect.h > calm.rect.h, "the wedge needs its own row");
        assert!((loud.rect.h - calm.rect.h - WARN_ROW_PX).abs() < loud.line_h + 1.0);
    }

    // -------------------------------------------------------------- lifetime

    #[test]
    fn the_lifetime_ladder_is_ordered_and_an_alarm_waits_for_you() {
        let chars = 60;
        let whim = lifetime(Urgency::Whim).total_ms(chars);
        let notable = lifetime(Urgency::Notable).total_ms(chars);
        let answer = lifetime(Urgency::Answer).total_ms(chars);
        assert!(whim < notable, "{whim} !< {notable}");
        assert!(notable < answer, "{notable} !< {answer}");
        assert!(lifetime(Urgency::Alarm).sticky);
        for u in [Urgency::Whim, Urgency::Notable, Urgency::Answer] {
            assert!(!lifetime(u).sticky, "{u:?} must time out");
            // §6: nothing interactive exceeds 320ms.
            assert!(lifetime(u).fade_ms <= wisp_theme::motion::MAX_INTERACTIVE_MS);
        }
        assert!(lifetime(Urgency::Alarm).fade_ms <= wisp_theme::motion::MAX_INTERACTIVE_MS);
    }

    #[test]
    fn should_dismiss_is_a_pure_function_of_how_long_it_has_been_up() {
        let l = lifetime(Urgency::Notable);
        let chars = 20;
        let total = l.total_ms(chars) as u64;
        assert!(!l.should_dismiss(chars, 0));
        assert!(!l.should_dismiss(chars, total - 1));
        assert!(l.should_dismiss(chars, total));
        assert!(l.should_dismiss(chars, total + 10_000));
        // An alarm never dismisses itself, however long it waits.
        let a = lifetime(Urgency::Alarm);
        assert!(!a.should_dismiss(chars, 10_000_000));
        assert_eq!(a.opacity_at(chars, 10_000_000), 1.0);
    }

    #[test]
    fn a_long_utterance_never_outlives_the_ceiling() {
        for u in URGENCIES {
            let l = lifetime(u);
            assert_eq!(l.total_ms(100_000), l.max_ms, "{u:?} ignored its ceiling");
        }
    }

    // ------------------------------------------------------------ by looking

    /// DESIGN.md §11 is reviewed by looking, not by reading code. This renders
    /// offscreen — **never a window**, SPEC §4 — and with `WISP_SHELL_DUMP=<dir>`
    /// writes each frame composited over a checkerboard, which is the only way
    /// to judge whether the ≥0.85 body is actually holding the desktop out.
    fn dump(img: &wisp_paint::Image, name: &str) {
        let Some(dir) = std::env::var_os("WISP_SHELL_DUMP") else { return };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).ok();
        let mut out = format!("P6\n{} {}\n255\n", img.w, img.h).into_bytes();
        for y in 0..img.h {
            for x in 0..img.w {
                let [r, g, b, a] = img.pixel(x, y);
                // A checker, so translucency is visible as a pattern rather
                // than as a shade you have to take on trust.
                let light = ((x / 24) + (y / 24)) % 2 == 0;
                let bg: [u8; 3] = if light { [0x86, 0x86, 0x8e] } else { [0x4e, 0x4e, 0x58] };
                let inv = (255 - a) as u32;
                for (i, c) in [r, g, b].into_iter().enumerate() {
                    out.push((c as u32 + bg[i] as u32 * inv / 255).min(255) as u8);
                }
            }
        }
        std::fs::write(dir.join(format!("{name}.ppm")), out).ok();
    }

    /// A stand-in for her silhouette, so the tail has something to aim at.
    fn her_stand_in(scene: &mut Scene, anchor: Point, size: f32) {
        let b = her_box(anchor, size);
        scene.fill(
            Path::rounded_rect(b, Radius::CARD),
            Paint::solid(palette::VIOLET.with_alpha(0.55)),
        );
    }

    #[test]
    fn the_bubble_renders_and_can_be_reviewed_by_looking() {
        if std::env::var_os("NX_WISP_CONFIG_DIR").is_none() {
            let dir = std::env::temp_dir().join(format!("nx-wisp-shell-{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok();
            std::env::set_var("NX_WISP_CONFIG_DIR", &dir);
        }
        let Ok(mut p) = wisp_paint::Painter::new(wisp_paint::AdapterPreference::HighPerformance)
        else {
            eprintln!("no Vulkan adapter here — skipping the look-at-it test");
            return;
        };
        let mut e = engine();

        let (w, h) = (720u32, 460u32);
        let bounds = Rect::from_size(w as f32, h as f32);
        let her = 120.0;
        let cases: [(&str, Urgency, Tier, f32, Point); 6] = [
            ("bubble_whim", Urgency::Whim, Tier::Full, 1.0, Point::new(360.0, 330.0)),
            ("bubble_notable", Urgency::Notable, Tier::Full, 1.0, Point::new(360.0, 330.0)),
            ("bubble_answer", Urgency::Answer, Tier::Full, 1.0, Point::new(360.0, 330.0)),
            ("bubble_alarm", Urgency::Alarm, Tier::Full, 1.0, Point::new(360.0, 330.0)),
            ("bubble_answer_t3", Urgency::Answer, Tier::Lobotomised, 1.0, Point::new(360.0, 330.0)),
            ("bubble_reveal_half", Urgency::Answer, Tier::Full, 0.5, Point::new(360.0, 330.0)),
        ];
        for (name, u, tier, reveal, anchor) in cases {
            let target = p.offscreen(w, h).expect("offscreen");
            let mut scene = Scene::new();
            her_stand_in(&mut scene, anchor, her);
            let layout = {
                let mut live = Live::new(&p, &mut e);
                let l = Layout::new(LONG, u, anchor, her, bounds, &mut live);
                l.paint(tier, reveal, &mut live, &mut scene);
                l
            };
            assert!(!scene.is_empty());
            assert_eq!(scene.blur_count(), 0);
            p.render(&target, &scene).expect("render");
            let img = p.read(&target).expect("read");
            dump(&img, name);
            // The body must actually be there: sample the middle of it.
            let c = layout.rect.centre();
            let a = img.alpha(c.x as u32, c.y as u32);
            assert!(a > 230, "{name}: the body is only {a}/255 opaque");
        }

        // The blur decision, measured rather than asserted. A layer surface's
        // "backdrop" is the painter's own scratch texture — the desktop under
        // the Wayland surface is not in it — so a `BlurBackdrop` under the
        // bubble frosts transparent black. Render the same frame with and
        // without one and show that it buys nothing but passes.
        {
            let anchor = Point::new(360.0, 330.0);
            let mut plain = Scene::new();
            her_stand_in(&mut plain, anchor, her);
            let layout = {
                let mut live = Live::new(&p, &mut e);
                let l = Layout::new(LONG, Urgency::Answer, anchor, her, bounds, &mut live);
                l.paint(Tier::Full, 1.0, &mut live, &mut plain);
                l
            };
            let mut blurred = Scene::new();
            her_stand_in(&mut blurred, anchor, her);
            let m = Floating::Bubble.material();
            blurred.push(wisp_paint::Cmd::BlurBackdrop { blur: m.blur });
            blurred.push(wisp_paint::Cmd::Backdrop { rect: layout.rect, radius: m.radius });
            let n_her = 1;
            for c in plain.cmds().iter().skip(n_her) {
                blurred.push(c.clone());
            }

            let a = p.offscreen(w, h).expect("offscreen");
            p.render(&a, &plain).expect("render");
            let plain_calls = p.last_draw_calls();
            let img_a = p.read(&a).expect("read");

            let b = p.offscreen(w, h).expect("offscreen");
            p.render(&b, &blurred).expect("render");
            let blur_calls = p.last_draw_calls();
            let img_b = p.read(&b).expect("read");
            dump(&img_b, "bubble_with_a_pointless_blur");

            assert_eq!(p.last_blur_count(), 1, "the control frame really did blur");
            assert!(
                blur_calls > plain_calls,
                "a blur has to cost something: {plain_calls} vs {blur_calls}"
            );
            let diff = img_a.mean_abs_diff(&img_b);
            assert!(
                diff < 0.5,
                "a real backdrop blur changed the frame by {diff}/255 — if this \
                 ever becomes visible, revisit the no-blur decision in the module docs"
            );
        }

        // The flip, in one frame: four corners, four placements.
        let target = p.offscreen(w, h).expect("offscreen");
        let mut scene = Scene::new();
        for a in [
            Point::new(80.0, 80.0),
            Point::new(640.0, 80.0),
            Point::new(80.0, 380.0),
            Point::new(640.0, 380.0),
        ] {
            her_stand_in(&mut scene, a, 90.0);
            let mut live = Live::new(&p, &mut e);
            let l = Layout::new("Corner check: the tail still finds her.", Urgency::Notable, a, 90.0, bounds, &mut live);
            assert!(contains(bounds, l.bounds()));
            l.paint(Tier::Full, 1.0, &mut live, &mut scene);
        }
        p.render(&target, &scene).expect("render");
        dump(&p.read(&target).expect("read"), "bubble_corners");
    }

    #[test]
    fn reveal_and_opacity_track_the_clock_the_host_owns() {
        let l = lifetime(Urgency::Answer);
        let chars = 50;
        assert_eq!(l.reveal_at(chars, 0), 0.0);
        assert_eq!(l.reveal_at(chars, l.reveal_ms(chars) as u64), 1.0);
        assert!((l.reveal_at(chars, (l.reveal_ms(chars) / 2) as u64) - 0.5).abs() < 0.02);
        assert_eq!(l.opacity_at(chars, 0), 1.0);
        assert_eq!(l.opacity_at(chars, l.total_ms(chars) as u64), 0.0);
        assert!(l.opacity_at(chars, (l.total_ms(chars) - l.fade_ms / 2) as u64) < 1.0);
        // Zero-length text does not divide by zero.
        assert_eq!(l.reveal_at(0, 0), 1.0);
    }
}
