//! DESIGN.md §4 — the glass tier system, as amended by v1.5 §12.
//!
//! > Structural, always-present surfaces (cards, tiles, rails) are **opaque**
//! > elevation steps. Depth comes from surface steps and the lit edge, never
//! > from see-through. Translucency + real blur are reserved for layers that
//! > genuinely float, and a floating layer's fill stays ≥ 0.85 alpha.
//!
//! That rule is expressed in the types rather than in a comment. There are
//! three surface kinds and each returns a *different material struct*:
//!
//! | Kind | Material | Has a `blur` field? |
//! |---|---|---|
//! | [`Structural`] | [`OpaqueMaterial`] | **no** |
//! | [`Recessed`] | [`InsetMaterial`] | **no** |
//! | [`Floating`] | [`GlassMaterial`] | yes, and it is not optional |
//!
//! So "put a real blur on a card" is not a mistake you can make quietly: the
//! field does not exist. And "float something see-through" is caught by
//! [`GlassMaterial::validate`], which every floating tier is tested against.

use crate::color::Color;
use crate::gradient::{Gradient, LightModel};
use crate::palette;
use crate::radius::Radius;
use crate::tokens;
use crate::ThemeViolation;

// ---------------------------------------------------------------- primitives

/// One of the **three** blur strengths that exist. There is no fourth, and no
/// arbitrary radius: §2 lists exactly `--blur-bar`, `--blur-sheet`,
/// `--blur-chip`, and a real backdrop blur is a budget, not a default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Blur {
    pub radius_px: f32,
    /// Backdrop saturation multiplier, e.g. 1.70 for `saturate(170%)`.
    pub saturate: f32,
}

impl Blur {
    /// `--blur-bar: blur(22px) saturate(170%)`
    pub const BAR: Blur = Blur { radius_px: 22.0, saturate: 1.70 };
    /// `--blur-sheet: blur(34px) saturate(185%)`
    pub const SHEET: Blur = Blur { radius_px: 34.0, saturate: 1.85 };
    /// `--blur-chip: blur(16px) saturate(160%)`
    pub const CHIP: Blur = Blur { radius_px: 16.0, saturate: 1.60 };
    pub const ALL: [Blur; 3] = [Blur::BAR, Blur::SHEET, Blur::CHIP];
}

/// §4: keep simultaneous real-blur elements at roughly ≤10 visible. The
/// renderer counts them and the governor sheds the excess.
pub const MAX_SIMULTANEOUS_BLURS: usize = 10;

/// A 1px gradient border, brighter top-left and darker bottom-right.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Edge {
    /// `--edge` — the card border.
    Plain,
    /// `--edge-lit` — the sheet border, with violet and cyan inside the glass.
    Lit,
    /// `--edge-top` — one lit hairline along the top only, for bars.
    TopOnly,
    /// `--line`, used as a gradient hairline. Wells and inputs.
    Line,
    None,
}

const EDGE_TOP_GRADIENT: Gradient =
    Gradient::solid(&[crate::gradient::Stop::new(0.0, tokens::EDGE_TOP)]);
const LINE_GRADIENT: Gradient = Gradient::solid(&[crate::gradient::Stop::new(0.0, palette::LINE)]);

impl Edge {
    pub const WIDTH_PX: f32 = 1.0;

