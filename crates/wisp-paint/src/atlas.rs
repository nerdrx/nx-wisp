//! The sprite-atlas baker — SPEC §3.1's "Lobotomised" path.
//!
//! At T3 a game owns the GPU and she must cost it nothing. So the poses she
//! still needs are rendered **once**, through the ordinary vector path, into
//! one atlas texture; from then on a frame is a handful of textured quads with
//! no tessellation, no gradient evaluation, no blur, and no compute pass.
//!
//! Baking deliberately forces the vector path regardless of the current tier —
//! otherwise baking *at* T3 would produce an atlas of nothing. The tier is
//! restored afterwards.

use std::collections::HashMap;

use crate::error::{PaintError, Result};
use crate::geom::Rect;
use crate::painter::Painter;
use crate::scene::Scene;
use crate::texture::Texture;
use wisp_proto::{Governed, Tier, TierReason};

/// Where one baked scene lives in the atlas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sprite {
    /// Normalised texture coordinates.
    pub uv: Rect,
    /// Pixel rect in the atlas, and therefore the natural draw size.
    pub px: Rect,
}

impl Sprite {
    pub fn size(&self) -> (f32, f32) {
        (self.px.w, self.px.h)
    }
}

/// A baked set of scenes.
pub struct Atlas {
    texture: Texture,
    entries: HashMap<String, Sprite>,
    w: u32,
    h: u32,
    used_px: u32,
}

impl Atlas {
    pub fn texture(&self) -> &Texture {
        &self.texture
    }
    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }
    pub fn get(&self, name: &str) -> Option<Sprite> {
        self.entries.get(name).copied()
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(|s| s.as_str())
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// Fraction of the atlas occupied, for the cost meter.
    pub fn occupancy(&self) -> f32 {
        self.used_px as f32 / (self.w * self.h).max(1) as f32
    }

    /// Draw a baked sprite at its natural size.
    pub fn draw(&self, scene: &mut Scene, name: &str, x: f32, y: f32) -> Result<()> {
        self.draw_tinted(scene, name, x, y, wisp_theme::Color::hex(0xffffff))
    }

    /// Draw a baked sprite, modulated. A white opaque tint is the identity, so
    /// the fade-out at T3 is a tint, not a re-bake.
    pub fn draw_tinted(
        &self,
        scene: &mut Scene,
        name: &str,
        x: f32,
        y: f32,
        tint: wisp_theme::Color,
    ) -> Result<()> {
        let s = self.get(name).ok_or_else(|| PaintError::NoSuchSprite(name.to_string()))?;
        scene.sprite(Rect::new(x, y, s.px.w, s.px.h), s.uv, self.texture.clone(), tint);
        Ok(())
    }
}

/// One scene to bake, at a fixed size.
pub struct BakeItem {
    pub name: String,
    pub scene: Scene,
    pub w: u32,
    pub h: u32,
}

impl BakeItem {
    pub fn new(name: impl Into<String>, w: u32, h: u32, scene: Scene) -> BakeItem {
        BakeItem { name: name.into(), scene, w, h }
    }
}

/// 1px of dead space around each slot, so bilinear filtering at the edge of a
/// quad cannot pull in the neighbour.
pub const GUTTER: u32 = 1;

/// A shelf packer. Deterministic — the same input always produces the same
/// layout, which matters because the atlas is compared against the vector path
/// pixel for pixel in the test suite.
pub fn pack(items: &[(u32, u32)], atlas_w: u32, atlas_h: u32) -> Option<Vec<(u32, u32)>> {
    // Tallest first: the classic shelf heuristic, and stable because ties fall
    // back to the original index.
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| (std::cmp::Reverse(items[i].1), i));

    let mut out = vec![(0u32, 0u32); items.len()];
    let (mut x, mut y, mut shelf_h) = (0u32, 0u32, 0u32);
    for &i in &order {
        let (w, h) = (items[i].0 + GUTTER, items[i].1 + GUTTER);
        if w > atlas_w || h > atlas_h {
            return None;
        }
        if x + w > atlas_w {
            x = 0;
            y += shelf_h;
            shelf_h = 0;
        }
        if y + h > atlas_h {
            return None;
        }
        out[i] = (x, y);
        x += w;
        shelf_h = shelf_h.max(h);
    }
    Some(out)
}

