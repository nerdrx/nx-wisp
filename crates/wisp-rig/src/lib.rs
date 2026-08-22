//! **wisp-rig** — the skeletal 2D animation system and the skin format.
//!
//! SPEC.md §2 gives this crate the skeleton, mesh deform, IK, clip playback and
//! skin format parse/serialise. It implements F10 and F67–F75 of the plan (the
//! form) and F49 (the skin format), and it owns SPEC §3.6: **`wisp-rig` is the
//! only crate that may change the skin format.**
//!
//! # It is pure
//!
//! Nothing here touches a GPU, a compositor, a clock or the network. The rig
//! computes geometry — paths, transforms, vertices — and hands it out through
//! [`frame::RigFrame`], this crate's own small output type. The renderer maps
//! that onto vello; the shell decides where she is on screen and when to draw.
//! Consequences worth stating plainly:
//!
//! * `cargo test -p wisp-rig` runs anywhere, with no GPU and no Wayland
//!   session, which is what SPEC §4 asks of every pure module.
//! * `wisp-rig` deliberately does **not** depend on `wisp-paint` or
//!   `wisp-theme`, so neither crate blocks the other.
//! * Every simulation is a deterministic `step`: the same inputs give the same
//!   trajectory on any machine, so motion quality is *tested*, not reviewed by
//!   eye.
//!
//! # The shape of a frame
//!
//! ```no_run
//! use wisp_rig::{rig::{Rig, RigInput}, skin, contour::ContourOptions, math::Vec2};
//!
//! let mut rig = Rig::new(skin::default_skin().expect("the shipped skin compiles"));
//! rig.set_expression("curious");
//!
//! let input = RigInput {
//!     size_px: 128.0,
//!     anchor: Vec2::new(900.0, 540.0),
//!     cursor: Some(Vec2::new(1200.0, 400.0)),
//!     ..Default::default()
//! };
//! rig.update(1.0 / 60.0, &input);
//!
//! for shape in &rig.frame().shapes {
//!     // hand `shape.verbs`, `shape.points`, `shape.fill` to the renderer
//! }
//! // ...and this is the Wayland input region, so clicks pass through
//! // everywhere she is not (F2).
//! let outline = rig.contour(ContourOptions::default());
//! ```
//!
//! # Units and conventions
//!
//! * **Canvas units** are the space artwork is authored in; a skin declares the
//!   canvas size and every coordinate in the file is in it. Scaling the canvas
//!   onto a pixel size is what makes her resolution independent (F75).
//! * **Surface pixels**, y down, matching Wayland. A [`frame::RigFrame`] is
//!   already in them.
//! * A **skin file** authors degrees and milliseconds, because that is what a
//!   person types. Compilation converts to radians and seconds once, and
//!   everything at runtime is radians and seconds.
//!
//! # Module map
//!
//! | Module | What it owns |
//! |---|---|
//! | [`math`] | `Vec2`, `Affine`, `Rect` — the only geometry primitives |
//! | [`ease`] | The NX motion tokens as curves, plus the spring-damper |
//! | [`path`] | Vector paths and the SVG-subset parser |
//! | [`paint`] | Colour, gradients, strokes — output description |
//! | [`skeleton`] | The bone tree and pose resolution |
//! | [`deform`] | Mesh binding and linear blend skinning |
//! | [`ik`] | Two-bone IK and the look-at constraint |
//! | [`clip`] | Keyframed bone-channel tracks |
//! | [`player`] | Layered playback, cross-fades, additive layers |
//! | [`motion`] | Squash and stretch, overshoot, secondary motion |
//! | [`physics`] | Gravity, throwing, landing — one pure step function |
//! | [`contour`] | Silhouette to click-through polygon |
//! | [`skin`] | The skin format: parse, validate, compile, serialise |
//! | [`frame`] | What the renderer is handed |
//! | [`rig`] | All of the above, wired together |

pub mod clip;
pub mod contour;
pub mod deform;
pub mod ease;
pub mod frame;
pub mod ik;
pub mod math;
pub mod motion;
pub mod paint;
pub mod path;
pub mod physics;
pub mod player;
pub mod rig;
pub mod skeleton;
pub mod skin;

pub use clip::{Clip, Track, REQUIRED_CLIPS, REQUIRED_EXPRESSIONS};
pub use contour::{ContourOptions, Polygon};
pub use ease::{Ease, Spring1, Spring2, SpringParams};
pub use frame::{DrawShape, RigFrame};
pub use ik::{solve_two_bone, LookAt, TwoBoneSolution};
pub use math::{Affine, Rect, Vec2};
pub use motion::{squash_from_velocity, ChainParams, Follower, SpringChain, Squash, SquashParams};
pub use paint::{Paint, Rgba, Stroke};
pub use path::{Path, Verb};
pub use physics::{BodyState, Forces, PhysicsEvent, PhysicsParams, Surface};
pub use player::{ClipPlayer, LayerSpec};
pub use rig::{Detail, Rig, RigInput};
pub use skeleton::{Bone, BoneOffsets, Channel, Pose, Skeleton};
pub use skin::{default_skin, Skin, SkinDoc, SkinError, WISP_SKIN_TOML};
