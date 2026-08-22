//! A small **retained** widget layer: enough to build a speech bubble, a
//! settings panel and the rig editor's chrome, and deliberately no more.
//!
//! The tree is an arena of [`Node`]s. Layout is two passes — measure, then
//! arrange — and painting emits into a [`Scene`], so the same widget code that
//! draws live is what the atlas baker bakes.
//!
//! Everything visual comes from `wisp-theme`. There is no colour, radius,
//! spacing or duration literal in this file, and that is the point: other
//! crates build on this, so if a token can be reached from here, it will be.

use wisp_theme::component::{ButtonState, ButtonVariant, Status};
use wisp_theme::surface::{Floating, Recessed, Structural};
use wisp_theme::typography::{IconStyle, Role};
use wisp_theme::{Color, Insets, Radius, Space, Surface};

use crate::geom::{Path, Point, Rect};
use crate::paint::Paint;
use crate::painter::Painter;
use crate::scene::Scene;
use crate::text::TextEngine;

pub type NodeId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Row,
    Column,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Start,
    Centre,
    End,
    /// Stretch to the container's cross size.
    Stretch,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sizing {
    /// Exactly this many pixels.
    Fixed(f32),
    /// As small as the content allows.
    Hug,
    /// Share what is left over with the other `Fill` siblings.
    Fill,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub w: Sizing,
    pub h: Sizing,
}

impl Size {
    pub const HUG: Size = Size { w: Sizing::Hug, h: Sizing::Hug };
    pub const FILL: Size = Size { w: Sizing::Fill, h: Sizing::Fill };
    pub const fn new(w: Sizing, h: Sizing) -> Size {
        Size { w, h }
    }
    pub const fn fixed(w: f32, h: f32) -> Size {
        Size { w: Sizing::Fixed(w), h: Sizing::Fixed(h) }
    }
}

/// The stroked, geometric glyph set of DESIGN.md §8. No emoji, no icon font.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Close,
    ChevronDown,
    ChevronRight,
    Check,
    Plus,
    Minus,
    Gear,
    Warning,
}

#[derive(Debug, Clone)]
pub enum Widget {
    /// A themed surface. Which of the three kinds it is decides whether it can
    /// have a blur at all — `wisp-theme` will not let this go wrong.
    Surface(Surface),
    /// A surface painted with an explicit paint, for the rig editor's swatches
    /// and the progress fill.
    Fill { paint: Paint, radius: Radius },
    Text { text: String, role: Role, color: Option<Color>, wrap: bool },
    Icon { icon: Icon, style: IconStyle },
    /// A status dot — one of the two sanctioned circles.
    StatusDot(Status),
    Stack { axis: Axis, gap: Space, padding: Insets, align: Align },
    /// A clipped, scrollable viewport around one child.
    Scroll { offset: f32 },
    Button { label: String, variant: ButtonVariant, state: ButtonState },
    /// A gradient hairline divider.
    Divider,
    /// Layout-only. Spacers and hit regions.
    Spacer,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub widget: Widget,
    pub size: Size,
    pub children: Vec<NodeId>,
    /// Filled in by [`Ui::layout`].
    pub layout: Rect,
    pub visible: bool,
    /// Content extent measured during layout — what a scroll container needs.
    pub content: (f32, f32),
    parent: Option<NodeId>,
}

impl Node {
    fn new(widget: Widget, size: Size) -> Node {
        Node {
            widget,
            size,
            children: Vec::new(),
            layout: Rect::ZERO,
            visible: true,
            content: (0.0, 0.0),
            parent: None,
        }
    }
}

/// The retained tree.
pub struct Ui {
    nodes: Vec<Node>,
    root: Option<NodeId>,
    pub motion: wisp_theme::Motion,
    /// Pointer position, for hover and for the pointer-bound sheen.
    pointer: Option<Point>,
    pressed: Option<NodeId>,
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}