/// Bake every scene into one texture.
pub fn bake(painter: &mut Painter, atlas_w: u32, atlas_h: u32, items: Vec<BakeItem>) -> Result<Atlas> {
    if atlas_w == 0 || atlas_h == 0 {
        return Err(PaintError::EmptyTarget { w: atlas_w, h: atlas_h });
    }
    let sizes: Vec<(u32, u32)> = items.iter().map(|i| (i.w.max(1), i.h.max(1))).collect();
    let needed: u32 = sizes.iter().map(|(w, h)| (w + GUTTER) * (h + GUTTER)).sum();
    let slots = pack(&sizes, atlas_w, atlas_h)
        .ok_or(PaintError::AtlasFull { needed, free: atlas_w * atlas_h })?;

    let atlas_tex = painter.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("wisp.atlas"),
        size: wgpu::Extent3d { width: atlas_w, height: atlas_h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: crate::pipelines::FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });

    // Baking always uses the vector path — see the module docs.
    let restore = painter.tier();
    painter.set_tier(Tier::Full, &TierReason::Pinned);

    let mut entries = HashMap::new();
    for (item, (sx, sy)) in items.iter().zip(&slots) {
        let (w, h) = (item.w.max(1), item.h.max(1));
        let slot = painter.offscreen(w, h)?;
        painter.render(&slot, &item.scene)?;

        let mut enc = painter
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("wisp.atlas.copy") });
        enc.copy_texture_to_texture(
            slot.texture().as_image_copy(),
            wgpu::TexelCopyTextureInfo {
                texture: &atlas_tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x: *sx, y: *sy, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        painter.queue().submit(Some(enc.finish()));

        entries.insert(
            item.name.clone(),
            Sprite {
                uv: Rect::new(
                    *sx as f32 / atlas_w as f32,
                    *sy as f32 / atlas_h as f32,
                    w as f32 / atlas_w as f32,
                    h as f32 / atlas_h as f32,
                ),
                px: Rect::new(*sx as f32, *sy as f32, w as f32, h as f32),
            },
        );
    }

    painter.set_tier(restore, &TierReason::Pinned);
    let texture = painter.wrap_texture(atlas_tex, atlas_w, atlas_h, false);
    Ok(Atlas { texture, entries, w: atlas_w, h: atlas_h, used_px: needed })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_places_everything_inside_the_atlas() {
        let items = vec![(64, 64), (32, 100), (128, 20), (10, 10)];
        let slots = pack(&items, 256, 256).unwrap();
        for (i, (x, y)) in slots.iter().enumerate() {
            assert!(x + items[i].0 <= 256, "item {i} runs off the right");
            assert!(y + items[i].1 <= 256, "item {i} runs off the bottom");
        }
    }

    #[test]
    fn packed_slots_do_not_overlap() {
        let items: Vec<(u32, u32)> = (0..20).map(|i| (16 + i * 3, 12 + (i % 5) * 7)).collect();
        let slots = pack(&items, 512, 512).unwrap();
        for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                let a = Rect::new(slots[i].0 as f32, slots[i].1 as f32, items[i].0 as f32, items[i].1 as f32);
                let b = Rect::new(slots[j].0 as f32, slots[j].1 as f32, items[j].0 as f32, items[j].1 as f32);
                assert!(a.intersect(b).is_empty(), "{i} overlaps {j}: {a:?} {b:?}");
            }
        }
    }

    #[test]
    fn packing_is_deterministic() {
        let items = vec![(30, 40), (30, 40), (10, 90), (200, 5)];
        assert_eq!(pack(&items, 256, 256), pack(&items, 256, 256));
    }

    #[test]
    fn an_oversized_item_fails_rather_than_being_squeezed() {
        assert!(pack(&[(1000, 10)], 256, 256).is_none());
        assert!(pack(&[(10, 1000)], 256, 256).is_none());
    }

    #[test]
    fn a_full_atlas_fails_cleanly() {
        let items: Vec<(u32, u32)> = (0..100).map(|_| (60, 60)).collect();
        assert!(pack(&items, 128, 128).is_none());
    }

    #[test]
    fn sprites_leave_a_gutter_so_filtering_cannot_bleed() {
        let items = vec![(10, 10), (10, 10)];
        let slots = pack(&items, 64, 64).unwrap();
        assert!(slots[1].0 >= slots[0].0 + 10 + GUTTER || slots[1].1 >= slots[0].1 + 10 + GUTTER);
    }
}