    pub fn gradient(self) -> Option<Gradient> {
        match self {
            Edge::Plain => Some(tokens::EDGE),
            Edge::Lit => Some(tokens::EDGE_LIT),
            Edge::TopOnly => Some(EDGE_TOP_GRADIENT),
            Edge::Line => Some(LINE_GRADIENT),
            Edge::None => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowLayer {
    pub dx: f32,
    pub dy: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
}

/// A stack of drop-shadow layers, outermost first, exactly as the CSS token
/// lists them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow(pub &'static [ShadowLayer]);

const fn sl(dx: f32, dy: f32, blur: f32, spread: f32, color: Color) -> ShadowLayer {
    ShadowLayer { dx, dy, blur, spread, color }
}

/// `--shadow`
pub const SHADOW: Shadow = Shadow(&[
    sl(0.0, 14.0, 34.0, -12.0, Color::hexa(0x000000, 0.72)),
    sl(0.0, 2.0, 8.0, 0.0, Color::hexa(0x000000, 0.30)),
]);
/// `--shadow-lift` — hover. The second layer is a violet bloom, not a shadow.
pub const SHADOW_LIFT: Shadow = Shadow(&[
    sl(0.0, 26.0, 54.0, -16.0, Color::hexa(0x000000, 0.80)),
    sl(0.0, 0.0, 40.0, -8.0, Color::hexa(0x7700ff, 0.34)),
]);
/// `--shadow-bar`
pub const SHADOW_BAR: Shadow = Shadow(&[
    sl(0.0, 20.0, 44.0, -24.0, Color::hexa(0x000000, 0.90)),
    sl(0.0, 1.0, 0.0, 0.0, Color::hexa(0xffffff, 0.04)),
]);
/// `--shadow-sheet`
pub const SHADOW_SHEET: Shadow = Shadow(&[
    sl(0.0, 48.0, 96.0, -32.0, Color::hexa(0x000000, 0.86)),
    sl(0.0, 0.0, 0.0, 1.0, Color::hexa(0xffffff, 0.06)),
]);
/// `--focus-ring`. Never a bare outline (§5).
pub const FOCUS_RING: Shadow = Shadow(&[
    sl(0.0, 0.0, 0.0, 2.0, Color::hexa(0x7700ff, 0.60)),
    sl(0.0, 0.0, 0.0, 5.0, Color::hexa(0x7700ff, 0.20)),
]);

// ----------------------------------------------------------------- materials

/// What a **structural** surface is made of. Note what is missing: there is no
/// `blur` field, because a structural surface never has one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpaqueMaterial {
    /// Fully opaque, always. [`OpaqueMaterial::validate`] proves it.
    pub fill: Gradient,
    pub edge: Edge,
    pub shadow: Option<Shadow>,
    pub radius: Radius,
    /// Which face this is. Everything with elevation is `Raised`; the field
    /// itself shades nothing and is `Ambient`.
    pub light: LightModel,
}

impl OpaqueMaterial {
    pub fn validate(&self) -> Result<(), ThemeViolation> {
        if !self.fill.is_opaque() {
            return Err(ThemeViolation::TranslucentStructuralSurface(self.fill.min_alpha()));
        }
        if !self.fill.obeys_light(self.light) {
            return Err(ThemeViolation::LightFromTheWrongSide);
        }
        Ok(())
    }
}

/// What a **recessed** region inside an opaque parent is made of. Translucent
/// (it darkens whatever opaque surface it is cut into) but never blurred —
/// glass inside glass reads as fog.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InsetMaterial {
    pub fill: Gradient,
    pub edge: Edge,
    pub radius: Radius,
}

impl InsetMaterial {
    pub fn validate(&self) -> Result<(), ThemeViolation> {
        if !self.fill.obeys_light(LightModel::Recessed) {
            return Err(ThemeViolation::LightFromTheWrongSide);
        }
        Ok(())
    }
}

/// What a **floating** layer is made of. The blur is mandatory — if a surface
/// does not deserve a real blur it is not a floating layer, it is a card.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlassMaterial {
    /// The body. Carries the legibility; alpha ≥ [`MIN_FLOATING_ALPHA`].
    pub body: Gradient,
    /// The white top-left specular painted over the body.
    pub highlight: Gradient,
    pub blur: Blur,
    pub edge: Edge,
    pub shadow: Shadow,
    pub radius: Radius,
}

/// v1.5 §4: "a floating layer's fill stays ≥ 0.85 alpha so nothing behind it
/// shows through its body."
pub const MIN_FLOATING_ALPHA: f32 = 0.85;

