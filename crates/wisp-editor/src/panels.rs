//! The editor's chrome: the panels, and what clicking them means.
//!
//! # It is chrome, so it obeys the geometry rule
//!
//! SPEC §3.5b is explicit that the rig editor is chrome. Every surface here is
//! opaque and structural, every radius is 3–6px, there are no pills, and the
//! only circles are the two DESIGN.md sanctions. Glass appears nowhere: none
//! of these panels floats over anything, so none of them may be translucent
//! (DESIGN.md v1.5 §4). The one round thing on screen is the character, and
//! she is drawn by [`crate::overlay`], not by this module.
//!
//! # Built on the widget layer, not beside it
//!
//! `wisp_paint::widget::Ui` already has surfaces, stacks with Fixed/Hug/Fill,
//! scrolling, hit-testing that respects scroll clips, and press/release click
//! detection. This module builds trees out of it and does not add a second
//! widget system. The two things it does add — the timeline and the graph —
//! are drawn straight into a [`Scene`] because they are *canvases*, not
//! stacks of boxes, and expressing a keyframe grid as widget nodes would be a
//! worse description of it, not a better one.
//!
//! # Clicks come back as intentions
//!
//! [`Panels::build`] records a `NodeId → `[`Action`] map as it builds. The
//! editor asks [`Panels::click`] what a press meant and gets `SelectTool`,
//! `Undo`, `PickSwatch(3)` — never a node id. So the editor's input routing
//! has no idea what the layout looks like, and rearranging a panel cannot
//! break a command.

use wisp_paint::geom::{Path as PPath, Point, Rect};
use wisp_paint::paint::Paint as PPaint;
use wisp_paint::scene::Scene;
use wisp_paint::text::TextEngine;
use wisp_paint::widget::{self, Align, Axis, NodeId, Size, Sizing, Ui, Widget};
use wisp_theme::component::{ButtonState, ButtonVariant};
use wisp_theme::surface::{Recessed, Structural};
use wisp_theme::typography::Role;
use wisp_theme::{palette, Color, Insets, Radius, Space, Surface};

use crate::graph::MoodGraph;
use crate::select::{Target, Tool};
use crate::text::TextSink;
use crate::timeline::TimelineState;

/// What a click on the chrome meant.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    SelectTool(Tool),
    Select(Target),
    Undo,
    Redo,
    Save,
    FitView,
    ZoomIn,
    ZoomOut,
    PlayPause,
    ToggleOnion,
    ToggleGraph,
    /// A swatch from the NX palette row.
    PickSwatch(usize),
    /// Give the selected shape a lit edge.
    AddLitEdge,
    AddBone,
    AddShape,
    DeleteSelected,
    /// Jump the playhead, in milliseconds.
    Scrub(f32),
}

/// Panel geometry, so the canvas and the timeline know where they are.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frames {
    pub toolbar: Rect,
    pub left: Rect,
    pub canvas: Rect,
    pub right: Rect,
    pub timeline: Rect,
    pub status: Rect,
}

pub const TOOLBAR_H: f32 = 40.0;
pub const STATUS_H: f32 = 28.0;
pub const TIMELINE_H: f32 = 184.0;
pub const SIDE_W: f32 = 232.0;
/// The timeline's name gutter, left of the ruler.
pub const GUTTER_W: f32 = 148.0;
/// One track row.
pub const ROW_H: f32 = 22.0;

/// Split a rectangle into the editor's regions. Total: every region is inside
/// `bounds`, and a window too small for the side panels drops them rather than
/// producing negative widths.
pub fn frames(bounds: Rect) -> Frames {
    let toolbar = Rect::new(bounds.x, bounds.y, bounds.w, TOOLBAR_H.min(bounds.h));
    let status_h = STATUS_H.min((bounds.h - toolbar.h).max(0.0));
    let status = Rect::new(bounds.x, bounds.bottom() - status_h, bounds.w, status_h);
    let mid_h = (bounds.h - toolbar.h - status_h).max(0.0);
    let timeline_h = TIMELINE_H.min(mid_h * 0.5);
    let timeline = Rect::new(bounds.x, status.y - timeline_h, bounds.w, timeline_h);
    let body_h = (mid_h - timeline_h).max(0.0);
    let body_y = toolbar.bottom();
    let side = if bounds.w >= SIDE_W * 3.0 { SIDE_W } else { 0.0 };
    let left = Rect::new(bounds.x, body_y, side, body_h);
    let right = Rect::new(bounds.right() - side, body_y, side, body_h);
    let canvas = Rect::new(left.right(), body_y, (bounds.w - side * 2.0).max(0.0), body_h);
    Frames { toolbar, left, canvas, right, timeline, status }
}

