//! A frame's worth of drawing, recorded and then replayed by the [`Painter`].
//!
//! A `Scene` is immediate-mode *description* — the retained structure lives in
//! [`crate::widget`], which rebuilds a scene each frame. Keeping the two apart
//! is what lets the sprite-atlas baker bake exactly the same drawing code the
//! vector path runs.
//!
//! [`Painter`]: crate::Painter

use crate::geom::{Path, Point, Rect};
use crate::paint::Paint;
use crate::texture::Texture;
use wisp_theme::surface::{Edge, Floating, Recessed, Structural};
use wisp_theme::{Blur, Color, Radius};

#[derive(Debug, Clone)]
pub enum Cmd {
    Fill { path: Path, paint: Paint },
    Stroke { path: Path, paint: Paint, width: f32 },
    /// A shaped glyph run, rasterised to single-channel coverage and tinted.
    Text { rect: Rect, tex: Texture, color: Color },
    /// A quad out of a baked atlas. **The only thing T3 is allowed to draw.**
    Sprite { rect: Rect, uv: Rect, tint: Color, atlas: Texture },
    /// Blur everything drawn so far into the scratch backdrop, so the next
    /// [`Cmd::Backdrop`] can sample it. This is the real `backdrop-filter`
    /// of DESIGN.md §4, and it is a budget: the painter counts them.
    BlurBackdrop { blur: Blur },
    /// Paint the blurred backdrop back through a rounded-rect mask.
    Backdrop { rect: Rect, radius: Radius },
    /// `None` pops back to the whole target.
    Clip(Option<Rect>),
}

impl Cmd {
    /// Is this command part of the sprite path (T3) rather than the vector
    /// path? SPEC §3.1 gives T3 "canned" output only.
    pub fn is_sprite(&self) -> bool {
        matches!(self, Cmd::Sprite { .. } | Cmd::Clip(_))
    }

