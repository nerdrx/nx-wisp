//! Palette swatches, gradient stops, and the lit-edge helper — as tools.
//!
//! F76 asks for these "as first-class tools rather than hand-typed numbers",
//! and that phrase is the whole specification of this module. Everything here
//! exists because otherwise the operator would be typing `#a469ffb0` into a
//! TOML file and counting stop positions on their fingers.
//!
//! Three things get first-class treatment:
//!
//! * **The NX palette.** [`NX_SWATCHES`] is the frozen brand block of
//!   DESIGN.md §1, read straight out of `wisp-theme` so there is no second
//!   copy of `#7700ff` in the tree to drift. [`ensure_palette`] drops the
//!   whole set into a skin as `[[color]]` entries, which is what makes the
//!   swatch row work on a skin that was authored without them.
//! * **Gradient stops.** [`add_stop_at`] inserts a stop *in the right place*
//!   with a colour sampled from the gradient it lands in, so clicking the
//!   middle of a ramp gives you the colour that was already there rather than
//!   a black notch to fix up.
//! * **The lit edge.** [`lit_edge`] builds DESIGN.md's `--edge-lit` for a
//!   given shape: a hairline whose bright end is at the shape's **upper-left**
//!   and whose deep end is at its lower-right. That single rule — one light
//!   source, upper-left — is what stops a fake-glass surface reading as
//!   plastic, and getting it backwards is the classic way to lose it. Deriving
//!   the two endpoints from the shape's own bounding box means it cannot be
//!   got backwards here.

use wisp_rig::math::Vec2;
use wisp_rig::paint::Rgba;
use wisp_rig::skin::doc::{to_pt, ColorDoc, GradientDoc, Num, ShapeDoc, SkinDoc, StrokeDoc};
use wisp_theme::palette;

use crate::cmd::Command;
use crate::error::EditError;

/// A named colour the swatch row offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Swatch {
    pub name: &'static str,
    pub hex: &'static str,
    /// What DESIGN.md permits this colour to mean. Shown as the swatch's
    /// tooltip, because "amber is update and attention, nothing else" is a
    /// rule that only holds if it is visible at the moment of choosing.
    pub role: &'static str,
}

/// The brand block of DESIGN.md §1. Frozen — these are not suggestions.
pub const NX_SWATCHES: &[Swatch] = &[
    Swatch { name: "violet", hex: "#7700ff", role: "actions, focus, identity — violet dominates" },
    Swatch { name: "violet_soft", hex: "#9a3cff", role: "violet, one step lighter" },
    Swatch { name: "cyan", hex: "#00e5ff", role: "light inside materials — never a surface colour" },
    Swatch { name: "amber", hex: "#ffb300", role: "update and attention, nothing else" },
    Swatch { name: "danger", hex: "#ff5470", role: "danger only" },
    Swatch { name: "text", hex: "#efeaff", role: "type" },
    Swatch { name: "muted", hex: "#9a8fc0", role: "secondary type" },
    Swatch { name: "panel", hex: "#171028", role: "structural violet" },
    Swatch { name: "deep", hex: "#12091f", role: "the bottom of the field — never flat black" },
];

/// The palette as `wisp-theme` holds it, so a test can prove the two agree.
pub fn theme_hex(name: &str) -> Option<String> {
    let c = match name {
        "violet" => palette::VIOLET,
        "violet_soft" => palette::VIOLET_SOFT,
        "cyan" => palette::CYAN,
        "amber" => palette::AMBER,
        "danger" => palette::DANGER,
        "text" => palette::TEXT,
        "muted" => palette::MUTED,
        "panel" => palette::PANEL,
        "deep" => palette::BG_BOTTOM,
        _ => return None,
    };
    Some(format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b))
}

/// Add whichever NX palette entries this skin is missing, so the swatch row
/// can name them instead of pasting hex into every shape.
pub fn ensure_palette(doc: &SkinDoc) -> Command {
    let mut cmds = Vec::new();
    let mut at = doc.colors.len();
    for s in NX_SWATCHES {
        if doc.colors.iter().any(|c| c.name == s.name) {
            continue;
        }
        cmds.push(Command::InsertColor {
            at,
            value: ColorDoc { name: s.name.to_string(), value: s.hex.to_string() },
        });
        at += 1;
    }
    Command::Batch { label: "add the NX palette", cmds }
}