impl GlassMaterial {
    pub fn validate(&self) -> Result<(), ThemeViolation> {
        let a = self.body.min_alpha();
        if a < MIN_FLOATING_ALPHA - 1e-4 {
            return Err(ThemeViolation::FloatingFillTooSheer(a));
        }
        if !self.body.obeys_light(LightModel::Raised) {
            return Err(ThemeViolation::LightFromTheWrongSide);
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ surfaces

/// Always present, opaque, an elevation step. Cards, tiles, rails, the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Structural {
    /// The deep-space field itself, behind everything.
    Field,
    /// The rail / sidebar, one step below a card.
    Rail,
    /// `--surface-1`. Content cards and tiles.
    Card,
    /// `--surface-1-hover`.
    CardHover,
}

impl Structural {
    pub fn material(self) -> OpaqueMaterial {
        match self {
            Structural::Field => OpaqueMaterial {
                fill: tokens::FIELD,
                edge: Edge::None,
                shadow: None,
                radius: Radius::XS,
                light: LightModel::Ambient,
            },
            Structural::Rail => OpaqueMaterial {
                fill: tokens::SURFACE_0,
                edge: Edge::Plain,
                shadow: None,
                radius: Radius::SM,
                light: LightModel::Raised,
            },
            Structural::Card => OpaqueMaterial {
                fill: tokens::SURFACE_1,
                edge: Edge::Plain,
                shadow: Some(SHADOW),
                radius: Radius::CARD,
                light: LightModel::Raised,
            },
            Structural::CardHover => OpaqueMaterial {
                fill: tokens::SURFACE_1_HOVER,
                edge: Edge::Plain,
                shadow: Some(SHADOW_LIFT),
                radius: Radius::CARD,
                light: LightModel::Raised,
            },
        }
    }

    /// The colour text will actually sit on, worst case, for contrast checks:
    /// the darkest point of the fill is not always the worst case, so both
    /// ends get checked by the caller.
    pub fn fill_extremes(self) -> (Color, Color) {
        let g = self.material().fill;
        (g.first(), g.last())
    }

    pub const ALL: [Structural; 4] =
        [Structural::Field, Structural::Rail, Structural::Card, Structural::CardHover];
}

/// A recessed region **inside** an opaque structural surface: list rows, code,
/// logs, input fields, progress troughs. Never a top-level surface, and never
/// a second frosted layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Recessed {
    /// `--well`
    Well,
    /// `--well-deep`
    WellDeep,
    /// An input field: a well with a `--line` border that goes violet on focus.
    Input,
    /// A progress trough.
    Trough,
}

impl Recessed {
    pub fn material(self) -> InsetMaterial {
        match self {
            Recessed::Well => {
                InsetMaterial { fill: tokens::WELL, edge: Edge::None, radius: Radius::SM }
            }
            Recessed::WellDeep => {
                InsetMaterial { fill: tokens::WELL_DEEP, edge: Edge::Line, radius: Radius::SM }
            }
            Recessed::Input => {
                InsetMaterial { fill: tokens::WELL, edge: Edge::Line, radius: Radius::SM }
            }
            Recessed::Trough => {
                InsetMaterial { fill: tokens::WELL_DEEP, edge: Edge::None, radius: Radius::XS }
            }
        }
    }

    pub const ALL: [Recessed; 4] =
        [Recessed::Well, Recessed::WellDeep, Recessed::Input, Recessed::Trough];
}

/// Genuinely floats over content it does not own. The only kind allowed a real
/// backdrop blur, and the only kind that may be translucent at all — within
/// the ≥0.85 body-alpha floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Floating {
    /// App header, floating toolbars.
    Bar,
    /// Modals and slide-overs.
    Sheet,
    /// Context menus.
    Menu,
    /// Toasts. Bottom-right stack.
    Toast,
    /// Tooltips and floating chips.
    Chip,
    /// The speech bubble — the one floating layer that is Wisp's own.
    Bubble,
}

impl Floating {
    pub fn material(self) -> GlassMaterial {
        match self {
            Floating::Bar => GlassMaterial {
                body: tokens::glass::BAR_BODY,
                highlight: tokens::glass::HIGHLIGHT,
                blur: Blur::BAR,
                edge: Edge::TopOnly,
                shadow: SHADOW_BAR,
                radius: Radius::SM,
            },
            Floating::Sheet => GlassMaterial {
                body: tokens::glass::SHEET_BODY,
                highlight: tokens::glass::HIGHLIGHT,
                blur: Blur::SHEET,
                edge: Edge::Lit,
                shadow: SHADOW_SHEET,
                radius: Radius::CARD,
            },
            Floating::Menu => GlassMaterial {
                body: tokens::glass::SHEET_BODY,
                highlight: tokens::glass::HIGHLIGHT,
                blur: Blur::CHIP,
                edge: Edge::Lit,
                shadow: SHADOW_SHEET,
                radius: Radius::SM,
            },
            Floating::Toast => GlassMaterial {
                body: tokens::glass::SHEET_BODY,
                highlight: tokens::glass::HIGHLIGHT,
                blur: Blur::CHIP,
                edge: Edge::Lit,
                shadow: SHADOW,
                radius: Radius::SM,
            },
            Floating::Chip => GlassMaterial {
                body: tokens::glass::CHIP_BODY,
                highlight: tokens::glass::HIGHLIGHT,
                blur: Blur::CHIP,
                edge: Edge::Plain,
                shadow: SHADOW,
                radius: Radius::XS,
            },
            Floating::Bubble => GlassMaterial {
                body: tokens::glass::SHEET_BODY,
                highlight: tokens::glass::HIGHLIGHT,
                blur: Blur::CHIP,
                edge: Edge::Lit,
                shadow: SHADOW,
                radius: Radius::CARD,
            },
        }
    }

