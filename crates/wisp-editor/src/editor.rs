//! The editor: one state machine, and the input routing on top of it.
//!
//! # It is a library, not an application
//!
//! Nothing here opens a window, owns a clock, or touches the compositor. The
//! host mounts it: it hands over a rectangle, feeds it pointer and key events,
//! calls [`Editor::tick`] with a delta it got from somewhere else, and renders
//! the [`Scene`] that comes back. That is the whole contract, and it is what
//! lets `cargo test -p wisp-editor` drive the entire editor — every tool,
//! every gesture — with no GPU and no Wayland session.
//!
//! # One document, one compile
//!
//! [`Editor::doc`] is the working [`SkinDoc`] and the only mutable truth.
//! After every edit the document is compiled through `wisp-rig`'s ordinary
//! parser, and the result is either the [`Skin`] the preview draws or the list
//! of problems the status strip shows. There is no privileged path: the editor
//! reads and writes exactly what a third-party pack reads and writes (F49), so
//! a skin the editor cannot open is a skin the runtime cannot open either.
//!
//! An edit that leaves the document invalid is **kept**, not rejected. An
//! editor that refuses to hold a half-finished state is an editor you cannot
//! work in — you would have to make every change in an order that is valid at
//! every intermediate step. The invalid state is loud instead: the preview
//! keeps showing the last good compile, the status strip goes red, and the
//! problems are listed. The one exception is a bone cycle, which
//! [`crate::bones::reparent`] refuses outright, because the rig's own
//! validator rejects it and losing the whole preview to a mis-drag is not a
//! trade worth making.

use std::path::{Path, PathBuf};

use wisp_paint::geom::{Point, Rect};
use wisp_paint::scene::Scene;
use wisp_paint::text::TextEngine;
use wisp_rig::math::Vec2;
use wisp_rig::skin::doc::SkinDoc;
use wisp_rig::Skin;

use crate::cmd::Command;
use crate::error::{EditError, Validation};
use crate::graph::{GraphCommand, MoodGraph};
use crate::history::History;
use crate::panels::{Action, Frames, PanelState, Panels};
use crate::preview::Preview;
use crate::puppet::{Grip, Puppet};
use crate::save::SaveReport;
use crate::select::{SelectMode, Selection, Target, Tool};
use crate::text::TextSink;
use crate::timeline::TimelineState;
use crate::view::Viewport;

/// A key the editor understands. The host maps its own keysyms onto these, so
/// this crate never sees a Wayland keycode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Undo,
    Redo,
    Save,
    Delete,
    /// Keyframe the pose puppet mode is holding.
    Keyframe,
    PlayPause,
    ToggleOnion,
    ToggleGraph,
    NextFrame,
    PrevFrame,
    /// Abandon whatever gesture is in flight.
    Escape,
    Fit,
    Tool(Tool),
}

/// A drag in flight on the canvas.
#[derive(Debug, Clone)]
enum Drag {
    /// Moving path points.
    Points { shape: usize, points: Vec<usize>, grab: Vec2 },
    /// Moving a bone's rest position.
    Bone { bone: usize },
    /// Rubber band.
    Marquee { from: Point },
    /// Dragging the canvas.
    Pan { from: Point },
    /// Dragging a bone out of a parent.
    NewBone { parent: usize },
    /// Painting weights.
    Weight { bone: usize },
    /// Puppeting.
    Puppet,
    /// Dragging the playhead.
    Scrub,
    /// Moving a node in the state-machine view.
    GraphNode { state: usize, grab: (f32, f32) },
}

/// How far, in pixels, the pointer has to move before a press counts as a drag
/// rather than a click. Below this everything is a click, which is what stops
/// a slightly shaky hand from nudging every point it selects.
pub const DRAG_SLOP_PX: f32 = 3.0;

pub struct Editor {
    doc: SkinDoc,
    /// The file's original text, kept so comments survive a save.
    source: Option<String>,
    path: Option<PathBuf>,
    compiled: Option<Skin>,
    validation: Validation,
    history: History<Command>,

    pub selection: Selection,
    pub tool: Tool,
    pub view: Viewport,
    pub timeline: TimelineState,

    pub graph: MoodGraph,
    graph_history: History<GraphCommand>,
    pub show_graph: bool,
    /// Graph pan and zoom: the graph-space point at the panel's centre, and
    /// its scale.
    pub graph_view: (f32, f32, f32),

    preview: Option<Preview>,
    puppet: Option<Puppet>,
    puppet_grips: Vec<Grip>,

    frames: Frames,
    drag: Option<Drag>,
    press_at: Option<Point>,
    /// The last pointer position, so a marquee can be drawn between the press
    /// and the release without the host having to hand it back.
    last_pointer: Option<Point>,
    dragging: bool,
    /// Bones clicked so far with the IK tool.
    ik_pick: Vec<usize>,
    /// The shape the operator is working on. Sticky: selecting a *bone* to
    /// paint weights towards must not lose the shape you are painting on, and
    /// an editor where picking a tool's second operand cancels its first is an
    /// editor you fight.
    active_shape: Option<usize>,
    /// Keyframe boxes from the last timeline draw.
    timeline_hits: Vec<(Rect, Target)>,
    graph_hits: Vec<(Rect, Target)>,
    status: String,
    last_save: Option<SaveReport>,
}

impl Editor {
    /// Open the shipped skin — what `wisp --edit` does with no argument.
    pub fn default_skin() -> Result<Editor, EditError> {
        let src = wisp_rig::skin::WISP_SKIN_TOML;
        let doc: SkinDoc = toml::from_str(src).map_err(|e| EditError::Read(e.to_string()))?;
        Ok(Editor::new(doc, Some(src.to_string()), None))
    }