impl Ui {
    pub fn new() -> Ui {
        Ui {
            nodes: Vec::new(),
            root: None,
            motion: wisp_theme::Motion::Full,
            pointer: None,
            pressed: None,
        }
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
        self.root = None;
        self.pressed = None;
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    pub fn root(&self) -> Option<NodeId> {
        self.root
    }
    pub fn set_root(&mut self, id: NodeId) {
        self.root = Some(id);
    }

    pub fn add(&mut self, widget: Widget, size: Size) -> NodeId {
        self.nodes.push(Node::new(widget, size));
        let id = self.nodes.len() - 1;
        if self.root.is_none() {
            self.root = Some(id);
        }
        id
    }

    /// Convenience: add a node and immediately parent it.
    pub fn add_to(&mut self, parent: NodeId, widget: Widget, size: Size) -> NodeId {
        let id = self.add(widget, size);
        self.attach(parent, id);
        id
    }

    pub fn attach(&mut self, parent: NodeId, child: NodeId) {
        if parent == child {
            return;
        }
        self.nodes[parent].children.push(child);
        self.nodes[child].parent = Some(parent);
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }
    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id]
    }
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].parent
    }

    pub fn set_text(&mut self, id: NodeId, s: impl Into<String>) {
        if let Widget::Text { text, .. } = &mut self.nodes[id].widget {
            *text = s.into();
        } else if let Widget::Button { label, .. } = &mut self.nodes[id].widget {
            *label = s.into();
        }
    }

    pub fn scroll_by(&mut self, id: NodeId, dy: f32) {
        let max = (self.nodes[id].content.1 - self.nodes[id].layout.h).max(0.0);
        if let Widget::Scroll { offset } = &mut self.nodes[id].widget {
            *offset = (*offset + dy).clamp(0.0, max);
        }
    }

    // ------------------------------------------------------------------ input

    /// Feed the pointer in. Returns the node that was *clicked* — press then
    /// release over the same node — so callers do not have to track it.
    pub fn pointer(&mut self, at: Option<Point>, down: bool) -> Option<NodeId> {
        self.pointer = at;
        let over = at.and_then(|p| self.hit(p));
        let mut clicked = None;
        match (down, self.pressed) {
            (true, None) => {
                if let Some(id) = over {
                    if self.is_enabled(id) {
                        self.pressed = Some(id);
                    }
                }
            }
            (false, Some(id)) => {
                if over == Some(id) && self.is_enabled(id) {
                    clicked = Some(id);
                }
                self.pressed = None;
            }
            _ => {}
        }
        let pressed = self.pressed;
        for (i, n) in self.nodes.iter_mut().enumerate() {
            if let Widget::Button { state, .. } = &mut n.widget {
                if !state.disabled {
                    state.hovered = over == Some(i);
                    state.pressed = pressed == Some(i);
                }
            }
        }
        clicked
    }

    fn is_enabled(&self, id: NodeId) -> bool {
        match &self.nodes[id].widget {
            Widget::Button { state, .. } => !state.disabled,
            _ => true,
        }
    }

    /// Topmost node containing the point, respecting scroll clipping.
    pub fn hit(&self, p: Point) -> Option<NodeId> {
        let root = self.root?;
        self.hit_in(root, p, None)
    }

    fn hit_in(&self, id: NodeId, p: Point, clip: Option<Rect>) -> Option<NodeId> {
        let n = &self.nodes[id];
        if !n.visible {
            return None;
        }
        if let Some(c) = clip {
            if !c.contains(p) {
                return None;
            }
        }
        let clip = match n.widget {
            Widget::Scroll { .. } => Some(clip.map_or(n.layout, |c| c.intersect(n.layout))),
            _ => clip,
        };
        // Later children paint on top, so they win the hit.
        for &c in n.children.iter().rev() {
            if let Some(hit) = self.hit_in(c, p, clip) {
                return Some(hit);
            }
        }
        if n.layout.contains(p) && !matches!(n.widget, Widget::Spacer) {
            Some(id)
        } else {
            None
        }
    }

    // ----------------------------------------------------------------- layout

    /// Measure, then arrange, into `bounds`.
    pub fn layout(&mut self, text: &mut TextEngine, bounds: Rect) {
        let Some(root) = self.root else { return };
        self.measure(root, text, bounds.w, bounds.h);
        self.arrange(root, bounds);
    }

    fn measure(&mut self, id: NodeId, text: &mut TextEngine, avail_w: f32, avail_h: f32) -> (f32, f32) {
        let (widget, size, children) = {
            let n = &self.nodes[id];
            (n.widget.clone(), n.size, n.children.clone())
        };
        let mut content = match &widget {
            Widget::Text { text: s, role, wrap, .. } => {
                let style = role.style();
                let wrap_w = matches!(size.w, Sizing::Fixed(_) | Sizing::Fill)
                    .then_some(avail_w)
                    .filter(|_| *wrap);
                text.measure(s, &style, wrap_w)
            }
            Widget::Icon { style, .. } => (style.box_px, style.box_px),
            Widget::StatusDot(s) => {
                let d = s.dot().diameter_px;
                (d, d)
            }
            Widget::Divider => (avail_w, 1.0),
            Widget::Button { label, .. } => {
                let (w, h) = text.measure(label, &Role::BodyStrong.style(), None);
                (w + BUTTON_PADDING.horizontal(), h + BUTTON_PADDING.vertical())
            }
            Widget::Stack { axis, gap, padding, .. } => {
                let inner_w = (avail_w - padding.horizontal()).max(0.0);
                let inner_h = (avail_h - padding.vertical()).max(0.0);
                let mut main = 0.0f32;
                let mut cross = 0.0f32;
                let visible: Vec<NodeId> =
                    children.iter().copied().filter(|c| self.nodes[*c].visible).collect();
                for (i, &c) in visible.iter().enumerate() {
                    let (cw, ch) = self.measure(c, text, inner_w, inner_h);
                    let (m, x) = match axis {
                        Axis::Row => (cw, ch),
                        Axis::Column => (ch, cw),
                    };
                    main += m;
                    if i + 1 < visible.len() {
                        main += gap.get();
                    }
                    cross = cross.max(x);
                }
                let (w, h) = match axis {
                    Axis::Row => (main, cross),
                    Axis::Column => (cross, main),
                };
                (w + padding.horizontal(), h + padding.vertical())
            }
            Widget::Scroll { .. } => {
                let mut w = 0.0f32;
                let mut h = 0.0f32;
                for &c in &children {
                    let (cw, ch) = self.measure(c, text, avail_w, f32::INFINITY);
                    w = w.max(cw);
                    h = h.max(ch);
                }
                (w, h)
            }
            Widget::Surface(_) | Widget::Fill { .. } | Widget::Spacer => {
                let mut w = 0.0f32;
                let mut h = 0.0f32;
                for &c in &children {
                    let (cw, ch) = self.measure(c, text, avail_w, avail_h);
                    w = w.max(cw);
                    h = h.max(ch);
                }
                (w, h)
            }
        };
        self.nodes[id].content = content;
        if let Sizing::Fixed(w) = size.w {
            content.0 = w;
        }
        if let Sizing::Fixed(h) = size.h {
            content.1 = h;
        }
        content
    }

    fn arrange(&mut self, id: NodeId, rect: Rect) {
        self.nodes[id].layout = rect;
        let (widget, children) = {
            let n = &self.nodes[id];
            (n.widget.clone(), n.children.clone())
        };
        match widget {
            Widget::Stack { axis, gap, padding, align } => {
                let inner = rect.inset_by(padding);
                let visible: Vec<NodeId> =
                    children.into_iter().filter(|c| self.nodes[*c].visible).collect();
                if visible.is_empty() {
                    return;
                }
                let gaps = gap.get() * (visible.len() - 1) as f32;
                let avail_main = match axis {
                    Axis::Row => inner.w,
                    Axis::Column => inner.h,
                } - gaps;

                let mut fixed = 0.0f32;
                let mut fills = 0usize;
                for &c in &visible {
                    let n = &self.nodes[c];
                    let s = match axis {
                        Axis::Row => n.size.w,
                        Axis::Column => n.size.h,
                    };
                    match s {
                        Sizing::Fill => fills += 1,
                        Sizing::Fixed(v) => fixed += v,
                        Sizing::Hug => {
                            fixed += match axis {
                                Axis::Row => n.content.0,
                                Axis::Column => n.content.1,
                            }
                        }
                    }
                }
                let share = if fills > 0 { ((avail_main - fixed) / fills as f32).max(0.0) } else { 0.0 };

                let mut cursor = match axis {
                    Axis::Row => inner.x,
                    Axis::Column => inner.y,
                };
                for &c in &visible {
                    let (size, content) = {
                        let n = &self.nodes[c];
                        (n.size, n.content)
                    };
                    let (main_s, cross_s) = match axis {
                        Axis::Row => (size.w, size.h),
                        Axis::Column => (size.h, size.w),
                    };
                    let (main_c, cross_c) = match axis {
                        Axis::Row => (content.0, content.1),
                        Axis::Column => (content.1, content.0),
                    };
                    let main = match main_s {
                        Sizing::Fixed(v) => v,
                        Sizing::Hug => main_c,
                        Sizing::Fill => share,
                    };
                    let cross_avail = match axis {
                        Axis::Row => inner.h,
                        Axis::Column => inner.w,
                    };
                    let cross = match cross_s {
                        Sizing::Fixed(v) => v,
                        Sizing::Fill => cross_avail,
                        Sizing::Hug => match align {
                            Align::Stretch => cross_avail,
                            _ => cross_c.min(cross_avail),
                        },
                    };
                    let cross_off = match align {
                        Align::Start | Align::Stretch => 0.0,
                        Align::Centre => (cross_avail - cross) * 0.5,
                        Align::End => cross_avail - cross,
                    };
                    let child_rect = match axis {
                        Axis::Row => Rect::new(cursor, inner.y + cross_off, main, cross),
                        Axis::Column => Rect::new(inner.x + cross_off, cursor, cross, main),
                    };
                    self.arrange(c, child_rect);
                    cursor += main + gap.get();
                }
            }
            Widget::Scroll { offset } => {
                let max = (self.nodes[id].content.1 - rect.h).max(0.0);
                let offset = offset.clamp(0.0, max);
                if let Widget::Scroll { offset: o } = &mut self.nodes[id].widget {
                    *o = offset;
                }
                for c in children {
                    let h = self.nodes[c].content.1;
                    let w = match self.nodes[c].size.w {
                        Sizing::Fixed(v) => v,
                        _ => rect.w,
                    };
                    self.arrange(c, Rect::new(rect.x, rect.y - offset, w, h.max(rect.h)));
                }
            }
            _ => {
                for c in children {
                    self.arrange(c, rect);
                }
            }
        }
    }

    // ----------------------------------------------------------------- paint

    pub fn paint(&self, painter: &Painter, text: &mut TextEngine, scene: &mut Scene) {
        if let Some(root) = self.root {
            self.paint_node(root, painter, text, scene);
        }
    }

    fn paint_node(&self, id: NodeId, painter: &Painter, text: &mut TextEngine, scene: &mut Scene) {
        let n = &self.nodes[id];
        if !n.visible || n.layout.is_empty() {
            return;
        }
        let r = n.layout;
        let mut clipped = false;
        match &n.widget {
            Widget::Surface(s) => match s {
                Surface::Structural(k) => {
                    scene.structural(r, *k);
                }
                Surface::Recessed(k) => {
                    scene.recessed(r, *k);
                }
                Surface::Floating(k) => {
                    scene.floating(r, *k);
                }
            },
            Widget::Fill { paint, radius } => {
                scene.fill_rect(r, *radius, paint.clone());
            }
            Widget::Text { text: s, role, color, wrap } => {
                let mut style = role.style();
                if let Some(c) = color {
                    style = style.with_color(*c);
                }
                let wrap_w = wrap.then_some(r.w);
                let run = text.run(painter, s, &style, wrap_w);
                scene.text(run.rect_at(r.x, r.y), run.tex.clone(), style.color);
            }
            Widget::Icon { icon, style } => {
                paint_icon(scene, *icon, r, *style);
            }
            Widget::StatusDot(s) => {
                scene.circle(r.centre(), s.dot(), s.color());
            }
            Widget::Divider => {
                scene.hairline(r);
            }
            Widget::Button { label, variant, state } => {
                let skin = variant.skin(*state);
                let box_r = r.translate(0.0, skin.lift_px).scale_about_centre(skin.scale);
                if let Some(ring) = skin.focus_ring {
                    for layer in ring.0.iter().rev() {
                        scene.fill_rect(
                            box_r.inset(-layer.spread),
                            skin.radius,
                            Paint::solid(layer.color),
                        );
                    }
                }
                scene.fill_rect(box_r, skin.radius, Paint::token(skin.fill).with_opacity(skin.opacity));
                scene.lit_edge(box_r, skin.radius, skin.edge);
                let style = Role::BodyStrong.style().with_color(skin.label);
                let run = text.run(painter, label, &style, None);
                let tx = box_r.x + (box_r.w - run.width) * 0.5;
                let ty = box_r.y + (box_r.h - run.height) * 0.5;
                scene.text(
                    run.rect_at(tx, ty),
                    run.tex.clone(),
                    skin.label.with_alpha(skin.label.alpha_f() * skin.opacity),
                );
            }
            Widget::Scroll { .. } => {
                scene.push_clip(r);
                clipped = true;
            }
            // Containers paint nothing themselves.
            Widget::Stack { .. } | Widget::Spacer => {}
        }
        for &c in &n.children {
            self.paint_node(c, painter, text, scene);
        }
        if clipped {
            scene.pop_clip();
        }
    }
}