/// What the panels need to know about the editor to draw themselves.
///
/// A borrowed view rather than `&Editor`, so a test can build a panel from
/// hand-made state without constructing a whole editor — and so this module
/// cannot reach into the editor and mutate it.
pub struct PanelState<'a> {
    pub doc: &'a wisp_rig::skin::doc::SkinDoc,
    pub selection: &'a crate::select::Selection,
    pub tool: Tool,
    pub timeline: &'a TimelineState,
    pub can_undo: bool,
    pub can_redo: bool,
    pub dirty: bool,
    pub status: &'a str,
    pub problems: usize,
    pub show_graph: bool,
    /// The mood graph, when there is one — the properties well has nothing
    /// useful to say about a selected state without it.
    pub graph: Option<&'a MoodGraph>,
}

/// The built chrome for one frame.
pub struct Panels {
    pub ui: Ui,
    pub frames: Frames,
    actions: Vec<(NodeId, Action)>,
}

impl Panels {
    /// Build every panel and lay it out. No GPU: `Ui::layout` measures text
    /// through a bare `TextEngine`.
    pub fn build(state: &PanelState<'_>, bounds: Rect, text: &mut TextEngine) -> Panels {
        let frames = frames(bounds);
        let mut ui = Ui::new();
        let mut actions = Vec::new();

        // The whole editor sits on the field, so nothing is see-through.
        let root = ui.add(Widget::Surface(Surface::Structural(Structural::Field)), Size::FILL);
        ui.set_root(root);

        let toolbar = toolbar(&mut ui, state, &mut actions);
        ui.attach(root, toolbar);
        let left = left_panel(&mut ui, state, &mut actions);
        ui.attach(root, left);
        let right = right_panel(&mut ui, state, &mut actions);
        ui.attach(root, right);
        let status = status_strip(&mut ui, state);
        ui.attach(root, status);

        // Each panel is laid out into its own rectangle: the regions are fixed
        // by `frames`, so there is no outer stack to fight with.
        ui.layout(text, bounds);
        arrange(&mut ui, toolbar, frames.toolbar, text);
        arrange(&mut ui, left, frames.left, text);
        arrange(&mut ui, right, frames.right, text);
        arrange(&mut ui, status, frames.status, text);

        Panels { ui, frames, actions }
    }

    /// What a press-and-release at `at` meant.
    pub fn click(&mut self, at: Point, down: bool) -> Option<Action> {
        let hit = self.ui.pointer(Some(at), down)?;
        // Walk up: a click on a row's label is a click on the row.
        let mut node = Some(hit);
        while let Some(n) = node {
            if let Some((_, a)) = self.actions.iter().find(|(id, _)| *id == n) {
                return Some(a.clone());
            }
            node = self.ui.parent(n);
        }
        None
    }

    /// Move the pointer without pressing, so hover states update.
    pub fn hover(&mut self, at: Option<Point>) {
        self.ui.pointer(at, false);
    }

    /// The action bound to a node, for tests that want to click by intent.
    pub fn node_for(&self, action: &Action) -> Option<NodeId> {
        self.actions.iter().find(|(_, a)| a == action).map(|(id, _)| *id)
    }

    pub fn actions(&self) -> impl Iterator<Item = (NodeId, &Action)> {
        self.actions.iter().map(|(id, a)| (*id, a))
    }
}

/// Re-run layout for one subtree into its own rectangle.
fn arrange(ui: &mut Ui, node: NodeId, rect: Rect, text: &mut TextEngine) {
    let root = ui.root();
    ui.set_root(node);
    ui.layout(text, rect);
    if let Some(r) = root {
        ui.set_root(r);
    }
}