    /// Open a skin file, keeping its text for the comment merge on save.
    pub fn open(path: &Path) -> Result<Editor, EditError> {
        let (doc, src) = crate::save::read(path)?;
        let mut ed = Editor::new(doc, Some(src), Some(path.to_path_buf()));
        let graph_path = MoodGraph::path_for(path);
        if let Ok(text) = std::fs::read_to_string(&graph_path) {
            if let Ok(g) = MoodGraph::parse(&text) {
                ed.graph = g;
            }
        }
        Ok(ed)
    }

    /// A blank skin, for authoring a new character from nothing.
    pub fn blank(name: &str) -> Editor {
        Editor::new(SkinDoc::blank(name), None, None)
    }

    pub fn new(doc: SkinDoc, source: Option<String>, path: Option<PathBuf>) -> Editor {
        let graph = MoodGraph::from_skin(&doc);
        let mut ed = Editor {
            doc,
            source,
            path,
            compiled: None,
            validation: Validation::default(),
            history: History::default(),
            selection: Selection::new(),
            tool: Tool::Select,
            view: Viewport::default(),
            timeline: TimelineState::default(),
            graph,
            graph_history: History::default(),
            show_graph: false,
            graph_view: (0.0, 0.0, 1.0),
            preview: None,
            puppet: None,
            puppet_grips: Vec::new(),
            frames: crate::panels::frames(Rect::from_size(1280.0, 800.0)),
            drag: None,
            press_at: None,
            last_pointer: None,
            dragging: false,
            ik_pick: Vec::new(),
            active_shape: None,
            timeline_hits: Vec::new(),
            graph_hits: Vec::new(),
            status: String::new(),
            last_save: None,
        };
        ed.recompile();
        ed.status = ed.validation.summary();
        ed
    }

    // -------------------------------------------------------------- reading