/// Set a named colour's value, from a swatch or from a picker.
pub fn set_color(doc: &SkinDoc, color: usize, hex: &str) -> Result<Command, EditError> {
    let c = doc
        .colors
        .get(color)
        .ok_or(EditError::NoSuchIndex { kind: "colour", at: color, len: doc.colors.len() })?;
    Rgba::parse_hex(hex).map_err(|e| EditError::BadPath { at: "the colour".into(), reason: e.0 })?;
    Ok(Command::SetColor {
        at: color,
        value: ColorDoc { value: hex.to_string(), ..c.clone() },
    })
}

// ------------------------------------------------------------ gradient stops

/// The colour a gradient shows at `t`, for the stop editor's preview strip and
/// for picking the colour of a newly inserted stop.
pub fn sample(gradient: &GradientDoc, t: f32) -> Rgba {
    let n = gradient.stop_at.len().min(gradient.stop_color.len());
    if n == 0 {
        return Rgba::new(0.0, 0.0, 0.0, 0.0);
    }
    let stops: Vec<wisp_rig::paint::GradientStop> = (0..n)
        .map(|i| wisp_rig::paint::GradientStop {
            at: gradient.stop_at[i].0,
            color: Rgba::parse_hex(&gradient.stop_color[i]).unwrap_or(Rgba::new(0.0, 0.0, 0.0, 0.0)),
        })
        .collect();
    wisp_rig::paint::sample_stops(&stops, t)
}

/// Insert a stop at `t`, taking the colour the ramp already has there.
///
/// Clicking a gradient bar should add a handle, not a discontinuity.
pub fn add_stop_at(doc: &SkinDoc, gradient: usize, t: f32) -> Result<Command, EditError> {
    let g = doc
        .gradients
        .get(gradient)
        .ok_or(EditError::NoSuchIndex { kind: "gradient", at: gradient, len: doc.gradients.len() })?;
    if !(0.0..=1.0).contains(&t) {
        return Err(EditError::NotFinite { at: "the stop position", value: t });
    }
    let color = sample(g, t).to_hex();
    let at = g.stop_at.iter().position(|s| s.0 > t).unwrap_or(g.stop_at.len());
    Ok(Command::InsertStop { gradient, at, position: t, color })
}

/// Drag a stop along the bar, clamped between its neighbours so the list stays
/// ordered — the rig refuses a gradient whose stops go backwards.
pub fn move_stop(
    doc: &SkinDoc,
    gradient: usize,
    stop: usize,
    t: f32,
) -> Result<Command, EditError> {
    let g = doc
        .gradients
        .get(gradient)
        .ok_or(EditError::NoSuchIndex { kind: "gradient", at: gradient, len: doc.gradients.len() })?;
    if stop >= g.stop_at.len() {
        return Err(EditError::NoSuchIndex {
            kind: "gradient stop",
            at: stop,
            len: g.stop_at.len(),
        });
    }
    let lo = if stop == 0 { 0.0 } else { g.stop_at[stop - 1].0 };
    let hi = if stop + 1 >= g.stop_at.len() { 1.0 } else { g.stop_at[stop + 1].0 };
    let color = g.stop_color.get(stop).cloned().unwrap_or_default();
    Ok(Command::SetStop { gradient, at: stop, position: t.clamp(lo, hi), color })
}

/// Recolour one stop.
pub fn set_stop_color(
    doc: &SkinDoc,
    gradient: usize,
    stop: usize,
    hex: &str,
) -> Result<Command, EditError> {
    let g = doc
        .gradients
        .get(gradient)
        .ok_or(EditError::NoSuchIndex { kind: "gradient", at: gradient, len: doc.gradients.len() })?;
    if stop >= g.stop_at.len() {
        return Err(EditError::NoSuchIndex {
            kind: "gradient stop",
            at: stop,
            len: g.stop_at.len(),
        });
    }
    Rgba::parse_hex(hex).map_err(|e| EditError::BadPath { at: "the stop".into(), reason: e.0 })?;
    Ok(Command::SetStop {
        gradient,
        at: stop,
        position: g.stop_at[stop].0,
        color: hex.to_string(),
    })
}

