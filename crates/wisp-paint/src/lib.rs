//! **wisp-paint** — the GPU renderer and the widget layer.
//!
//! SPEC.md §2 gives this crate the wgpu device and surface, the 2D scene
//! building, the widget layer, text, and the sprite-atlas baker. Everything
//! visual it draws comes from `wisp-theme`; everything it costs is reachable
//! by the governor through [`wisp_proto::Governed`].
//!
//! ## The shape of a frame
//!
//! ```text
//!   widget::Ui  ──layout──▶  Scene  ──Painter::render──▶  pixels
//!   (retained)               (per-frame command list)
//! ```
//!
//! A [`Scene`] is a flat command list. [`Painter`] tessellates it with lyon,
//! compiles each paint into a GPU record, batches, and draws into a
//! supersampled scratch texture which is resolved into the target. Floating
//! layers get a real backdrop blur by snapshotting that scratch texture and
//! running a separable Gaussian over it.
//!
//! ## Why not vello
//!
//! The plan (§1.2) names vello. **vello 0.10 — the current release — pins
//! `wgpu = "29.0.3"`, which is semver-incompatible with this workspace's
//! `wgpu = "30.0"`.** Two wgpu versions cannot share a device, a surface or a
//! queue, so a vello `Renderer` simply cannot be handed the swapchain the M0
//! spike proved out. Downgrading wgpu was not an option: the layer-shell
//! surface, the alpha mode and the readback path are all built against 30. So
//! the vector path is lyon tessellation plus our own pipelines, behind the same
//! `Scene`/`Painter` API vello would present. See [`tess`] for the swap notes.
//!
//! One thing that fell out well: there is **no compute pass anywhere in this
//! crate**, which is what SPEC §3.1's T3 asks of the rig.
//!
//! ## The governor
//!
//! [`Painter`] implements [`wisp_proto::Governed`]:
//!
//! | Tier | fps | What draws |
//! |---|---|---|
//! | T0 Feral / T1 Full | 60 | full vector, 2× supersampled, real blur |
//! | T2 Reduced | 30 | full vector, 1× |
//! | T3 Lobotomised | 15 | sprite-atlas quads only — no tessellation, no blur |
//! | T4 Dormant | — | nothing; the surface still gets a transparent frame |
//!
//! Downgrades apply to the very next frame, which satisfies SPEC §3.1's
//! "synchronously and immediately": a frame is the unit of work here, so there
//! is nothing to shed asynchronously.
//!
//! ## Testing
//!
//! Every GPU test renders **offscreen** and asserts on read-back pixels — SPEC
//! §4, and the M0 spike's proven pattern. No test opens a window.

pub mod adapter;
pub mod atlas;
pub mod error;
pub mod geom;
pub mod paint;
pub mod painter;
pub mod pipelines;
pub mod scene;
pub mod tess;
pub mod text;
pub mod texture;
pub mod widget;

pub use adapter::{AdapterPreference, AdapterSummary, DeviceKind};
pub use atlas::{Atlas, BakeItem, Sprite};
pub use error::{PaintError, Result};
pub use geom::{Path, PathBuilder, Point, Rect};
pub use paint::Paint;
pub use painter::{DrawMode, Image, Offscreen, Painter};
pub use scene::{Cmd, Scene};
pub use text::{TextEngine, TextRun};
pub use texture::Texture;
pub use widget::{Align, Axis, Icon, NodeId, Size, Sizing, Ui, Widget};

/// Re-exported so downstream crates get the same theme this renderer was
/// written against, without a second dependency edge to keep in sync.
pub use wisp_theme as theme;