    pub fn doc(&self) -> &SkinDoc {
        &self.doc
    }
    pub fn skin(&self) -> Option<&Skin> {
        self.compiled.as_ref()
    }
    pub fn validation(&self) -> &Validation {
        &self.validation
    }
    pub fn dirty(&self) -> bool {
        self.history.dirty()
    }
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn status(&self) -> &str {
        &self.status
    }
    pub fn last_save(&self) -> Option<&SaveReport> {
        self.last_save.as_ref()
    }
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }
    pub fn preview(&self) -> Option<&Preview> {
        self.preview.as_ref()
    }
    pub fn frames(&self) -> Frames {
        self.frames
    }

    /// The shape the tools act on: whatever the selection points at, or the
    /// last one that was selected.
    pub fn current_shape(&self) -> Option<usize> {
        self.selection
            .anchor()
            .and_then(|t| t.owning_shape())
            .or_else(|| self.selection.iter().find_map(|t| t.owning_shape()))
            .or(self.active_shape)
            .filter(|i| *i < self.doc.shapes.len())
    }

    /// The document as it would be written, with comments merged back in.
    pub fn to_toml(&self) -> Result<(String, SaveReport), EditError> {
        match &self.source {
            Some(src) => crate::save::to_toml_preserving(&self.doc, src),
            None => Ok((crate::save::to_toml(&self.doc)?, SaveReport::default())),
        }
    }

    /// What saving would cost in comments, **before** it happens. The editor
    /// shows this in the save confirmation, which is the whole of "make the
    /// destructiveness loud".
    pub fn save_preview(&self) -> Result<SaveReport, EditError> {
        Ok(self.to_toml()?.1)
    }

    // -------------------------------------------------------------- editing

    /// Apply a command through the undo stack and recompile.
    pub fn apply(&mut self, cmd: Command) -> Result<(), EditError> {
        let label = cmd.label();
        match self.history.apply(&mut self.doc, cmd) {
            Ok(()) => {
                self.after_edit();
                self.status = if self.validation.ok() {
                    label.to_string()
                } else {
                    self.validation.summary()
                };
                Ok(())
            }
            Err(e) => {
                self.status = e.message();
                Err(e)
            }
        }
    }

    /// Apply a graph edit through the graph's own undo stack.
    pub fn apply_graph(&mut self, cmd: GraphCommand) -> Result<(), EditError> {
        match self.graph_history.apply(&mut self.graph, cmd) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.status = e.message();
                Err(e)
            }
        }
    }

    pub fn undo(&mut self) -> Result<(), EditError> {
        let label = self.history.undo(&mut self.doc)?;
        self.after_edit();
        self.status = format!("undid {label}");
        Ok(())
    }

    pub fn redo(&mut self) -> Result<(), EditError> {
        let label = self.history.redo(&mut self.doc)?;
        self.after_edit();
        self.status = format!("redid {label}");
        Ok(())
    }

    /// Open a gesture: everything applied until [`Editor::end_gesture`] that
    /// touches the same field folds into one undo step.
    pub fn begin_gesture(&mut self) {
        self.history.begin();
    }
    pub fn end_gesture(&mut self) {
        self.history.end();
    }

    fn after_edit(&mut self) {
        self.recompile();
        let doc = &self.doc;
        self.selection.retain_valid(|t| valid_target(doc, t));
        // The preview holds a compiled skin; a changed document means a new
        // one. Rebuilding here rather than lazily keeps "what is on screen"
        // and "what is in the file" from ever disagreeing.
        self.preview = self.compiled.clone().map(Preview::new);
        self.puppet = None;
        self.puppet_grips.clear();
    }

    fn recompile(&mut self) {
        match Skin::compile(self.doc.clone()) {
            Ok(skin) => {
                self.compiled = Some(skin);
                self.validation = Validation::default();
            }
            Err(e) => {
                self.validation = Validation::from_result(Some(&e));
                // Keep the last good compile: the preview is more useful stale
                // than blank while a half-finished edit is in flight.
            }
        }
        if self.preview.is_none() {
            self.preview = self.compiled.clone().map(Preview::new);
        }
    }

    // ---------------------------------------------------------------- saving

    /// Write the skin, and the mood graph beside it.
    pub fn save(&mut self) -> Result<SaveReport, EditError> {
        let Some(path) = self.path.clone() else {
            return Err(EditError::Write(
                "this skin has never been saved — choose a file first".into(),
            ));
        };
        self.save_as(&path)
    }

    pub fn save_as(&mut self, path: &Path) -> Result<SaveReport, EditError> {
        let report = crate::save::write(path, &self.doc, self.source.as_deref())?;
        // What was just written becomes the source for the next save, so a
        // second save is byte-identical to the first.
        let (text, _) = self.to_toml()?;
        self.source = Some(text);
        self.path = Some(path.to_path_buf());
        self.history.mark_saved();

        if !self.graph.states.is_empty() {
            let gp = MoodGraph::path_for(path);
            let text = self.graph.to_toml()?;
            std::fs::write(&gp, text).map_err(|e| EditError::Write(e.to_string()))?;
        }
        self.status = match report.warning() {
            Some(w) => w,
            None => format!("saved {} comment blocks intact", report.carried),
        };
        self.last_save = Some(report.clone());
        Ok(report)
    }

    // ----------------------------------------------------------- the preview

    /// Pose her at the playhead. Call after moving the playhead or editing a
    /// clip; it is a replay, so it is the one call in the editor with a real
    /// cost.
    pub fn repose(&mut self) {
        let (layer, clip, t) = (self.timeline.layer, self.timeline.clip, self.timeline.playhead_ms);
        if let Some(p) = self.preview.as_mut() {
            p.seek(layer, clip, t);
        }
        if let (Some(p), Some(puppet)) = (self.preview.as_ref(), self.puppet.as_mut()) {
            puppet.sync(p.pose());
        }
    }

    /// Advance playback. The host owns the clock; this owns what to do with it.
    pub fn tick(&mut self, dt_ms: f32) {
        let duration = self
            .doc
            .clips
            .get(self.timeline.clip)
            .map(|c| c.duration_ms.0)
            .unwrap_or(0.0);
        if self.timeline.playing {
            self.timeline.tick(dt_ms, duration);
            self.repose();
        }
    }

    /// Frame the whole canvas.
    pub fn fit(&mut self) {
        let size = Vec2::new(self.doc.canvas.size[0].0, self.doc.canvas.size[1].0);
        self.view.rect = self.frames.canvas;
        self.view.fit(size, 32.0);
    }

    // ----------------------------------------------------------------- input

    /// Build the chrome and remember where everything is.
    pub fn build_panels(&mut self, bounds: Rect, text: &mut TextEngine) -> Panels {
        self.frames = crate::panels::frames(bounds);
        self.view.rect = self.frames.canvas;
        let state = PanelState {
            doc: &self.doc,
            selection: &self.selection,
            tool: self.tool,
            timeline: &self.timeline,
            can_undo: self.history.can_undo(),
            can_redo: self.history.can_redo(),
            dirty: self.history.dirty(),
            status: &self.status,
            problems: self.validation.problems.len(),
            show_graph: self.show_graph,
            graph: Some(&self.graph),
        };
        Panels::build(&state, bounds, text)
    }

    /// Draw everything into `scene`. The panels come from
    /// [`Editor::build_panels`]; the canvas, timeline and graph are drawn
    /// here because they are not made of widgets.
    ///
    /// Widget painting needs a `Painter` and therefore a GPU, so it stays the
    /// host's call: this draws the parts that are pure.
    pub fn draw_canvas(&mut self, sink: &mut impl TextSink, scene: &mut Scene) {
        // Everything below is in canvas coordinates and can run far outside
        // the viewport — she is drawn at whatever pan and zoom the operator
        // set. Without this clip she spills over the side panels, which is
        // exactly what happened the first time this was rendered and looked at.
        scene.push_clip(self.frames.canvas);
        crate::overlay::draw_ground(&self.doc, &self.view, scene);

        // Ghosts. She is an opaque creature, not a line drawing, so ghosts
        // *behind* her would be hidden by her own body — the classic onion
        // skin arrangement only works for outlines. They are drawn over her
        // at low alpha instead, which reads as a motion trail and is what an
        // animator is actually looking for here.
        let ghosts = {
            let duration = self
                .doc
                .clips
                .get(self.timeline.clip)
                .map(|c| c.duration_ms.0)
                .unwrap_or(0.0);
            let looping =
                self.doc.clips.get(self.timeline.clip).map(|c| c.looping).unwrap_or(true);
            self.timeline.onion.ghosts(self.timeline.playhead_ms, duration, looping)
        };
        let ghost_frames = if ghosts.is_empty() {
            Vec::new()
        } else {
            let (layer, clip) = (self.timeline.layer, self.timeline.clip);
            let out = self
                .preview
                .as_mut()
                .map(|p| p.ghosts(layer, clip, &ghosts))
                .unwrap_or_default();
            // The ghosts left the replay wherever the last one landed.
            self.repose();
            out
        };

        if let Some(p) = self.preview.as_ref() {
            crate::overlay::draw_frame(p.frame(), &self.view, 1.0, scene);
        }
        for g in &ghost_frames {
            crate::overlay::draw_frame(&g.frame, &self.view, g.alpha, scene);
        }

        // Chrome on top of her, never mixed into her.
        for i in 0..self.doc.shapes.len() {
            let selected = self.selection.contains(Target::Shape(i))
                || !self.selection.points_of(i).is_empty();
            if selected {
                crate::overlay::draw_rest_outline(&self.doc, i, &self.view, true, scene);
            }
        }
        if let Some(shape) = self.current_shape() {
            crate::overlay::draw_handles(&self.doc, shape, &self.view, &self.selection, scene);
            if self.tool == Tool::Weight {
                if let Some(b) = self.selection.bones().first().and_then(|i| self.doc.bones.get(*i))
                {
                    crate::overlay::draw_weights(&self.doc, shape, &b.name, &self.view, scene);
                }
            }
        }
        if let (Some(skin), Some(pose)) = (
            self.compiled.as_ref(),
            self.puppet
                .as_ref()
                .map(|p| p.pose())
                .or_else(|| self.preview.as_ref().map(|p| p.pose())),
        ) {
            crate::overlay::draw_bones(skin, pose, &self.view, &self.selection, scene);
            crate::overlay::draw_ik(skin, pose, &self.view, scene);
        }
        if let Some(Drag::Marquee { from }) = &self.drag {
            crate::overlay::draw_marquee(*from, self.last_pointer.unwrap_or(*from), scene);
        }
        scene.pop_clip();

        scene.push_clip(self.frames.timeline);
        self.timeline_hits = crate::panels::draw_timeline(
            &self.doc,
            &self.timeline,
            self.frames.timeline,
            sink,
            scene,
        );
        scene.pop_clip();

        if self.show_graph {
            scene.push_clip(self.frames.canvas);
            self.graph_hits = crate::panels::draw_graph(
                &self.graph,
                self.frames.canvas,
                (self.graph_view.0, self.graph_view.1),
                self.graph_view.2,
                &self.selection,
                sink,
                scene,
            );
            scene.pop_clip();
        }
    }

    /// Do what a chrome click asked for.
    pub fn perform(&mut self, action: Action) {
        match action {
            Action::SelectTool(t) => {
                self.tool = t;
                self.ik_pick.clear();
                self.status = t.hint().to_string();
            }
            Action::Select(t) => {
                if let Some(s) = t.owning_shape() {
                    self.active_shape = Some(s);
                }
                self.selection.set(t);
            }
            Action::Undo => {
                let _ = self.undo();
            }
            Action::Redo => {
                let _ = self.redo();
            }
            Action::Save => {
                let _ = self.save();
            }
            Action::FitView => self.fit(),
            Action::ZoomIn => self.view.wheel(self.frames.canvas.centre(), 1.0),
            Action::ZoomOut => self.view.wheel(self.frames.canvas.centre(), -1.0),
            Action::PlayPause => self.timeline.playing = !self.timeline.playing,
            Action::ToggleOnion => self.timeline.onion.enabled = !self.timeline.onion.enabled,
            Action::ToggleGraph => self.show_graph = !self.show_graph,
            Action::PickSwatch(i) => self.pick_swatch(i),
            Action::AddLitEdge => self.add_lit_edge(),
            Action::AddBone => self.add_bone_at_centre(),
            Action::AddShape => self.add_shape_at_centre(),
            Action::DeleteSelected => self.delete_selected(),
            Action::Scrub(ms) => {
                self.timeline.playhead_ms = ms;
                self.repose();
            }
        }
    }

    pub fn key(&mut self, key: Key) {
        match key {
            Key::Undo => {
                let _ = self.undo();
            }
            Key::Redo => {
                let _ = self.redo();
            }
            Key::Save => {
                let _ = self.save();
            }
            Key::Delete => self.delete_selected(),
            Key::Keyframe => self.keyframe_pose(),
            Key::PlayPause => self.timeline.playing = !self.timeline.playing,
            Key::ToggleOnion => self.timeline.onion.enabled = !self.timeline.onion.enabled,
            Key::ToggleGraph => self.show_graph = !self.show_graph,
            Key::NextFrame => self.step_frame(1.0),
            Key::PrevFrame => self.step_frame(-1.0),
            Key::Fit => self.fit(),
            Key::Escape => {
                self.drag = None;
                self.dragging = false;
                self.ik_pick.clear();
                self.history.end();
                self.status = "cancelled".into();
            }
            Key::Tool(t) => self.perform(Action::SelectTool(t)),
        }
    }

    fn step_frame(&mut self, dir: f32) {
        let duration = self
            .doc
            .clips
            .get(self.timeline.clip)
            .map(|c| c.duration_ms.0)
            .unwrap_or(0.0);
        // One display frame at 60fps, which is the grain the runtime shows.
        let step = 1000.0 / 60.0;
        self.timeline.playhead_ms =
            (self.timeline.playhead_ms + dir * step).clamp(0.0, duration.max(0.0));
        self.repose();
    }

    // ------------------------------------------------------------ the canvas

    /// A press. `mode` comes from the modifier keys.
    pub fn pointer_down(&mut self, at: Point, mode: SelectMode) {
        self.last_pointer = Some(at);
        self.press_at = Some(at);
        self.dragging = false;
        if self.frames.timeline.contains(at) {
            self.timeline_press(at, mode);
            return;
        }
        if !self.frames.canvas.contains(at) {
            return;
        }
        if self.show_graph {
            self.graph_press(at, mode);
            return;
        }
        self.history.begin();
        let p = self.view.to_canvas(at);
        match self.tool {
            Tool::Pan => self.drag = Some(Drag::Pan { from: at }),
            Tool::Select => self.select_press(at, p, mode),
            Tool::Pen => self.pen_press(p, at),
            Tool::Erase => self.erase_press(at),
            Tool::Bone => self.bone_press(at),
            Tool::Weight => self.weight_press(at),
            Tool::Ik => self.ik_press(at),
            Tool::Puppet => self.puppet_press(p),
        }
    }

    pub fn pointer_move(&mut self, at: Point) {
        let moved = self
            .press_at
            .map(|p| (p.x - at.x).abs() + (p.y - at.y).abs() > DRAG_SLOP_PX)
            .unwrap_or(false);
        if moved {
            self.dragging = true;
        }
        self.last_pointer = Some(at);
        if !self.dragging {
            return;
        }
        let p = self.view.to_canvas(at);
        match self.drag.clone() {
            Some(Drag::Pan { from }) => {
                self.view.pan_by_px(at.x - from.x, at.y - from.y);
                self.drag = Some(Drag::Pan { from: at });
            }
            Some(Drag::Points { shape, points, grab }) => {
                let delta = p - grab;
                if let Ok(cmd) = crate::canvas::move_points(&self.doc, shape, &points, delta) {
                    let _ = self.apply(cmd);
                }
                self.drag = Some(Drag::Points { shape, points, grab: p });
            }
            Some(Drag::Bone { bone }) => {
                if let Ok(cmd) = crate::bones::move_bone(&self.doc, bone, p) {
                    let _ = self.apply(cmd);
                }
            }
            Some(Drag::Weight { bone }) => self.paint_weights_at(p, bone),
            Some(Drag::Puppet) => {
                if let (Some(skin), Some(puppet)) = (self.compiled.as_ref(), self.puppet.as_mut()) {
                    if let Some(g) = puppet.drag_to(skin, p) {
                        if !self.puppet_grips.contains(&g) {
                            self.puppet_grips.push(g);
                        }
                    }
                }
            }
            Some(Drag::Scrub) => {
                let duration = self
                    .doc
                    .clips
                    .get(self.timeline.clip)
                    .map(|c| c.duration_ms.0)
                    .unwrap_or(0.0);
                let ruler_x = self.frames.timeline.x + crate::panels::GUTTER_W;
                self.timeline.scrub_to_px(at.x, ruler_x, duration);
                self.repose();
            }
            Some(Drag::GraphNode { state, grab }) => {
                let (gx, gy) = self.graph_point(at);
                let _ = self.apply_graph(GraphCommand::MoveState {
                    at: state,
                    x: gx - grab.0,
                    y: gy - grab.1,
                });
            }
            Some(Drag::Marquee { .. }) | Some(Drag::NewBone { .. }) | None => {}
        }
    }

    pub fn pointer_up(&mut self, at: Point) {
        self.last_pointer = Some(at);
        let drag = self.drag.take();
        let was_drag = self.dragging;
        self.dragging = false;
        self.press_at = None;
        match drag {
            Some(Drag::Marquee { from }) if was_drag => self.marquee_select(from, at),
            Some(Drag::NewBone { parent }) if was_drag => {
                let p = self.view.to_canvas(at);
                let name = self.next_bone_name(parent);
                let parent_name = self.doc.bones[parent].name.clone();
                match crate::bones::drag_out_child(&self.doc, &parent_name, &name, p) {
                    Ok(cmd) => {
                        let _ = self.apply(cmd);
                        let i = self.doc.bones.len() - 1;
                        self.selection.set(Target::Bone(i));
                    }
                    Err(e) => self.status = e.message(),
                }
            }
            Some(Drag::Puppet) => {
                if let Some(p) = self.puppet.as_mut() {
                    p.end();
                }
                self.status = "posed — press K to keyframe it".into();
            }
            _ => {}
        }
        self.history.end();
        self.graph_history.end();
    }

    pub fn wheel(&mut self, at: Point, notches: f32) {
        if self.frames.timeline.contains(at) {
            self.timeline.scale_px_per_ms =
                (self.timeline.scale_px_per_ms * 1.15f32.powf(notches)).clamp(0.01, 4.0);
            return;
        }
        if self.show_graph && self.frames.canvas.contains(at) {
            self.graph_view.2 = (self.graph_view.2 * 1.15f32.powf(notches)).clamp(0.2, 4.0);
            return;
        }
        if self.frames.canvas.contains(at) {
            self.view.wheel(at, notches);
        }
    }

    // ---------------------------------------------------------- tool presses

    fn select_press(&mut self, at: Point, p: Vec2, mode: SelectMode) {
        // Bone handles win over path points when the bone tool's siblings are
        // not armed: they are drawn on top and they are what puppet mode and
        // the tree both talk about.
        if let (Some(skin), Some(pose)) =
            (self.compiled.as_ref(), self.preview.as_ref().map(|pv| pv.pose()))
        {
            let r = self.view.grab_radius();
            let mut best: Option<(usize, f32)> = None;
            for i in 0..skin.skeleton.len() {
                let d = pose.world_pos(i).dist(p);
                if d <= r && best.map(|(_, bd)| d < bd).unwrap_or(true) {
                    best = Some((i, d));
                }
            }
            if let Some((bone, _)) = best {
                self.selection.apply(Target::Bone(bone), mode);
                self.drag = Some(Drag::Bone { bone });
                return;
            }
        }
        match crate::canvas::pick(&self.doc, &self.view, at, &self.selection) {
            crate::canvas::Hit::Point { shape, point, .. } => {
                let target = Target::Point { shape, point };
                if !self.selection.contains(target) {
                    self.selection.apply(target, mode);
                }
                self.active_shape = Some(shape);
                let points = {
                    let mut v = self.selection.points_of(shape);
                    if v.is_empty() {
                        v.push(point);
                    }
                    v
                };
                self.drag = Some(Drag::Points { shape, points, grab: p });
            }
            crate::canvas::Hit::Shape(shape) => {
                self.selection.apply(Target::Shape(shape), mode);
                self.active_shape = Some(shape);
                let points = crate::canvas::points_of(&self.doc, shape)
                    .map(|v| (0..v.len()).collect::<Vec<_>>())
                    .unwrap_or_default();
                self.drag = Some(Drag::Points { shape, points, grab: p });
            }
            _ => {
                if mode == SelectMode::Replace {
                    self.selection.clear();
                }
                self.drag = Some(Drag::Marquee { from: at });
            }
        }
    }

    fn pen_press(&mut self, p: Vec2, at: Point) {
        let Some(shape) = self.current_shape() else {
            self.status = "pick a shape first, or press add shape".into();
            return;
        };
        if let crate::canvas::Hit::Segment { shape: s, after_point, at: on, .. } =
            crate::canvas::hit_segment(&self.doc, &self.view, at)
        {
            if s == shape {
                match crate::canvas::split_segment(&self.doc, shape, after_point, on) {
                    Ok(cmd) => {
                        let _ = self.apply(cmd);
                        self.selection.set(Target::Point { shape, point: after_point + 1 });
                    }
                    Err(e) => self.status = e.message(),
                }
                return;
            }
        }
        match crate::canvas::append_point(&self.doc, shape, p) {
            Ok(cmd) => {
                let _ = self.apply(cmd);
                if let Ok(pts) = crate::canvas::points_of(&self.doc, shape) {
                    self.selection.set(Target::Point { shape, point: pts.len() - 1 });
                }
            }
            Err(e) => self.status = e.message(),
        }
    }

    fn erase_press(&mut self, at: Point) {
        match crate::canvas::pick(&self.doc, &self.view, at, &self.selection) {
            crate::canvas::Hit::Point { shape, point, .. } => {
                match crate::canvas::delete_point(&self.doc, shape, point) {
                    Ok(cmd) => {
                        let _ = self.apply(cmd);
                    }
                    Err(e) => self.status = e.message(),
                }
            }
            crate::canvas::Hit::Shape(shape) => {
                let _ = self.apply(Command::RemoveShape { at: shape });
            }
            _ => {}
        }
    }

    fn bone_press(&mut self, at: Point) {
        let p = self.view.to_canvas(at);
        let Some(skin) = self.compiled.as_ref() else { return };
        let Some(pose) = self.preview.as_ref().map(|pv| pv.pose()) else { return };
        let r = self.view.grab_radius();
        let mut best: Option<(usize, f32)> = None;
        for i in 0..skin.skeleton.len() {
            let d = pose.world_pos(i).dist(p).min(pose.world_tip(&skin.skeleton, i).dist(p));
            if d <= r && best.map(|(_, bd)| d < bd).unwrap_or(true) {
                best = Some((i, d));
            }
        }
        match best {
            Some((bone, _)) => {
                self.selection.set(Target::Bone(bone));
                self.drag = Some(Drag::NewBone { parent: bone });
                self.status = "drag out a child bone".into();
            }
            None => self.status = "start a bone by dragging from an existing one".into(),
        }
    }

    fn weight_press(&mut self, at: Point) {
        let Some(&bone) = self.selection.bones().first() else {
            self.status = "select the bone you want to paint towards".into();
            return;
        };
        self.drag = Some(Drag::Weight { bone });
        self.paint_weights_at(self.view.to_canvas(at), bone);
    }

    fn paint_weights_at(&mut self, p: Vec2, bone: usize) {
        let Some(shape) = self.current_shape() else {
            self.status = "select the shape you want to paint on".into();
            return;
        };
        let Ok(points) = crate::canvas::points_of(&self.doc, shape) else { return };
        let Some(b) = self.doc.bones.get(bone) else { return };
        let name = b.name.clone();
        // The brush is a screen-sized disc, so it feels the same at every zoom.
        let radius = self.view.to_canvas_len(28.0);
        let mut cmds = Vec::new();
        for (i, q) in points.iter().enumerate() {
            let d = q.dist(p);
            if d > radius {
                continue;
            }
            let amount = crate::bones::falloff_weight(d, radius);
            if amount <= 0.01 {
                continue;
            }
            if let Ok(cmd) = crate::bones::paint_weight(&self.doc, shape, i, &name, amount) {
                cmds.push(cmd);
            }
        }
        if !cmds.is_empty() {
            let _ = self.apply(Command::Batch { label: "paint weights", cmds });
        }
    }

    fn ik_press(&mut self, at: Point) {
        let p = self.view.to_canvas(at);
        let Some(skin) = self.compiled.as_ref() else { return };
        let Some(pose) = self.preview.as_ref().map(|pv| pv.pose()) else { return };
        let r = self.view.grab_radius();
        // Bones already in the chain are excluded rather than silently
        // re-picked: in this rig several joints sit on top of each other, and
        // "click three bones and nothing happens" is the worst possible
        // outcome of a three-click gesture.
        let mut best: Option<(usize, f32)> = None;
        for i in 0..skin.skeleton.len() {
            if self.ik_pick.contains(&i) {
                continue;
            }
            let d = pose.world_pos(i).dist(p);
            if d <= r && best.map(|(_, bd)| d < bd).unwrap_or(true) {
                best = Some((i, d));
            }
        }
        let Some((bone, _)) = best else {
            self.status = if self.ik_pick.is_empty() {
                "click a bone".into()
            } else {
                "that bone is already in the chain — pick another".to_string()
            };
            return;
        };
        self.ik_pick.push(bone);
        self.selection.set(Target::Bone(bone));
        match self.ik_pick.len() {
            1 => self.status = "root picked — now the middle bone".into(),
            2 => self.status = "middle picked — now the tip".into(),
            _ => {
                let names: Vec<String> = self
                    .ik_pick
                    .iter()
                    .map(|i| self.doc.bones[*i].name.clone())
                    .collect();
                let name = format!("{}_ik", names[2]);
                match crate::bones::add_two_bone(
                    &self.doc, &name, &names[0], &names[1], &names[2], "none",
                ) {
                    Ok(cmd) => {
                        let _ = self.apply(cmd);
                        self.status = format!("placed the IK chain {name:?}");
                    }
                    Err(e) => self.status = e.message(),
                }
                self.ik_pick.clear();
            }
        }
    }

    fn puppet_press(&mut self, p: Vec2) {
        let Some(skin) = self.compiled.clone() else { return };
        if self.puppet.is_none() {
            if let Some(pv) = self.preview.as_ref() {
                self.puppet = Some(Puppet::new(pv.pose().clone()));
            }
        }
        let Some(puppet) = self.puppet.as_mut() else { return };
        let r = self.view.grab_radius().max(6.0);
        let Some(bone) = puppet.hit_bone(&skin, p, r) else {
            self.status = "grab one of her bones".into();
            return;
        };
        if puppet.begin(&skin, bone, p).is_ok() {
            self.selection.set(Target::Bone(bone));
            self.drag = Some(Drag::Puppet);
            self.status = crate::puppet::grip_for(&skin, bone).describe().to_string();
        }
    }

    /// Turn the puppeted pose into keyframes at the playhead.
    pub fn keyframe_pose(&mut self) {
        let (Some(skin), Some(puppet)) = (self.compiled.clone(), self.puppet.as_ref()) else {
            self.status = "there is no posed frame to keyframe".into();
            return;
        };
        if !puppet.is_posed() {
            self.status = "pose her first — drag a bone in puppet mode".into();
            return;
        }
        let grips = self.puppet_grips.clone();
        match puppet.keys(&self.doc, &skin, self.timeline.clip, self.timeline.playhead_ms, &grips) {
            Ok(cmd) => {
                let _ = self.apply(cmd);
                self.status = "keyframed the pose".into();
            }
            Err(e) => self.status = e.message(),
        }
    }

    fn marquee_select(&mut self, from: Point, to: Point) {
        let a = self.view.to_canvas(from);
        let b = self.view.to_canvas(to);
        let (lo, hi) = (a.min(b), a.max(b));
        let mut picked: Vec<Target> = Vec::new();
        for shape in 0..self.doc.shapes.len() {
            let Ok(points) = crate::canvas::points_of(&self.doc, shape) else { continue };
            for (i, p) in points.iter().enumerate() {
                if p.x >= lo.x && p.x <= hi.x && p.y >= lo.y && p.y <= hi.y {
                    picked.push(Target::Point { shape, point: i });
                }
            }
        }
        if picked.is_empty() {
            self.selection.clear();
        } else {
            self.selection.set_many(picked);
        }
    }

    // -------------------------------------------------------- other surfaces

    fn timeline_press(&mut self, at: Point, mode: SelectMode) {
        if let Some((_, target)) =
            self.timeline_hits.iter().find(|(r, _)| r.contains(at)).cloned()
        {
            self.selection.apply(target, mode);
            if let Target::Key { clip, track, key } = target {
                if let Some(t) =
                    self.doc.clips.get(clip).and_then(|c| c.tracks.get(track))
                {
                    if let Some(ms) = t.t.get(key) {
                        self.timeline.playhead_ms = ms.0;
                        self.repose();
                    }
                }
            }
            return;
        }
        let ruler_x = self.frames.timeline.x + crate::panels::GUTTER_W;
        if at.x >= ruler_x {
            let duration = self
                .doc
                .clips
                .get(self.timeline.clip)
                .map(|c| c.duration_ms.0)
                .unwrap_or(0.0);
            self.timeline.scrub_to_px(at.x, ruler_x, duration);
            self.repose();
            self.drag = Some(Drag::Scrub);
        }
    }

    fn graph_point(&self, at: Point) -> (f32, f32) {
        let r = self.frames.canvas;
        let z = self.graph_view.2.max(1e-3);
        (
            self.graph_view.0 + (at.x - r.x - r.w * 0.5) / z,
            self.graph_view.1 + (at.y - r.y - r.h * 0.5) / z,
        )
    }

    fn graph_press(&mut self, at: Point, mode: SelectMode) {
        if let Some((_, target)) = self.graph_hits.iter().find(|(r, _)| r.contains(at)).cloned() {
            self.selection.apply(target, mode);
            if let Target::State(i) = target {
                let (gx, gy) = self.graph_point(at);
                let s = &self.graph.states[i];
                self.graph_history.begin();
                self.drag = Some(Drag::GraphNode { state: i, grab: (gx - s.x, gy - s.y) });
            }
            return;
        }
        if mode == SelectMode::Replace {
            self.selection.clear();
        }
    }

    // ---------------------------------------------------------- chrome verbs

    fn pick_swatch(&mut self, i: usize) {
        let Some(sw) = crate::swatch::NX_SWATCHES.get(i) else { return };
        let Some(shape) = self.current_shape() else {
            self.status = "select a shape to paint it".into();
            return;
        };
        // Prefer the named colour if the skin has it, so the file stays
        // readable instead of collecting hex literals.
        let named = self.doc.colors.iter().any(|c| c.name == sw.name);
        let value = if named { sw.name } else { sw.hex };
        match crate::canvas::set_fill_color(&self.doc, shape, value) {
            Ok(cmd) => {
                let _ = self.apply(cmd);
                self.status = format!("filled with {} — {}", sw.name, sw.role);
            }
            Err(e) => self.status = e.message(),
        }
    }

    fn add_lit_edge(&mut self) {
        let Some(shape) = self.current_shape() else {
            self.status = "select a shape to give it an edge".into();
            return;
        };
        let name = format!("{}_edge", self.doc.shapes[shape].name);
        match crate::swatch::lit_edge(&self.doc, shape, &name) {
            Ok(cmd) => {
                let _ = self.apply(cmd);
            }
            Err(e) => self.status = e.message(),
        }
    }

    fn next_bone_name(&self, parent: usize) -> String {
        let base = self.doc.bones.get(parent).map(|b| b.name.as_str()).unwrap_or("bone");
        for n in 1..1000 {
            let candidate = format!("{base}_{n}");
            if !self.doc.bones.iter().any(|b| b.name == candidate) {
                return candidate;
            }
        }
        format!("{base}_{}", self.doc.bones.len())
    }

    fn add_bone_at_centre(&mut self) {
        let centre = self.view.to_canvas(self.frames.canvas.centre());
        let parent = self
            .selection
            .bones()
            .first()
            .and_then(|i| self.doc.bones.get(*i))
            .map(|b| b.name.clone())
            .unwrap_or_default();
        let name = match self.selection.bones().first() {
            Some(&i) => self.next_bone_name(i),
            None => {
                let mut n = self.doc.bones.len();
                loop {
                    let c = format!("bone_{n}");
                    if !self.doc.bones.iter().any(|b| b.name == c) {
                        break c;
                    }
                    n += 1;
                }
            }
        };
        match crate::bones::add_bone(&self.doc, &name, &parent, centre) {
            Ok(cmd) => {
                let _ = self.apply(cmd);
                self.selection.set(Target::Bone(self.doc.bones.len() - 1));
            }
            Err(e) => self.status = e.message(),
        }
    }

    fn add_shape_at_centre(&mut self) {
        let centre = self.view.to_canvas(self.frames.canvas.centre());
        let r = (self.doc.canvas.size[0].0.min(self.doc.canvas.size[1].0) * 0.15).max(4.0);
        let path = crate::canvas::blob(centre, r);
        let mut n = self.doc.shapes.len();
        let name = loop {
            let c = format!("shape_{n}");
            if !self.doc.shapes.iter().any(|s| s.name == c) {
                break c;
            }
            n += 1;
        };
        let fill = if self.doc.colors.iter().any(|c| c.name == "violet") {
            "violet"
        } else {
            "#7700ff"
        };
        let cmd = crate::canvas::new_shape(&self.doc, &name, &path, fill);
        let _ = self.apply(cmd);
        let i = self.doc.shapes.len() - 1;
        self.active_shape = Some(i);
        self.selection.set(Target::Shape(i));
    }

    fn delete_selected(&mut self) {
        let targets: Vec<Target> = self.selection.iter().collect();
        let mut cmds: Vec<Command> = Vec::new();
        // Highest index first everywhere, so a removal never shifts the next.
        let mut points: Vec<(usize, usize)> = Vec::new();
        let mut shapes: Vec<usize> = Vec::new();
        let mut bones: Vec<usize> = Vec::new();
        let mut keys: Vec<(usize, usize, usize)> = Vec::new();
        let mut states: Vec<usize> = Vec::new();
        for t in targets {
            match t {
                Target::Point { shape, point } => points.push((shape, point)),
                Target::Shape(i) => shapes.push(i),
                Target::Bone(i) => bones.push(i),
                Target::Key { clip, track, key } => keys.push((clip, track, key)),
                Target::State(i) => states.push(i),
                _ => {}
            }
        }
        points.sort_by(|a, b| b.cmp(a));
        shapes.sort_by(|a, b| b.cmp(a));
        bones.sort_by(|a, b| b.cmp(a));
        keys.sort_by(|a, b| b.cmp(a));

        for (clip, track, key) in keys {
            match crate::timeline::delete_key(&self.doc, clip, track, key) {
                Ok(c) => cmds.push(c),
                Err(e) => self.status = e.message(),
            }
        }
        for (shape, point) in points {
            match crate::canvas::delete_point(&self.doc, shape, point) {
                Ok(c) => cmds.push(c),
                Err(e) => self.status = e.message(),
            }
        }
        for i in shapes {
            cmds.push(Command::RemoveShape { at: i });
        }
        for i in bones {
            match crate::bones::delete_bone(&self.doc, i) {
                Ok(c) => cmds.push(c),
                Err(e) => self.status = e.message(),
            }
        }
        if !cmds.is_empty() {
            let _ = self.apply(Command::Batch { label: "delete", cmds });
            self.selection.clear();
        }
        states.sort_by(|a, b| b.cmp(a));
        for i in states {
            if let Ok(c) = crate::graph::delete_state(&self.graph, i) {
                let _ = self.apply_graph(c);
            }
        }
    }

    /// Reparent a bone from the tree panel, surfacing the refusal rather than
    /// silently doing nothing.
    pub fn reparent(&mut self, bone: usize, parent: &str) -> Result<(), EditError> {
        let cmd = crate::bones::reparent(&self.doc, bone, parent).inspect_err(|e| {
            self.status = e.message();
        })?;
        self.apply(cmd)
    }

    /// Switch which clip the timeline edits.
    pub fn open_clip(&mut self, clip: usize) {
        if clip >= self.doc.clips.len() {
            return;
        }
        self.timeline.clip = clip;
        self.timeline.playhead_ms = 0.0;
        self.selection.set(Target::Clip(clip));
        self.repose();
    }

    /// Open the clip an expression plays, so all eight are one click away.
    pub fn open_expression(&mut self, expression: usize) -> Result<(), EditError> {
        let e = self.doc.expressions.get(expression).ok_or(EditError::NoSuchIndex {
            kind: "expression",
            at: expression,
            len: self.doc.expressions.len(),
        })?;
        let clip = e.clip.clone();
        let i = crate::timeline::clip_index(&self.doc, &clip)
            .ok_or(EditError::NoSuchName { kind: "clip", name: clip })?;
        self.open_clip(i);
        Ok(())
    }
}