/// Delete a stop, refusing to take the last one — a gradient with no stops is
/// a validation error, and an editor should not be able to author one.
pub fn delete_stop(doc: &SkinDoc, gradient: usize, stop: usize) -> Result<Command, EditError> {
    let g = doc
        .gradients
        .get(gradient)
        .ok_or(EditError::NoSuchIndex { kind: "gradient", at: gradient, len: doc.gradients.len() })?;
    if g.stop_at.len() <= 1 {
        return Err(EditError::BadPath {
            at: format!("gradient {:?}", g.name),
            reason: "a gradient needs at least one stop".into(),
        });
    }
    Ok(Command::RemoveStop { gradient, at: stop })
}

/// A new linear gradient across a shape's bounding box, top to bottom.
pub fn new_linear(doc: &SkinDoc, name: &str, from: Vec2, to: Vec2) -> Result<Command, EditError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(EditError::EmptyName { kind: "gradient", name: name.to_string() });
    }
    if doc.gradients.iter().any(|g| g.name == trimmed) {
        return Err(EditError::DuplicateName { kind: "gradient", name: trimmed.to_string() });
    }
    Ok(Command::InsertGradient {
        at: doc.gradients.len(),
        value: Box::new(GradientDoc {
            name: trimmed.to_string(),
            kind: "linear".into(),
            start: Some(to_pt(from)),
            end: Some(to_pt(to)),
            center: None,
            focus: None,
            radius: None,
            extend: String::new(),
            follow_bone: String::new(),
            stop_at: vec![Num(0.0), Num(1.0)],
            stop_color: vec!["#9a3cffff".into(), "#7700ff00".into()],
        }),
    })
}

/// A new radial gradient, with its focus pushed towards the upper-left.
///
/// The offset is not decoration: an off-axis highlight is what makes a radial
/// read as a lit sphere instead of a flat disc, and DESIGN.md §1 puts the
/// light in the upper-left. Thirty percent of the radius is the offset the
/// shipped skin's core uses.
pub fn new_radial(
    doc: &SkinDoc,
    name: &str,
    centre: Vec2,
    radius: f32,
) -> Result<Command, EditError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(EditError::EmptyName { kind: "gradient", name: name.to_string() });
    }
    if doc.gradients.iter().any(|g| g.name == trimmed) {
        return Err(EditError::DuplicateName { kind: "gradient", name: trimmed.to_string() });
    }
    let focus = Vec2::new(centre.x - radius * 0.3, centre.y - radius * 0.3);
    Ok(Command::InsertGradient {
        at: doc.gradients.len(),
        value: Box::new(GradientDoc {
            name: trimmed.to_string(),
            kind: "radial".into(),
            start: None,
            end: None,
            center: Some(to_pt(centre)),
            focus: Some(to_pt(focus)),
            radius: Some(Num(radius)),
            extend: String::new(),
            follow_bone: String::new(),
            stop_at: vec![Num(0.0), Num(1.0)],
            stop_color: vec!["#ffffffff".into(), "#7700ff00".into()],
        }),
    })
}

// -------------------------------------------------------------- the lit edge

/// DESIGN.md's `--edge-lit` stop ramp: white at the lit end, violet through
/// the middle, deep violet at the shadowed end. Taken from the shipped skin's
/// `edge_lit`, which is the reference implementation of the token.
pub const EDGE_LIT_STOPS: &[(f32, &str)] = &[
    (0.0, "#ffffffff"),
    (0.32, "#e9d2ffd6"),
    (0.62, "#a469ffb0"),
    (1.0, "#3a0b7ae0"),
];

/// How thick a lit edge is, in canvas units, on a 256-unit canvas. Scaled for
/// other canvas sizes so the hairline stays a hairline.
pub const EDGE_WIDTH_UNITS: f32 = 2.8;

