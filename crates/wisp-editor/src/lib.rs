//! **wisp-editor** — the in-app rig editor (F76).
//!
//! SPEC.md §2 gives this crate the editor and lets it depend on `wisp-proto`,
//! `wisp-rig`, `wisp-paint` and `wisp-theme`. It owns no other crate's files
//! and it changes neither the rig engine nor the skin format.
//!
//! # It is a library, and it never opens a window
//!
//! This is **not** an application. There is no `main`, no event loop, no
//! surface, no clock. [`editor::Editor`] is a pure state machine plus scene
//! building; the host mounts it, feeds it input, and renders what comes back.
//! That is what lets the whole editor — every tool, every gesture, the graph,
//! the save path — be driven by `cargo test` with no GPU and no compositor.
//!
//! ```no_run
//! use wisp_editor::editor::{Editor, Key};
//! use wisp_editor::select::SelectMode;
//! use wisp_paint::{geom::{Point, Rect}, scene::Scene, text::TextEngine};
//!
//! let mut editor = Editor::default_skin().expect("the shipped skin opens");
//! let mut engine = TextEngine::new();
//! let bounds = Rect::from_size(1440.0, 900.0);
//!
//! // Once, when the panel is mounted.
//! editor.build_panels(bounds, &mut engine);
//! editor.fit();
//!
//! // Every frame the host draws.
//! let panels = editor.build_panels(bounds, &mut engine);
//! let mut scene = Scene::new();
//! // panels.ui.paint(painter, &mut engine, &mut scene);   // widgets: needs a GPU
//! // editor.draw_canvas(&mut Live::new(painter, &mut engine), &mut scene);
//!
//! // Input the host forwards.
//! editor.pointer_down(Point::new(400.0, 300.0), SelectMode::Replace);
//! editor.pointer_up(Point::new(420.0, 310.0));
//! editor.key(Key::Save);
//! ```
//!
//! # The whole feature, by module
//!
//! | Module | What it owns |
//! |---|---|
//! | [`cmd`] | Every mutation as one reversible value; apply returns the inverse |
//! | [`history`] | Undo/redo, gesture coalescing, the save watermark |
//! | [`select`] | What is selected, and what the pointer currently does |
//! | [`view`] | Pan, zoom, and the canvas ↔ pixel conversion |
//! | [`canvas`] | Path points: read, hit-test, move, split, delete |
//! | [`bones`] | The bone tree, cycle refusal, weight painting, IK placement |
//! | [`timeline`] | Keyframes, scrub, loop, onion skin |
//! | [`preview`] | Posing her at an absolute time, against the shipping rig |
//! | [`puppet`] | Dragging her limbs, and turning the pose into keyframes |
//! | [`graph`] | The mood/behaviour state machine, as a drawn document |
//! | [`swatch`] | NX palette, gradient stops, the lit-edge helper |
//! | [`save`] | Writing the skin back **with the author's comments intact** |
//! | [`overlay`] | Drawing her and the canvas chrome into a `Scene` |
//! | [`panels`] | The panels, and what clicking them means |
//! | [`text`] | `TextSink`: real shaping, optional rasterising |
//! | [`editor`] | All of the above, wired together |
//!
//! # Three decisions worth knowing before changing anything
//!
//! ## 1. The editor has no privileged path
//!
//! It reads and writes the same declarative format a third-party pack uses
//! (F49). The shipped skin is opened through `wisp-rig`'s ordinary parser and
//! written back through the ordinary serialiser, and `tests/render.rs` proves
//! it pixel-for-pixel: render, save, reload the saved bytes through
//! `Skin::load`, render again, and the two images are byte-identical.
//!
//! ## 2. Comments survive a save
//!
//! `Skin::to_toml` cannot keep them — serde has nowhere to put a comment. The
//! shipped skin is 2453 lines of which **651 are comments**, and they record
//! two complete character designs that were built and rejected, and why. So
//! [`save`] merges them back structurally, matching each `[[bone]]`,
//! `[[shape]]` and `[[clip]]` by name, and reports by name any comment whose
//! subject the operator deleted. Preserve by default; be loud about what could
//! not be preserved.
//!
//! ## 3. She is round; the editor is not
//!
//! SPEC §3.5b. The artwork is drawn exactly as the rig produced it and no
//! editor styling touches it. The chrome around it — panels, handles, bone
//! gizmos, the timeline — is angular, opaque and made of `wisp-theme` tokens,
//! because the chrome *is* governed by DESIGN.md's geometry rule.

pub mod bones;
pub mod canvas;
pub mod cmd;
pub mod editor;
pub mod error;
pub mod graph;
pub mod history;
pub mod overlay;
pub mod panels;
pub mod preview;
pub mod puppet;
pub mod save;
pub mod select;
pub mod swatch;
pub mod text;
pub mod timeline;
pub mod view;

pub use cmd::Command;
pub use editor::{Editor, Key};
pub use error::{EditError, Validation};
pub use graph::{GraphCommand, MoodGraph};
pub use history::{History, Reversible};
pub use panels::{Action, Frames, PanelState, Panels};
pub use preview::Preview;
pub use puppet::{Grip, Puppet};
pub use save::SaveReport;
pub use select::{SelectMode, Selection, Target, Tool};
pub use text::{Dry, Live, TextSink};
pub use timeline::{Onion, TimelineState};
pub use view::Viewport;

/// Re-exported so a host gets the same rig this editor was written against.
pub use wisp_rig as rig;

/// What `wisp --edit` needs from the host, written down so the wiring is a
/// checklist rather than an archaeology exercise.
///
/// The editor is mounted, not launched:
///
/// 1. Build it — [`Editor::default_skin`], [`Editor::open`] or
///    [`Editor::blank`].
/// 2. Every frame: [`Editor::build_panels`] with the panel's rectangle, then
///    paint the returned `Ui` and call [`Editor::draw_canvas`].
/// 3. Forward pointer events to [`Editor::pointer_down`],
///    [`Editor::pointer_move`], [`Editor::pointer_up`] and [`Editor::wheel`],
///    and chrome clicks through [`Panels::click`] into [`Editor::perform`].
/// 4. Forward keys as [`Key`].
/// 5. Call [`Editor::tick`] with the frame delta so playback advances.
/// 6. Before saving, show [`Editor::save_preview`]'s warning if there is one.
///
/// The editor never reads the clock, never touches the filesystem except in
/// [`Editor::open`] and [`Editor::save_as`], and never opens a surface.
pub const MOUNT_CONTRACT: &str = "see the module docs on wisp_editor::MOUNT_CONTRACT";