/// Does this target still address something the document has?
fn valid_target(doc: &SkinDoc, t: Target) -> bool {
    match t {
        Target::Shape(i) => i < doc.shapes.len(),
        Target::Point { shape, point } => doc
            .shapes
            .get(shape)
            .and_then(|_| crate::canvas::points_of(doc, shape).ok())
            .map(|p| point < p.len())
            .unwrap_or(false),
        Target::Bone(i) => i < doc.bones.len(),
        Target::Gradient(i) => i < doc.gradients.len(),
        Target::GradientStop { gradient, stop } => {
            doc.gradients.get(gradient).map(|g| stop < g.stop_at.len()).unwrap_or(false)
        }
        Target::Ik(i) => i < doc.iks.len(),
        Target::Chain(i) => i < doc.chains.len(),
        Target::Layer(i) => i < doc.layers.len(),
        Target::Clip(i) => i < doc.clips.len(),
        Target::Expression(i) => i < doc.expressions.len(),
        Target::Key { clip, track, key } => doc
            .clips
            .get(clip)
            .and_then(|c| c.tracks.get(track))
            .map(|t| key < t.t.len())
            .unwrap_or(false),
        // The graph is a separate document; the skin cannot invalidate it.
        Target::State(_) | Target::Transition(_) => true,
    }
}
