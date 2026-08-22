//! Everything that can go wrong between a [`crate::Scene`] and a pixel.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PaintError {
    #[error("no Vulkan adapter matched the preference {0} — this build is Vulkan-only by SPEC §1")]
    NoAdapter(String),
    #[error("requesting a wgpu device failed: {0}")]
    Device(#[from] wgpu::RequestDeviceError),
    #[error("mapping a readback buffer failed: {0}")]
    Readback(String),
    #[error("the render target is {w}x{h}; both dimensions must be non-zero")]
    EmptyTarget { w: u32, h: u32 },
    #[error("the sprite atlas is full: {needed} does not fit in the remaining {free} px²")]
    AtlasFull { needed: u32, free: u32 },
    #[error("no scene named {0:?} was baked into this atlas")]
    NoSuchSprite(String),
    #[error("font system error: {0}")]
    Text(String),
}

pub type Result<T> = std::result::Result<T, PaintError>;