// ------------------------------------------------------------------ toolbar

fn toolbar(ui: &mut Ui, state: &PanelState<'_>, actions: &mut Vec<(NodeId, Action)>) -> NodeId {
    let bar = ui.add(Widget::Surface(Surface::Structural(Structural::Rail)), Size::FILL);
    let row = ui.add(
        Widget::Stack {
            axis: Axis::Row,
            gap: Space::S1,
            padding: Insets::xy(Space::S2, Space::ZERO),
            align: Align::Centre,
        },
        Size::FILL,
    );
    ui.attach(bar, row);

    for tool in Tool::ALL {
        let id = ui.add_to(
            row,
            Widget::Button {
                label: tool.name().to_string(),
                variant: if state.tool == tool {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                state: ButtonState::default(),
            },
            Size::HUG,
        );
        actions.push((id, Action::SelectTool(tool)));
    }

    ui.add_to(row, Widget::Spacer, Size::FILL);

    let mut chip = |ui: &mut Ui, label: &str, action: Action, enabled: bool| {
        let id = ui.add_to(
            row,
            Widget::Button {
                label: label.to_string(),
                variant: ButtonVariant::Secondary,
                state: ButtonState { disabled: !enabled, ..Default::default() },
            },
            Size::HUG,
        );
        actions.push((id, action));
    };
    chip(
        ui,
        if state.timeline.playing { "pause" } else { "play" },
        Action::PlayPause,
        !state.doc.clips.is_empty(),
    );
    chip(
        ui,
        if state.timeline.onion.enabled { "onion •" } else { "onion" },
        Action::ToggleOnion,
        !state.doc.clips.is_empty(),
    );
    chip(ui, "undo", Action::Undo, state.can_undo);
    chip(ui, "redo", Action::Redo, state.can_redo);
    chip(ui, "fit", Action::FitView, true);
    chip(ui, "graph", Action::ToggleGraph, true);
    chip(
        ui,
        if state.dirty { "save •" } else { "save" },
        Action::Save,
        true,
    );
    bar
}

// --------------------------------------------------------------- left panel

fn left_panel(ui: &mut Ui, state: &PanelState<'_>, actions: &mut Vec<(NodeId, Action)>) -> NodeId {
    let panel = ui.add(Widget::Surface(Surface::Structural(Structural::Rail)), Size::FILL);
    let col = ui.add(
        Widget::Stack {
            axis: Axis::Column,
            gap: Space::S1,
            padding: Insets::CONTENT,
            align: Align::Stretch,
        },
        Size::FILL,
    );
    ui.attach(panel, col);

    widget::label(ui, col, "Bones", Role::TitleSmall);
    let bones_scroll = ui.add_to(col, Widget::Scroll { offset: 0.0 }, Size::FILL);
    let bones = ui.add_to(
        bones_scroll,
        Widget::Stack {
            axis: Axis::Column,
            gap: Space::ZERO,
            padding: Insets::ZERO,
            align: Align::Stretch,
        },
        Size::new(Sizing::Fill, Sizing::Hug),
    );
    for row in crate::bones::tree_rows(state.doc) {
        let b = &state.doc.bones[row.bone];
        let selected = state.selection.contains(Target::Bone(row.bone));
        let id = tree_row(ui, bones, &b.name, row.depth, selected);
        actions.push((id, Action::Select(Target::Bone(row.bone))));
    }
    let add_bone = ui.add_to(
        col,
        Widget::Button {
            label: "add bone".into(),
            variant: ButtonVariant::Secondary,
            state: ButtonState::default(),
        },
        Size::HUG,
    );
    actions.push((add_bone, Action::AddBone));

    ui.add_to(col, Widget::Divider, Size::new(Sizing::Fill, Sizing::Fixed(1.0)));

    widget::label(ui, col, "Shapes", Role::TitleSmall);
    let shapes_scroll = ui.add_to(col, Widget::Scroll { offset: 0.0 }, Size::FILL);
    let shapes = ui.add_to(
        shapes_scroll,
        Widget::Stack {
            axis: Axis::Column,
            gap: Space::ZERO,
            padding: Insets::ZERO,
            align: Align::Stretch,
        },
        Size::new(Sizing::Fill, Sizing::Hug),
    );
    // Paint order, top of the list is what draws last — the way an artist
    // reads a layer stack.
    let mut order: Vec<usize> = (0..state.doc.shapes.len()).collect();
    order.sort_by(|a, b| state.doc.shapes[*b].z.cmp(&state.doc.shapes[*a].z).then(b.cmp(a)));
    for i in order {
        let selected = state.selection.contains(Target::Shape(i));
        let id = tree_row(ui, shapes, &state.doc.shapes[i].name, 0, selected);
        actions.push((id, Action::Select(Target::Shape(i))));
    }
    let add_shape = ui.add_to(
        col,
        Widget::Button {
            label: "add shape".into(),
            variant: ButtonVariant::Secondary,
            state: ButtonState::default(),
        },
        Size::HUG,
    );
    actions.push((add_shape, Action::AddShape));

    panel
}

/// One row of a tree: indent, name, and a selection wash behind it.
fn tree_row(ui: &mut Ui, parent: NodeId, name: &str, depth: usize, selected: bool) -> NodeId {
    let row = ui.add_to(
        parent,
        Widget::Fill {
            paint: PPaint::Solid(if selected {
                palette::VIOLET.with_alpha(0.28)
            } else {
                Color::rgba(0, 0, 0, 0)
            }),
            radius: Radius::XS,
        },
        Size::new(Sizing::Fill, Sizing::Fixed(ROW_H)),
    );
    let inner = ui.add_to(
        row,
        Widget::Stack {
            axis: Axis::Row,
            gap: Space::ZERO,
            padding: Insets::xy(Space::S1, Space::ZERO),
            align: Align::Centre,
        },
        Size::FILL,
    );
    if depth > 0 {
        ui.add_to(
            inner,
            Widget::Spacer,
            Size::new(Sizing::Fixed(depth as f32 * 12.0), Sizing::Fixed(1.0)),
        );
    }
    ui.add_to(
        inner,
        Widget::Text {
            text: name.to_string(),
            role: Role::Body,
            color: Some(if selected { palette::TEXT } else { palette::MUTED }),
            wrap: false,
        },
        Size::new(Sizing::Fill, Sizing::Hug),
    );
    row
}

// -------------------------------------------------------------- right panel

fn right_panel(ui: &mut Ui, state: &PanelState<'_>, actions: &mut Vec<(NodeId, Action)>) -> NodeId {
    let panel = ui.add(Widget::Surface(Surface::Structural(Structural::Rail)), Size::FILL);
    let col = ui.add(
        Widget::Stack {
            axis: Axis::Column,
            gap: Space::S2,
            padding: Insets::CONTENT,
            align: Align::Stretch,
        },
        Size::FILL,
    );
    ui.attach(panel, col);

    widget::label(ui, col, "Palette", Role::TitleSmall);
    let swatches = ui.add_to(
        col,
        Widget::Stack {
            axis: Axis::Row,
            gap: Space::ZERO,
            padding: Insets::ZERO,
            align: Align::Start,
        },
        Size::new(Sizing::Fill, Sizing::Fixed(24.0)),
    );
    for (i, s) in crate::swatch::NX_SWATCHES.iter().enumerate() {
        let c = wisp_rig::paint::Rgba::parse_hex(s.hex).unwrap_or(wisp_rig::paint::Rgba::WHITE);
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        let id = ui.add_to(
            swatches,
            Widget::Fill {
                paint: PPaint::Solid(Color::rgba(q(c.r), q(c.g), q(c.b), 255)),
                radius: Radius::XS,
            },
            Size::new(Sizing::Fill, Sizing::Fill),
        );
        actions.push((id, Action::PickSwatch(i)));
    }

    let lit = ui.add_to(
        col,
        Widget::Button {
            label: "lit edge".into(),
            variant: ButtonVariant::Secondary,
            state: ButtonState {
                disabled: state.selection.iter().all(|t| t.owning_shape().is_none()),
                ..Default::default()
            },
        },
        Size::HUG,
    );
    actions.push((lit, Action::AddLitEdge));

    ui.add_to(col, Widget::Divider, Size::new(Sizing::Fill, Sizing::Fixed(1.0)));

    widget::label(ui, col, "Selection", Role::TitleSmall);
    let well = widget::well(ui, Size::new(Sizing::Fill, Sizing::Fill));
    ui.attach(col, ui.parent(well).expect("well returns its content stack"));
    for line in describe_selection(state) {
        widget::label(ui, well, line, Role::Secondary);
    }

    let del = ui.add_to(
        col,
        Widget::Button {
            label: "delete".into(),
            variant: ButtonVariant::Danger,
            state: ButtonState {
                disabled: state.selection.is_empty(),
                ..Default::default()
            },
        },
        Size::HUG,
    );
    actions.push((del, Action::DeleteSelected));

    panel
}

/// What the properties well says about the current selection.
pub fn describe_selection(state: &PanelState<'_>) -> Vec<String> {
    let doc = state.doc;
    let Some(anchor) = state.selection.anchor() else {
        return vec!["nothing selected".to_string()];
    };
    let mut out = Vec::new();
    if state.selection.len() > 1 {
        out.push(format!("{} things selected", state.selection.len()));
    }
    match anchor {
        Target::Shape(i) => {
            if let Some(s) = doc.shapes.get(i) {
                out.push(format!("shape {:?}", s.name));
                out.push(format!("z {}", s.z));
                out.push(format!(
                    "bound to {}",
                    if s.bind.is_empty() {
                        s.bind_auto
                            .as_ref()
                            .map(|a| format!("{} bones by distance", a.bones.len()))
                            .unwrap_or_else(|| "nothing".into())
                    } else {
                        s.bind.clone()
                    }
                ));
                if let Some(f) = &s.fill {
                    out.push(if f.gradient.is_empty() {
                        format!("fill {}", f.color)
                    } else {
                        format!("fill gradient {}", f.gradient)
                    });
                }
                if s.stroke.is_some() {
                    out.push("has a lit edge".to_string());
                }
                if let Ok(p) = crate::canvas::path_of(doc, i) {
                    out.push(format!("{} points", p.points.len()));
                }
            }
        }
        Target::Point { shape, point } => {
            out.push(format!("point {point}"));
            if let Ok(p) = crate::canvas::path_of(doc, shape) {
                if let Some(v) = p.points.get(point) {
                    out.push(format!("at {:.1}, {:.1}", v.x, v.y));
                }
            }
            if let Some(s) = doc.shapes.get(shape) {
                out.push(format!("of {:?}", s.name));
                if let Some(w) = s.weights.iter().find(|w| w.point == point) {
                    for (b, v) in w.bones.iter().zip(&w.weights) {
                        out.push(format!("{b} {:.2}", v.0));
                    }
                }
            }
        }
        Target::Bone(i) => {
            if let Some(b) = doc.bones.get(i) {
                out.push(format!("bone {:?}", b.name));
                out.push(format!(
                    "parent {}",
                    if b.parent.is_empty() { "none (a root)" } else { &b.parent }
                ));
                out.push(format!("at {:.1}, {:.1}", b.pos[0].0, b.pos[1].0));
                if let Some(r) = b.rot {
                    out.push(format!("rotated {:.1}°", r.0));
                }
                if let Some(l) = b.length {
                    out.push(format!("length {:.1}", l.0));
                }
            }
        }
        Target::Clip(i) => {
            if let Some(c) = doc.clips.get(i) {
                out.push(format!("clip {:?}", c.name));
                out.push(format!("{:.0} ms", c.duration_ms.0));
                out.push(if c.looping { "loops".into() } else { "plays once".to_string() });
                out.push(format!("{} tracks", c.tracks.len()));
            }
        }
        Target::Key { clip, track, key } => {
            out.push("keyframe".to_string());
            if let Some(t) = doc.clips.get(clip).and_then(|c| c.tracks.get(track)) {
                out.push(format!("{} {}", t.bone, t.channel));
                if let (Some(tt), Some(vv)) = (t.t.get(key), t.v.get(key)) {
                    out.push(format!("{:.0} ms = {:.3}", tt.0, vv.0));
                }
            }
        }
        Target::State(i) => match state.graph.and_then(|g| g.states.get(i)) {
            Some(s) => {
                out.push(format!("state {:?}", s.name));
                out.push(format!("behaviour {}", if s.behaviour.is_empty() { "none" } else { &s.behaviour }));
                if !s.clip.is_empty() {
                    out.push(format!("plays {}", s.clip));
                }
                if !s.expression.is_empty() {
                    out.push(format!("expression {}", s.expression));
                }
                if s.initial {
                    out.push("the entry point".to_string());
                }
                if let Some(g) = state.graph {
                    let out_edges = g.transitions.iter().filter(|t| t.from == s.name).count();
                    let in_edges = g.transitions.iter().filter(|t| t.to == s.name).count();
                    out.push(format!("{in_edges} in, {out_edges} out"));
                }
            }
            None => out.push("state selected".to_string()),
        },
        Target::Transition(i) => match state.graph.and_then(|g| g.transitions.get(i)) {
            Some(t) => {
                out.push(format!("{} -> {}", t.from, t.to));
                if !t.on.is_empty() {
                    out.push(format!("on {}", t.on));
                }
                if let Some(ms) = t.after_ms {
                    out.push(format!("after {:.1} s", ms / 1000.0));
                }
            }
            None => out.push("transition selected".to_string()),
        },
        other => out.push(format!("{} selected", other.kind())),
    }
    out
}

// -------------------------------------------------------------------- status

fn status_strip(ui: &mut Ui, state: &PanelState<'_>) -> NodeId {
    let bar = ui.add(Widget::Surface(Surface::Structural(Structural::Rail)), Size::FILL);
    let row = ui.add(
        Widget::Stack {
            axis: Axis::Row,
            gap: Space::S2,
            padding: Insets::xy(Space::S2, Space::ZERO),
            align: Align::Centre,
        },
        Size::FILL,
    );
    ui.attach(bar, row);
    ui.add_to(
        row,
        Widget::StatusDot(if state.problems > 0 {
            wisp_theme::component::Status::Failed
        } else if state.dirty {
            wisp_theme::component::Status::Pending
        } else {
            wisp_theme::component::Status::Live
        }),
        Size::HUG,
    );
    ui.add_to(
        row,
        Widget::Text {
            text: state.status.to_string(),
            role: Role::Secondary,
            color: Some(if state.problems > 0 { palette::DANGER } else { palette::MUTED }),
            wrap: false,
        },
        Size::new(Sizing::Fill, Sizing::Hug),
    );
    ui.add_to(
        row,
        Widget::Text {
            text: state.tool.hint().to_string(),
            role: Role::Micro,
            color: Some(palette::MUTED),
            wrap: false,
        },
        Size::HUG,
    );
    bar
}

// ------------------------------------------------------------------ timeline

/// Draw the timeline into `rect`. Keyframes, the ruler, the playhead.
///
/// Returns the clickable keyframe boxes so the editor can hit-test them
/// without re-deriving the layout — the same trick as `Panels::actions`, for
/// a surface that is not made of widgets.
pub fn draw_timeline(
    doc: &wisp_rig::skin::doc::SkinDoc,
    tl: &TimelineState,
    rect: Rect,
    sink: &mut impl TextSink,
    scene: &mut Scene,
) -> Vec<(Rect, Target)> {
    let mut hits = Vec::new();
    scene.structural(rect, Structural::Card);
    let Some(clip) = doc.clips.get(tl.clip) else {
        let style = Role::Secondary.style().with_color(palette::MUTED);
        if let Some(run) = sink.run("this skin has no clips yet", &style, None) {
            scene.text(run.rect_at(rect.x + 16.0, rect.y + 16.0), run.tex.clone(), style.color);
        }
        return hits;
    };
    let ruler_x = rect.x + GUTTER_W;
    let ruler = Rect::new(ruler_x, rect.y, (rect.right() - ruler_x).max(0.0), rect.h);
    scene.recessed(ruler, Recessed::Well);

    // Header: the clip's name and length.
    let title = format!("{}  ·  {:.0} ms", clip.name, clip.duration_ms.0);
    let style = Role::BodyStrong.style().with_color(palette::TEXT);
    if let Some(run) = sink.run(&title, &style, None) {
        scene.text(run.rect_at(rect.x + 8.0, rect.y + 5.0), run.tex.clone(), style.color);
    }

    // Ticks every 200ms, labelled every second.
    let tick = 200.0f32;
    let mut t = (tl.scroll_ms / tick).floor() * tick;
    let micro = Role::Micro.style().with_color(palette::MUTED);
    while t <= tl.scroll_ms + ruler.w / tl.scale_px_per_ms.max(1e-6) {
        let x = tl.time_to_px(t, ruler_x);
        if x >= ruler_x && x <= ruler.right() {
            let full = (t % 1000.0).abs() < 0.5;
            let h = if full { 10.0 } else { 5.0 };
            scene.stroke(
                PPath::build(|pb| {
                    pb.move_to(x, rect.y + 4.0).line_to(x, rect.y + 4.0 + h);
                }),
                PPaint::Solid(palette::LINE),
                1.0,
            );
            if full {
                let label = format!("{:.0}s", t / 1000.0);
                if let Some(run) = sink.run(&label, &micro, None) {
                    scene.text(run.rect_at(x + 3.0, rect.y + 2.0), run.tex.clone(), micro.color);
                }
            }
        }
        t += tick;
    }

    // One row per (bone, channel).
    let mut y = rect.y + 22.0;
    for row in crate::timeline::rows(doc, tl.clip) {
        if y + ROW_H > rect.bottom() {
            break;
        }
        let name = Role::Secondary.style().with_color(palette::TEXT);
        if let Some(run) = sink.run(&row.bone, &name, None) {
            scene.text(run.rect_at(rect.x + 8.0, y + 3.0), run.tex.clone(), name.color);
        }
        for &track in &row.tracks {
            if y + ROW_H > rect.bottom() {
                break;
            }
            let Some(tr) = clip.tracks.get(track) else { continue };
            if let Some(run) = sink.run(&tr.channel, &micro, None) {
                scene.text(
                    run.rect_at(rect.x + 8.0 + 84.0, y + 4.0),
                    run.tex.clone(),
                    micro.color,
                );
            }
            // The lane.
            scene.fill_rect(
                Rect::new(ruler_x, y + ROW_H * 0.5 - 1.0, ruler.w, 2.0),
                Radius::XS,
                PPaint::Solid(palette::LINE.with_alpha(0.5)),
            );
            for (k, kt) in tr.t.iter().enumerate() {
                let x = tl.time_to_px(kt.0, ruler_x);
                if x < ruler_x - 6.0 || x > ruler.right() + 6.0 {
                    continue;
                }
                let box_r = Rect::new(x - 4.0, y + ROW_H * 0.5 - 4.0, 8.0, 8.0);
                scene.fill_rect(box_r, Radius::XS, PPaint::Solid(palette::CYAN));
                hits.push((
                    box_r.inset(-3.0),
                    Target::Key { clip: tl.clip, track, key: k },
                ));
            }
            y += ROW_H;
        }
        y += 2.0;
    }

    // The playhead, last, over everything.
    let px = tl.time_to_px(tl.playhead_ms, ruler_x);
    if px >= ruler_x && px <= ruler.right() {
        scene.stroke(
            PPath::build(|pb| {
                pb.move_to(px, rect.y).line_to(px, rect.bottom());
            }),
            PPaint::Solid(palette::AMBER),
            1.0,
        );
    }
    hits
}

// --------------------------------------------------------------------- graph

/// Draw the state-machine view: nodes, edges, and what fires each one.
///
/// `origin` is the graph-space point shown at the rectangle's centre, and
/// `zoom` its scale, so the graph pans and zooms with the same arithmetic as
/// the canvas without borrowing its viewport.
pub fn draw_graph(
    g: &MoodGraph,
    rect: Rect,
    origin: (f32, f32),
    zoom: f32,
    selection: &crate::select::Selection,
    sink: &mut impl TextSink,
    scene: &mut Scene,
) -> Vec<(Rect, Target)> {
    let mut hits = Vec::new();
    scene.structural(rect, Structural::Card);
    let to_screen = |x: f32, y: f32| {
        Point::new(
            rect.x + rect.w * 0.5 + (x - origin.0) * zoom,
            rect.y + rect.h * 0.5 + (y - origin.1) * zoom,
        )
    };

    // Edges first, so a node's box covers the line that ends under it.
    let micro = Role::Micro.style().with_color(palette::MUTED);
    for t in &g.transitions {
        let Some((a, b)) = crate::graph::edge_ends(g, t) else { continue };
        let mut p = to_screen(a.0, a.1);
        let mut q = to_screen(b.0, b.1);
        // Two states that point at each other would otherwise draw one line
        // with two arrowheads and one unreadable label. Nudge each edge off
        // the centre line so a pair reads as a pair.
        if g.transitions.iter().any(|o| o.from == t.to && o.to == t.from) {
            let (dx, dy) = (q.x - p.x, q.y - p.y);
            let len = (dx * dx + dy * dy).sqrt().max(1e-3);
            let (nx, ny) = (-dy / len * 7.0, dx / len * 7.0);
            p = Point::new(p.x + nx, p.y + ny);
            q = Point::new(q.x + nx, q.y + ny);
        }
        scene.stroke(
            PPath::build(|pb| {
                pb.move_to(p.x, p.y).line_to(q.x, q.y);
            }),
            PPaint::Solid(palette::LINE),
            1.5,
        );
        // An arrowhead: two short strokes, no filled triangle. Cut geometry,
        // like a chamfer.
        let (dx, dy) = (q.x - p.x, q.y - p.y);
        let len = (dx * dx + dy * dy).sqrt().max(1e-3);
        let (ux, uy) = (dx / len, dy / len);
        let k = 8.0;
        scene.stroke(
            PPath::build(|pb| {
                pb.move_to(q.x - ux * k - uy * k * 0.5, q.y - uy * k + ux * k * 0.5)
                    .line_to(q.x, q.y)
                    .line_to(q.x - ux * k + uy * k * 0.5, q.y - uy * k - ux * k * 0.5);
            }),
            PPaint::Solid(palette::VIOLET_SOFT),
            1.5,
        );
        let label = match (t.on.as_str(), t.after_ms) {
            ("", Some(ms)) => format!("after {:.0}s", ms / 1000.0),
            (on, Some(ms)) => format!("{on} / {:.0}s", ms / 1000.0),
            (on, None) => on.to_string(),
        };
        if !label.is_empty() {
            if let Some(run) = sink.run(&label, &micro, None) {
                // A third of the way along *this* edge's own direction. A
                // reversed pair therefore labels opposite ends of the same
                // line instead of writing both captions on top of each other.
                const AT: f32 = 0.34;
                scene.text(
                    run.rect_at(
                        p.x + (q.x - p.x) * AT + 4.0,
                        p.y + (q.y - p.y) * AT - 12.0,
                    ),
                    run.tex.clone(),
                    micro.color,
                );
            }
        }
    }

    for (i, s) in g.states.iter().enumerate() {
        let c = to_screen(s.x, s.y);
        let w = crate::graph::NODE_W * zoom;
        let h = crate::graph::NODE_H * zoom;
        let r = Rect::new(c.x - w * 0.5, c.y - h * 0.5, w, h);
        let selected = selection.contains(Target::State(i));
        scene.structural(r, if selected { Structural::CardHover } else { Structural::Card });
        if s.initial {
            scene.stroke(PPath::rect(r.inset(1.0)), PPaint::Solid(palette::CYAN), 1.5);
        }
        if selected {
            scene.stroke(PPath::rect(r.inset(1.0)), PPaint::Solid(palette::VIOLET_SOFT), 2.0);
        }
        let title = Role::BodyStrong.style().with_color(palette::TEXT);
        if let Some(run) = sink.run(&s.name, &title, None) {
            scene.text(run.rect_at(r.x + 10.0, r.y + 8.0), run.tex.clone(), title.color);
        }
        let sub = if s.behaviour.is_empty() { "—".to_string() } else { s.behaviour.clone() };
        if let Some(run) = sink.run(&sub, &micro, None) {
            scene.text(run.rect_at(r.x + 10.0, r.y + 28.0), run.tex.clone(), micro.color);
        }
        hits.push((r, Target::State(i)));
    }
    hits
}