/// §5: buttons are sharp-cut glass blocks. The padding is on the 8px grid.
pub const BUTTON_PADDING: Insets = Insets::xy(Space::S2, Space::S1);

fn paint_icon(scene: &mut Scene, icon: Icon, r: Rect, style: IconStyle) {
    let b = style.box_px;
    let x = r.x + (r.w - b) * 0.5;
    let y = r.y + (r.h - b) * 0.5;
    let p = |u: f32, v: f32| (x + u * b, y + v * b);
    let path = Path::build(|pb| match icon {
        Icon::Close => {
            let (ax, ay) = p(0.25, 0.25);
            let (bx, by) = p(0.75, 0.75);
            pb.move_to(ax, ay).line_to(bx, by);
            let (cx, cy) = p(0.75, 0.25);
            let (dx, dy) = p(0.25, 0.75);
            pb.move_to(cx, cy).line_to(dx, dy);
        }
        Icon::ChevronDown => {
            let (ax, ay) = p(0.24, 0.40);
            let (bx, by) = p(0.50, 0.66);
            let (cx, cy) = p(0.76, 0.40);
            pb.move_to(ax, ay).line_to(bx, by).line_to(cx, cy);
        }
        Icon::ChevronRight => {
            let (ax, ay) = p(0.40, 0.24);
            let (bx, by) = p(0.66, 0.50);
            let (cx, cy) = p(0.40, 0.76);
            pb.move_to(ax, ay).line_to(bx, by).line_to(cx, cy);
        }
        Icon::Check => {
            let (ax, ay) = p(0.22, 0.52);
            let (bx, by) = p(0.42, 0.72);
            let (cx, cy) = p(0.78, 0.30);
            pb.move_to(ax, ay).line_to(bx, by).line_to(cx, cy);
        }
        Icon::Plus => {
            let (ax, ay) = p(0.50, 0.22);
            let (bx, by) = p(0.50, 0.78);
            pb.move_to(ax, ay).line_to(bx, by);
            let (cx, cy) = p(0.22, 0.50);
            let (dx, dy) = p(0.78, 0.50);
            pb.move_to(cx, cy).line_to(dx, dy);
        }
        Icon::Minus => {
            let (cx, cy) = p(0.22, 0.50);
            let (dx, dy) = p(0.78, 0.50);
            pb.move_to(cx, cy).line_to(dx, dy);
        }
        Icon::Gear => {
            // A hexagonal ring — the mark is a hexagon, so the gear echoes it
            // rather than importing a cog from somewhere else.
            for i in 0..6 {
                let a = std::f32::consts::TAU * (i as f32 / 6.0) - std::f32::consts::FRAC_PI_2;
                let (gx, gy) = p(0.5 + 0.28 * a.cos(), 0.5 + 0.28 * a.sin());
                if i == 0 {
                    pb.move_to(gx, gy);
                } else {
                    pb.line_to(gx, gy);
                }
            }
            pb.close();
        }
        Icon::Warning => {
            let (ax, ay) = p(0.50, 0.20);
            let (bx, by) = p(0.86, 0.80);
            let (cx, cy) = p(0.14, 0.80);
            pb.move_to(ax, ay).line_to(bx, by).line_to(cx, cy).close();
        }
    });
    scene.stroke(path, Paint::solid(style.color), style.stroke_px);
}

