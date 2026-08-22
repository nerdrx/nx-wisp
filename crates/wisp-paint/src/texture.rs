//! GPU textures, refcounted so a [`crate::Scene`] can hold one without caring
//! who owns it. Glyph rasters and the sprite atlas are both these.

use std::sync::Arc;

pub(crate) struct TextureInner {
    /// Kept alive for the bind group; nothing reads it back through here.
    #[allow(dead_code)]
    pub texture: wgpu::Texture,
    #[allow(dead_code)]
    pub view: wgpu::TextureView,
    pub bind: wgpu::BindGroup,
    pub w: u32,
    pub h: u32,
    pub alpha_only: bool,
}

/// A texture plus its ready-made bind group.
#[derive(Clone)]
pub struct Texture(pub(crate) Arc<TextureInner>);

impl Texture {
    pub fn size(&self) -> (u32, u32) {
        (self.0.w, self.0.h)
    }
    pub fn width(&self) -> u32 {
        self.0.w
    }
    pub fn height(&self) -> u32 {
        self.0.h
    }
    /// Single-channel coverage (a glyph raster) rather than premultiplied RGBA.
    pub fn is_alpha_only(&self) -> bool {
        self.0.alpha_only
    }
    pub(crate) fn bind(&self) -> &wgpu::BindGroup {
        &self.0.bind
    }
    /// Identity, for batching consecutive draws that share a texture.
    pub fn id(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }
}

impl PartialEq for Texture {
    fn eq(&self, other: &Texture) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for Texture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Texture")
            .field("w", &self.0.w)
            .field("h", &self.0.h)
            .field("alpha_only", &self.0.alpha_only)
            .finish()
    }
}