    pub const ALL: [Floating; 6] = [
        Floating::Bar,
        Floating::Sheet,
        Floating::Menu,
        Floating::Toast,
        Floating::Chip,
        Floating::Bubble,
    ];
}

/// The three kinds, when a call site has to be generic over them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Surface {
    Structural(Structural),
    Recessed(Recessed),
    Floating(Floating),
}

impl Surface {
    /// Only a floating surface can answer this with `Some`. That is the whole
    /// v1.5 rule, in one function.
    pub fn blur(self) -> Option<Blur> {
        match self {
            Surface::Floating(f) => Some(f.material().blur),
            Surface::Structural(_) | Surface::Recessed(_) => None,
        }
    }

    pub fn radius(self) -> Radius {
        match self {
            Surface::Structural(s) => s.material().radius,
            Surface::Recessed(r) => r.material().radius,
            Surface::Floating(f) => f.material().radius,
        }
    }

    pub fn fill(self) -> Gradient {
        match self {
            Surface::Structural(s) => s.material().fill,
            Surface::Recessed(r) => r.material().fill,
            Surface::Floating(f) => f.material().body,
        }
    }

    pub fn edge(self) -> Edge {
        match self {
            Surface::Structural(s) => s.material().edge,
            Surface::Recessed(r) => r.material().edge,
            Surface::Floating(f) => f.material().edge,
        }
    }

    pub fn validate(self) -> Result<(), ThemeViolation> {
        match self {
            Surface::Structural(s) => s.material().validate(),
            Surface::Recessed(r) => r.material().validate(),
            Surface::Floating(f) => f.material().validate(),
        }
    }
}

impl From<Structural> for Surface {
    fn from(s: Structural) -> Surface {
        Surface::Structural(s)
    }
}
impl From<Recessed> for Surface {
    fn from(r: Recessed) -> Surface {
        Surface::Recessed(r)
    }
}
impl From<Floating> for Surface {
    fn from(f: Floating) -> Surface {
        Surface::Floating(f)
    }
}

/// Everything an operator can actually be looking at, for the sweep tests.
pub fn all_surfaces() -> Vec<Surface> {
    Structural::ALL
        .into_iter()
        .map(Surface::from)
        .chain(Recessed::ALL.into_iter().map(Surface::from))
        .chain(Floating::ALL.into_iter().map(Surface::from))
        .collect()
}

/// What we assume is behind a floating layer when checking contrast: the
/// average of a blurred desktop, not an unblurred white document.
pub const WORST_CASE_BACKDROP: Color = Color::hex(0x808080);