// ------------------------------------------------------------------ builders

/// A card: opaque elevation step, `--sp-3` padding, column layout.
pub fn card(ui: &mut Ui, size: Size) -> NodeId {
    let surface = ui.add(Widget::Surface(Surface::Structural(Structural::Card)), size);
    let stack = ui.add(
        Widget::Stack {
            axis: Axis::Column,
            gap: Space::S2,
            padding: Insets::CARD,
            align: Align::Stretch,
        },
        Size::FILL,
    );
    ui.attach(surface, stack);
    stack
}

/// A well cut into whatever is underneath: list rows, logs, code.
pub fn well(ui: &mut Ui, size: Size) -> NodeId {
    let surface = ui.add(Widget::Surface(Surface::Recessed(Recessed::Well)), size);
    let stack = ui.add(
        Widget::Stack {
            axis: Axis::Column,
            gap: Space::S1,
            padding: Insets::CONTENT,
            align: Align::Stretch,
        },
        Size::FILL,
    );
    ui.attach(surface, stack);
    stack
}

/// The speech bubble. The one floating layer that is Wisp's own, so it is the
/// one place in the widget layer that spends a real blur.
pub fn bubble(ui: &mut Ui, size: Size) -> NodeId {
    let surface = ui.add(Widget::Surface(Surface::Floating(Floating::Bubble)), size);
    let stack = ui.add(
        Widget::Stack {
            axis: Axis::Column,
            gap: Space::S1,
            padding: Insets::CONTENT,
            align: Align::Start,
        },
        Size::FILL,
    );
    ui.attach(surface, stack);
    stack
}