    /// May this command still be drawn at T3?
    ///
    /// T3's promise is "no compute passes, nothing on the discrete GPU" — it
    /// was never "nothing at all". Sprite quads plus **flat solid** fills,
    /// strokes and text cost a handful of trivial draw calls on the integrated
    /// GPU, which is the point of moving her there.
    ///
    /// This exists because T3 is exactly when a warning matters most: an
    /// NX Sentry alarm arrives while the operator is inside a game or a
    /// headset. `wisp-attn` already lets `Alarm` through at T3 and
    /// `wisp-fleet` already gates on it — and the painter was silently
    /// discarding the result, so she would have raised the alarm to nobody.
    /// Silence there is a product failure, not a saving.
    ///
    /// Gradients, blurs and backdrops stay shed: those are the expensive part.
    pub fn survives_lobotomy(&self) -> bool {
        match self {
            Cmd::Sprite { .. } | Cmd::Clip(_) | Cmd::Text { .. } => true,
            Cmd::Fill { paint, .. } | Cmd::Stroke { paint, .. } => {
                matches!(paint, crate::paint::Paint::Solid(_))
            }
            Cmd::BlurBackdrop { .. } | Cmd::Backdrop { .. } => false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Scene {
    cmds: Vec<Cmd>,
    clip_stack: Vec<Rect>,
}

impl Scene {
    pub fn new() -> Scene {
        Scene::default()
    }

    pub fn cmds(&self) -> &[Cmd] {
        &self.cmds
    }

    pub fn is_empty(&self) -> bool {
        self.cmds.is_empty()
    }

    pub fn clear(&mut self) {
        self.cmds.clear();
        self.clip_stack.clear();
    }

    /// How many real blurs this frame asks for. DESIGN.md §4 wants ≤10.
    pub fn blur_count(&self) -> usize {
        self.cmds.iter().filter(|c| matches!(c, Cmd::BlurBackdrop { .. })).count()
    }

    pub fn push(&mut self, cmd: Cmd) -> &mut Self {
        self.cmds.push(cmd);
        self
    }

    // ------------------------------------------------------------- primitives

    pub fn fill(&mut self, path: Path, paint: impl Into<Paint>) -> &mut Self {
        let paint = paint.into();
        if !paint.is_invisible() {
            self.cmds.push(Cmd::Fill { path, paint });
        }
        self
    }

    pub fn stroke(&mut self, path: Path, paint: impl Into<Paint>, width: f32) -> &mut Self {
        let paint = paint.into();
        if !paint.is_invisible() && width > 0.0 {
            self.cmds.push(Cmd::Stroke { path, paint, width });
        }
        self
    }

    pub fn fill_rect(&mut self, r: Rect, radius: Radius, paint: impl Into<Paint>) -> &mut Self {
        self.fill(Path::rounded_rect(r, radius), paint)
    }

    /// A perfect circle — only reachable with a `wisp_theme::Circle`, which
    /// only exists for status dots and spinners.
    pub fn circle(&mut self, at: Point, c: wisp_theme::Circle, paint: impl Into<Paint>) -> &mut Self {
        self.fill(Path::circle(at, c), paint)
    }

    // ------------------------------------------------------------- the language

    /// The 1px gradient border of DESIGN.md §4: bright top-left, dark
    /// bottom-right, inset by half a pixel so it lands *on* the boundary.
    pub fn lit_edge(&mut self, r: Rect, radius: Radius, edge: Edge) -> &mut Self {
        let Some(g) = edge.gradient() else { return self };
        match edge {
            // A bar's edge is one lit hairline along the top, not a border.
            Edge::TopOnly => {
                let line = Rect::new(r.x, r.y, r.w, Edge::WIDTH_PX);
                self.fill(Path::rect(line), Paint::token(g))
            }
            _ => {
                let inset = r.inset(Edge::WIDTH_PX * 0.5);
                self.stroke(Path::rounded_rect(inset, radius), Paint::token(g), Edge::WIDTH_PX)
            }
        }
    }

    /// A divider that fades out at both ends. There are no solid grey lines.
    pub fn hairline(&mut self, r: Rect) -> &mut Self {
        let line = Rect::new(r.x, r.y, r.w, 1.0);
        self.fill(Path::rect(line), Paint::token(wisp_theme::tokens::HAIRLINE))
    }

    /// An opaque elevation step: fill, then its lit edge. No blur exists to
    /// pass, because [`Structural`] has none.
    pub fn structural(&mut self, r: Rect, s: Structural) -> &mut Self {
        let m = s.material();
        self.fill_rect(r, m.radius, Paint::token(m.fill));
        self.lit_edge(r, m.radius, m.edge)
    }

    /// A well cut into whatever opaque surface is underneath.
    pub fn recessed(&mut self, r: Rect, s: Recessed) -> &mut Self {
        let m = s.material();
        self.fill_rect(r, m.radius, Paint::token(m.fill));
        self.lit_edge(r, m.radius, m.edge)
    }

    /// A genuinely floating layer: blur the backdrop, paint it back through
    /// the shape, then the ≥0.85 body, the specular, and the lit edge.
    ///
    /// This is the only call in the crate that spends a real blur.
    pub fn floating(&mut self, r: Rect, f: Floating) -> &mut Self {
        let m = f.material();
        self.cmds.push(Cmd::BlurBackdrop { blur: m.blur });
        self.cmds.push(Cmd::Backdrop { rect: r, radius: m.radius });
        self.fill_rect(r, m.radius, Paint::token(m.body));
        self.fill_rect(r, m.radius, Paint::token(m.highlight));
        self.lit_edge(r, m.radius, m.edge)
    }

    /// The specular band. `driver` is a position, never a clock — bind it to
    /// the pointer, the tilt, the scroll, or the progress value (§1).
    pub fn sheen(
        &mut self,
        r: Rect,
        radius: Radius,
        driver: wisp_theme::Sheen,
        motion: wisp_theme::Motion,
        elapsed_ms: u64,
    ) -> &mut Self {
        let Some(pos) = driver.position(motion, elapsed_ms) else { return self };
        let paint = Paint::token(wisp_theme::tokens::SHEEN).shifted(pos - 0.5);
        self.fill_rect(r, radius, paint)
    }

    // ------------------------------------------------------------- textured

    pub fn text(&mut self, rect: Rect, tex: Texture, color: Color) -> &mut Self {
        if color.a > 0 {
            self.cmds.push(Cmd::Text { rect, tex, color });
        }
        self
    }

    pub fn sprite(&mut self, rect: Rect, uv: Rect, atlas: Texture, tint: Color) -> &mut Self {
        self.cmds.push(Cmd::Sprite { rect, uv, tint, atlas });
        self
    }

    // ------------------------------------------------------------- clipping

    /// Clip to `r`, intersected with whatever clip is already in force.
    pub fn push_clip(&mut self, r: Rect) -> &mut Self {
        let resolved = match self.clip_stack.last() {
            Some(prev) => prev.intersect(r),
            None => r,
        };
        self.clip_stack.push(resolved);
        self.cmds.push(Cmd::Clip(Some(resolved)));
        self
    }

    pub fn pop_clip(&mut self) -> &mut Self {
        self.clip_stack.pop();
        self.cmds.push(Cmd::Clip(self.clip_stack.last().copied()));
        self
    }

    /// The clip currently in force, for widgets that want to cull.
    pub fn current_clip(&self) -> Option<Rect> {
        self.clip_stack.last().copied()
    }

    /// Everything this scene touches, for damage tracking by `wisp-shell`.
    pub fn bounds(&self) -> Rect {
        let mut b = Rect::ZERO;
        for c in &self.cmds {
            let r = match c {
                Cmd::Fill { path, .. } => path.bbox(),
                Cmd::Stroke { path, width, .. } => path.bbox().inset(-width),
                Cmd::Text { rect, .. } | Cmd::Sprite { rect, .. } => *rect,
                Cmd::Backdrop { rect, .. } => *rect,
                Cmd::BlurBackdrop { .. } | Cmd::Clip(_) => continue,
            };
            b = b.union(r);
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_theme::palette;

    #[test]
    fn an_invisible_paint_records_nothing() {
        let mut s = Scene::new();
        s.fill_rect(Rect::from_size(10.0, 10.0), Radius::CARD, palette::VIOLET.with_alpha(0.0));
        assert!(s.is_empty());
        s.stroke(Path::rect(Rect::from_size(1.0, 1.0)), palette::VIOLET, 0.0);
        assert!(s.is_empty());
    }

    #[test]
    fn a_structural_surface_records_a_fill_and_an_edge_and_no_blur() {
        let mut s = Scene::new();
        s.structural(Rect::from_size(100.0, 60.0), Structural::Card);
        assert_eq!(s.cmds().len(), 2);
        assert!(matches!(s.cmds()[0], Cmd::Fill { .. }));
        assert!(matches!(s.cmds()[1], Cmd::Stroke { .. }));
        assert_eq!(s.blur_count(), 0);
    }

    #[test]
    fn a_floating_layer_spends_exactly_one_blur() {
        let mut s = Scene::new();
        s.floating(Rect::from_size(200.0, 120.0), Floating::Sheet);
        assert_eq!(s.blur_count(), 1);
        assert!(matches!(s.cmds()[0], Cmd::BlurBackdrop { .. }));
        assert!(matches!(s.cmds()[1], Cmd::Backdrop { .. }));
    }

    #[test]
    fn a_bar_edge_is_a_top_hairline_not_a_border() {
        let mut s = Scene::new();
        s.lit_edge(Rect::new(0.0, 0.0, 100.0, 40.0), Radius::SM, Edge::TopOnly);
        match &s.cmds()[0] {
            Cmd::Fill { path, .. } => assert_eq!(path.bbox().h, 1.0),
            other => panic!("expected a 1px fill, got {other:?}"),
        }
    }

    #[test]
    fn clips_intersect_and_unwind() {
        let mut s = Scene::new();
        s.push_clip(Rect::new(0.0, 0.0, 100.0, 100.0));
        s.push_clip(Rect::new(50.0, 50.0, 100.0, 100.0));
        assert_eq!(s.current_clip(), Some(Rect::new(50.0, 50.0, 50.0, 50.0)));
        s.pop_clip();
        assert_eq!(s.current_clip(), Some(Rect::new(0.0, 0.0, 100.0, 100.0)));
        s.pop_clip();
        assert_eq!(s.current_clip(), None);
        assert!(matches!(s.cmds().last(), Some(Cmd::Clip(None))));
    }

    #[test]
    fn bounds_cover_everything_drawn() {
        let mut s = Scene::new();
        s.fill_rect(Rect::new(10.0, 10.0, 20.0, 20.0), Radius::SM, palette::VIOLET);
        s.fill_rect(Rect::new(100.0, 5.0, 10.0, 10.0), Radius::SM, palette::CYAN);
        let b = s.bounds();
        assert_eq!(b.x, 10.0);
        assert_eq!(b.y, 5.0);
        assert_eq!(b.right(), 110.0);
        assert_eq!(b.bottom(), 30.0);
    }

    #[test]
    fn a_reduced_motion_sheen_records_nothing() {
        let mut s = Scene::new();
        s.sheen(
            Rect::from_size(100.0, 40.0),
            Radius::CARD,
            wisp_theme::Sheen::Driven(0.5),
            wisp_theme::Motion::Reduced,
            0,
        );
        assert!(s.is_empty());
    }

    #[test]
    fn only_sprites_and_clips_survive_the_t3_filter() {
        let mut s = Scene::new();
        s.structural(Rect::from_size(10.0, 10.0), Structural::Card);
        assert!(s.cmds().iter().all(|c| !c.is_sprite()));
        assert!(Cmd::Clip(None).is_sprite());
    }
}