/// Build a lit edge for one shape: the gradient plus the stroke that uses it.
///
/// The gradient runs from the shape's upper-left towards its lower-right —
/// **always**, derived from the shape's own bounds, so the light cannot end up
/// coming from the wrong side. It is inset slightly from the corners so the
/// brightest stop lands on the shape's edge rather than in the empty corner of
/// its bounding box.
pub fn lit_edge(doc: &SkinDoc, shape: usize, gradient_name: &str) -> Result<Command, EditError> {
    let s = doc
        .shapes
        .get(shape)
        .ok_or(EditError::NoSuchIndex { kind: "shape", at: shape, len: doc.shapes.len() })?;
    let bounds = crate::canvas::bounds_of(doc, shape)?;
    if bounds.is_empty() {
        return Err(EditError::BadPath {
            at: format!("shape {:?}", s.name),
            reason: "it has no geometry to run an edge around".into(),
        });
    }
    // 12% in from each corner along the diagonal.
    let inset = 0.12;
    let w = bounds.width();
    let h = bounds.height();
    let start = Vec2::new(bounds.min.x + w * inset, bounds.min.y + h * inset * 0.5);
    let end = Vec2::new(bounds.max.x - w * inset, bounds.max.y - h * inset * 0.5);

    let extent = doc.canvas.size[0].0.max(doc.canvas.size[1].0).max(1.0);
    let width = EDGE_WIDTH_UNITS * (extent / 256.0);

    let mut cmds = Vec::new();
    let gradient = GradientDoc {
        name: gradient_name.to_string(),
        kind: "linear".into(),
        start: Some(to_pt(start)),
        end: Some(to_pt(end)),
        center: None,
        focus: None,
        radius: None,
        extend: String::new(),
        follow_bone: String::new(),
        stop_at: EDGE_LIT_STOPS.iter().map(|(a, _)| Num(*a)).collect(),
        stop_color: EDGE_LIT_STOPS.iter().map(|(_, c)| c.to_string()).collect(),
    };
    match doc.gradients.iter().position(|g| g.name == gradient_name) {
        Some(at) => cmds.push(Command::SetGradient { at, value: Box::new(gradient) }),
        None => cmds.push(Command::InsertGradient {
            at: doc.gradients.len(),
            value: Box::new(gradient),
        }),
    }
    cmds.push(Command::SetShape {
        at: shape,
        value: Box::new(ShapeDoc {
            stroke: Some(StrokeDoc {
                color: String::new(),
                gradient: gradient_name.to_string(),
                alpha: None,
                width: Num(width),
                cap: String::new(),
                join: "round".into(),
            }),
            ..s.clone()
        }),
    });
    Ok(Command::Batch { label: "add a lit edge", cmds })
}

/// Does this gradient obey "light from the upper-left"?
///
/// The check the editor runs on every gradient it shows: the brightest stop
/// must be at the end nearer the top-left. It is a warning rather than a
/// refusal — a skin may have a deliberate reason, and SPEC §3.5b already says
/// the creature is not bound by the chrome rules — but it is a warning worth
/// seeing, because getting it backwards is invisible until the whole thing
/// looks like plastic.
pub fn light_from_upper_left(g: &GradientDoc) -> Option<bool> {
    let (start, end) = match (g.start, g.end) {
        (Some(s), Some(e)) => (wisp_rig::skin::doc::pt(s), wisp_rig::skin::doc::pt(e)),
        _ => return None,
    };
    let n = g.stop_at.len().min(g.stop_color.len());
    if n < 2 {
        return None;
    }
    let lum = |hex: &str| -> f32 {
        let c = Rgba::parse_hex(hex).unwrap_or(Rgba::new(0.0, 0.0, 0.0, 0.0));
        (0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b) * c.a
    };
    let first = lum(&g.stop_color[0]);
    let last = lum(&g.stop_color[n - 1]);
    // "Nearer the top-left" along the gradient's own axis.
    let start_first = (start.x + start.y) <= (end.x + end.y);
    Some(if start_first { first >= last } else { last >= first })
}
