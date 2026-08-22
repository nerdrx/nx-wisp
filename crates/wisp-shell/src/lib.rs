//! The compositor half: a `zwlr_layer_shell_v1` surface she lives on, a wgpu
//! swapchain with premultiplied alpha, and a per-frame input region so clicks
//! pass through everywhere she is not.
//!
//! The risky parts of this were proven before any of it was written — see
//! `spike/src/main.rs`, which established on KWin 6.7 that a layer surface
//! accepts a Vulkan swapchain with `PreMultiplied` alpha and an input region
//! smaller than the surface.

pub mod bridge;
pub mod layer;

pub use layer::{ShellConfig, WispShell};

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("no wayland display — she needs a Wayland session")]
    NoDisplay,
    #[error("this compositor does not offer zwlr_layer_shell_v1; KDE Plasma 6 does")]
    NoLayerShell,
    #[error("no Vulkan adapter could draw to the layer surface")]
    NoAdapter,
    #[error("wgpu: {0}")]
    Wgpu(String),
}