/// The composited colour text will sit on, at `t` along a surface's fill. For
/// a recessed surface this means "over the card it is cut into"; for a
/// floating one, "over the worst thing that could be behind it", which — since
/// the body is ≥0.85 — is nearly the body itself.
pub fn effective_background(surface: Surface, t: f32) -> Color {
    let sample = surface.fill().sample(t);
    match surface {
        Surface::Structural(_) => sample,
        // A well is cut into a card.
        Surface::Recessed(_) => sample.over(tokens::SURFACE_1.sample(t)),
        // A floating layer sits on a *blurred* backdrop, and a 16–34px blur of
        // anything real averages toward a mid tone — pure white is an
        // adversarial case that the blur itself destroys. Mid grey is the
        // honest worst case, and the ≥0.85 body floor does the rest.
        Surface::Floating(_) => sample.over(WORST_CASE_BACKDROP),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typography::{Role, MICRO, SECONDARY};

    #[test]
    fn structural_surfaces_have_no_blur_and_cannot_grow_one() {
        for s in Structural::ALL {
            assert_eq!(Surface::from(s).blur(), None);
        }
        for r in Recessed::ALL {
            assert_eq!(Surface::from(r).blur(), None);
        }
    }

    #[test]
    fn structural_surfaces_are_opaque() {
        for s in Structural::ALL {
            let m = s.material();
            assert!(m.fill.is_opaque(), "{s:?} is structural and must be opaque");
            m.validate().unwrap_or_else(|e| panic!("{s:?}: {e:?}"));
        }
    }

    #[test]
    fn floating_bodies_meet_the_v15_alpha_floor() {
        for f in Floating::ALL {
            let m = f.material();
            m.validate().unwrap_or_else(|e| panic!("{f:?}: {e:?}"));
            assert!(
                m.body.min_alpha() >= MIN_FLOATING_ALPHA,
                "{f:?} body is {} — v1.5 §4 demands ≥{MIN_FLOATING_ALPHA}",
                m.body.min_alpha()
            );
        }
    }

    #[test]
    fn every_floating_surface_actually_floats() {
        // A blur is mandatory on this kind, so the type has already proved it;
        // this pins the *strength* to one of the three that exist.
        for f in Floating::ALL {
            assert!(
                Blur::ALL.contains(&f.material().blur),
                "{f:?} invented a fourth blur strength"
            );
        }
    }

    #[test]
    fn recessed_surfaces_are_shadowed_at_the_lip() {
        for r in Recessed::ALL {
            r.material().validate().unwrap_or_else(|e| panic!("{r:?}: {e:?}"));
        }
    }

    #[test]
    fn every_radius_is_in_the_band() {
        for s in all_surfaces() {
            let px = s.radius().px_u8();
            assert!(
                (Radius::MIN_PX..=Radius::MAX_PX).contains(&px),
                "{s:?} has radius {px}px"
            );
        }
    }

    #[test]
    fn no_edge_is_a_solid_grey_line() {
        for s in all_surfaces() {
            if let Some(g) = s.edge().gradient() {
                for stop in g.stops {
                    // A solid grey would be an opaque neutral. Every edge stop
                    // is either translucent or a brand colour.
                    let c = stop.color;
                    let neutral = c.r == c.g && c.g == c.b;
                    assert!(
                        !(neutral && c.is_opaque()),
                        "{s:?} draws a solid grey line: {c:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn body_text_is_comfortable_on_every_surface_it_can_sit_on() {
        for s in all_surfaces() {
            for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
                let bg = effective_background(s, t);
                let ratio = palette::TEXT.contrast(bg);
                assert!(
                    ratio >= 7.0,
                    "--text on {s:?} at t={t} is only {ratio:.2}:1 (want AAA 7:1) over {bg:?}"
                );
            }
        }
    }

    #[test]
    fn muted_and_micro_text_still_clear_wcag_aa() {
        for s in all_surfaces() {
            for t in [0.0, 0.5, 1.0] {
                let bg = effective_background(s, t);
                for style in [SECONDARY, MICRO] {
                    let ratio = style.color.contrast(bg);
                    assert!(
                        ratio >= 4.5,
                        "{:?} text on {s:?} at t={t} is only {ratio:.2}:1",
                        style.color
                    );
                }
            }
        }
    }

    #[test]
    fn signal_colours_stay_legible_on_a_card() {
        let bg = effective_background(Surface::Structural(Structural::Card), 0.5);
        for (name, c) in [
            ("cyan", palette::CYAN),
            ("amber", palette::AMBER),
            ("danger", palette::DANGER),
        ] {
            let ratio = c.contrast(bg);
            assert!(ratio >= 4.5, "{name} on a card is only {ratio:.2}:1");
        }
    }

    #[test]
    fn text_on_a_primary_violet_button_is_legible() {
        // The one place raw --violet is a background.
        let ratio = palette::TEXT.contrast(palette::VIOLET);
        assert!(ratio >= 4.5, "--text on --violet is {ratio:.2}:1");
    }

    #[test]
    fn the_whole_ramp_is_readable_on_a_card() {
        let bg = effective_background(Surface::Structural(Structural::Card), 0.5);
        for role in Role::ALL {
            let ratio = role.style().color.contrast(bg);
            assert!(ratio >= 4.5, "{role:?} is only {ratio:.2}:1 on a card");
        }
    }

    #[test]
    fn there_are_exactly_three_blur_strengths() {
        assert_eq!(Blur::ALL.len(), 3);
        let mut seen: Vec<f32> = Blur::ALL.iter().map(|b| b.radius_px).collect();
        seen.sort_by(f32::total_cmp);
        assert_eq!(seen, vec![16.0, 22.0, 34.0]);
    }

    #[test]
    fn the_focus_ring_is_violet_and_two_layers_deep() {
        assert_eq!(FOCUS_RING.0.len(), 2);
        assert_eq!(FOCUS_RING.0[0].color.to_hex(), palette::VIOLET.to_hex());
    }

    #[test]
    fn hover_lifts_and_blooms() {
        let rest = Structural::Card.material();
        let hover = Structural::CardHover.material();
        assert!(hover.fill.first().luminance() > rest.fill.first().luminance());
        assert_eq!(hover.shadow, Some(SHADOW_LIFT));
    }
}