pub fn label(ui: &mut Ui, parent: NodeId, text: impl Into<String>, role: Role) -> NodeId {
    ui.add_to(
        parent,
        Widget::Text { text: text.into(), role, color: None, wrap: false },
        Size::HUG,
    )
}

pub fn paragraph(ui: &mut Ui, parent: NodeId, text: impl Into<String>) -> NodeId {
    ui.add_to(
        parent,
        Widget::Text { text: text.into(), role: Role::Body, color: None, wrap: true },
        Size::new(Sizing::Fill, Sizing::Hug),
    )
}

pub fn button(
    ui: &mut Ui,
    parent: NodeId,
    label: impl Into<String>,
    variant: ButtonVariant,
) -> NodeId {
    ui.add_to(
        parent,
        Widget::Button { label: label.into(), variant, state: ButtonState::default() },
        Size::HUG,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> TextEngine {
        TextEngine::new()
    }

    #[test]
    fn a_column_stacks_children_on_the_grid() {
        let mut ui = Ui::new();
        let root = ui.add(
            Widget::Stack {
                axis: Axis::Column,
                gap: Space::S2,
                padding: Insets::ZERO,
                align: Align::Start,
            },
            Size::FILL,
        );
        let a = ui.add_to(root, Widget::Spacer, Size::fixed(50.0, 20.0));
        let b = ui.add_to(root, Widget::Spacer, Size::fixed(50.0, 30.0));
        ui.layout(&mut engine(), Rect::from_size(200.0, 200.0));
        assert_eq!(ui.node(a).layout.y, 0.0);
        assert_eq!(ui.node(b).layout.y, 20.0 + Space::S2.get());
    }

    #[test]
    fn fill_children_share_the_leftover() {
        let mut ui = Ui::new();
        let root = ui.add(
            Widget::Stack {
                axis: Axis::Row,
                gap: Space::ZERO,
                padding: Insets::ZERO,
                align: Align::Start,
            },
            Size::FILL,
        );
        ui.add_to(root, Widget::Spacer, Size::new(Sizing::Fixed(40.0), Sizing::Fill));
        let a = ui.add_to(root, Widget::Spacer, Size::FILL);
        let b = ui.add_to(root, Widget::Spacer, Size::FILL);
        ui.layout(&mut engine(), Rect::from_size(140.0, 40.0));
        assert_eq!(ui.node(a).layout.w, 50.0);
        assert_eq!(ui.node(b).layout.w, 50.0);
        assert_eq!(ui.node(b).layout.x, 90.0);
    }

    #[test]
    fn padding_comes_off_the_inside() {
        let mut ui = Ui::new();
        let root = ui.add(
            Widget::Stack {
                axis: Axis::Column,
                gap: Space::ZERO,
                padding: Insets::CARD,
                align: Align::Stretch,
            },
            Size::FILL,
        );
        let a = ui.add_to(root, Widget::Spacer, Size::FILL);
        ui.layout(&mut engine(), Rect::from_size(200.0, 200.0));
        assert_eq!(ui.node(a).layout.x, Space::S3.get());
        assert_eq!(ui.node(a).layout.w, 200.0 - Space::S3.get() * 2.0);
    }

    #[test]
    fn centre_alignment_centres_on_the_cross_axis() {
        let mut ui = Ui::new();
        let root = ui.add(
            Widget::Stack {
                axis: Axis::Row,
                gap: Space::ZERO,
                padding: Insets::ZERO,
                align: Align::Centre,
            },
            Size::FILL,
        );
        let a = ui.add_to(root, Widget::Spacer, Size::fixed(20.0, 20.0));
        ui.layout(&mut engine(), Rect::from_size(100.0, 100.0));
        assert_eq!(ui.node(a).layout.y, 40.0);
    }

    #[test]
    fn hidden_children_take_no_space() {
        let mut ui = Ui::new();
        let root = ui.add(
            Widget::Stack {
                axis: Axis::Column,
                gap: Space::S1,
                padding: Insets::ZERO,
                align: Align::Start,
            },
            Size::FILL,
        );
        let a = ui.add_to(root, Widget::Spacer, Size::fixed(10.0, 10.0));
        let b = ui.add_to(root, Widget::Spacer, Size::fixed(10.0, 10.0));
        ui.node_mut(a).visible = false;
        ui.layout(&mut engine(), Rect::from_size(100.0, 100.0));
        assert_eq!(ui.node(b).layout.y, 0.0, "a hidden sibling must not leave a gap");
    }

    #[test]
    fn scrolling_clamps_to_the_content() {
        let mut ui = Ui::new();
        let sc = ui.add(Widget::Scroll { offset: 0.0 }, Size::FILL);
        ui.add_to(sc, Widget::Spacer, Size::fixed(50.0, 500.0));
        ui.layout(&mut engine(), Rect::from_size(100.0, 100.0));
        ui.scroll_by(sc, 1000.0);
        match ui.node(sc).widget {
            Widget::Scroll { offset } => assert_eq!(offset, 400.0),
            _ => unreachable!(),
        }
        ui.scroll_by(sc, -10_000.0);
        match ui.node(sc).widget {
            Widget::Scroll { offset } => assert_eq!(offset, 0.0),
            _ => unreachable!(),
        }
    }

    #[test]
    fn hit_testing_prefers_the_topmost_child() {
        let mut ui = Ui::new();
        let root = ui.add(Widget::Surface(Surface::Structural(Structural::Card)), Size::FILL);
        let a = ui.add_to(root, Widget::Spacer, Size::FILL);
        let b = ui.add_to(root, Widget::Surface(Surface::Recessed(Recessed::Well)), Size::FILL);
        let _ = a;
        ui.layout(&mut engine(), Rect::from_size(100.0, 100.0));
        assert_eq!(ui.hit(Point::new(50.0, 50.0)), Some(b));
        assert_eq!(ui.hit(Point::new(500.0, 50.0)), None);
    }

    #[test]
    fn a_scroll_container_clips_hit_testing() {
        let mut ui = Ui::new();
        let sc = ui.add(Widget::Scroll { offset: 0.0 }, Size::FILL);
        let child = ui.add_to(sc, Widget::Surface(Surface::Structural(Structural::Card)), Size::fixed(50.0, 500.0));
        ui.layout(&mut engine(), Rect::from_size(100.0, 100.0));
        assert_eq!(ui.hit(Point::new(10.0, 10.0)), Some(child));
        assert_eq!(ui.hit(Point::new(10.0, 300.0)), None, "outside the viewport");
    }

    #[test]
    fn press_then_release_over_the_same_button_is_a_click() {
        let mut ui = Ui::new();
        let root = ui.add(
            Widget::Stack { axis: Axis::Column, gap: Space::ZERO, padding: Insets::ZERO, align: Align::Start },
            Size::FILL,
        );
        let b = button(&mut ui, root, "Do it", ButtonVariant::Primary);
        ui.layout(&mut engine(), Rect::from_size(300.0, 100.0));
        let p = ui.node(b).layout.centre();
        assert_eq!(ui.pointer(Some(p), true), None);
        match ui.node(b).widget {
            Widget::Button { state, .. } => assert!(state.pressed && state.hovered),
            _ => unreachable!(),
        }
        assert_eq!(ui.pointer(Some(p), false), Some(b));
    }

    #[test]
    fn releasing_off_the_button_is_not_a_click() {
        let mut ui = Ui::new();
        let root = ui.add(
            Widget::Stack { axis: Axis::Column, gap: Space::ZERO, padding: Insets::ZERO, align: Align::Start },
            Size::FILL,
        );
        let b = button(&mut ui, root, "Do it", ButtonVariant::Primary);
        ui.layout(&mut engine(), Rect::from_size(300.0, 100.0));
        let p = ui.node(b).layout.centre();
        ui.pointer(Some(p), true);
        assert_eq!(ui.pointer(Some(Point::new(290.0, 90.0)), false), None);
    }

    #[test]
    fn a_disabled_button_never_reports_a_click() {
        let mut ui = Ui::new();
        let root = ui.add(
            Widget::Stack { axis: Axis::Column, gap: Space::ZERO, padding: Insets::ZERO, align: Align::Start },
            Size::FILL,
        );
        let b = button(&mut ui, root, "Do it", ButtonVariant::Primary);
        if let Widget::Button { state, .. } = &mut ui.node_mut(b).widget {
            state.disabled = true;
        }
        ui.layout(&mut engine(), Rect::from_size(300.0, 100.0));
        let p = ui.node(b).layout.centre();
        ui.pointer(Some(p), true);
        assert_eq!(ui.pointer(Some(p), false), None);
    }

    #[test]
    fn a_button_hugs_its_label_plus_grid_padding() {
        let mut ui = Ui::new();
        let root = ui.add(
            Widget::Stack { axis: Axis::Column, gap: Space::ZERO, padding: Insets::ZERO, align: Align::Start },
            Size::FILL,
        );
        let b = button(&mut ui, root, "Enable microphone", ButtonVariant::Secondary);
        ui.layout(&mut engine(), Rect::from_size(400.0, 100.0));
        let r = ui.node(b).layout;
        assert!(r.w > BUTTON_PADDING.horizontal());
        assert!(r.h >= BUTTON_PADDING.vertical());
        assert_eq!(BUTTON_PADDING.horizontal() % 8.0, 0.0);
    }

    #[test]
    fn the_card_builder_returns_the_content_stack_not_the_surface() {
        let mut ui = Ui::new();
        let content = card(&mut ui, Size::FILL);
        label(&mut ui, content, "Senses", Role::TitleSmall);
        ui.layout(&mut engine(), Rect::from_size(300.0, 200.0));
        let surface = ui.parent(content).unwrap();
        assert!(matches!(ui.node(surface).widget, Widget::Surface(Surface::Structural(_))));
        assert_eq!(ui.node(surface).layout, Rect::from_size(300.0, 200.0));
    }

    #[test]
    fn a_paragraph_wraps_inside_its_column() {
        let mut ui = Ui::new();
        let content = card(&mut ui, Size::FILL);
        let p = paragraph(
            &mut ui,
            content,
            "She is not multi-user and never will be; one operator, one machine.",
        );
        ui.layout(&mut engine(), Rect::from_size(240.0, 300.0));
        let r = ui.node(p).layout;
        assert!(r.w <= 240.0 - Insets::CARD.horizontal() + 0.5);
        assert!(ui.node(p).content.1 > Role::Body.style().line_height_px(), "it must have wrapped");
    }
}
