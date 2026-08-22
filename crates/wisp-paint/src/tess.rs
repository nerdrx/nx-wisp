//! CPU tessellation.
//!
//! **Why lyon and not vello.** vello 0.10 — the current release — pins
//! `wgpu = "29.0.3"`, and this workspace pins `wgpu = "30.0"`. Those are
//! semver-incompatible: cargo would link both, and vello's `Renderer` would
//! demand a `wgpu29::Device` that our surface, adapter and queue cannot
//! produce. Downgrading wgpu was not on the table (the M0 spike, the layer
//! surface and the swapchain are all built against 30). So paths are
//! tessellated on the CPU here and drawn by our own pipelines, behind the same
//! `Scene`/`Painter` API a vello backend would present — when vello ships a
//! wgpu-30 release, only this file and `pipelines.rs` change.
//!
//! The happy side effect: there is not one compute pass in the crate, which is
//! precisely what SPEC §3.1's T3 requires of the rig.

use bytemuck::{Pod, Zeroable};
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, StrokeOptions, StrokeTessellator,
    StrokeVertex, VertexBuffers,
};

use crate::geom::{Path, Rect};

/// One vector vertex. `local` is the position inside the shape's bounding box,
/// normalised to 0..1 — the gradient shader's only input.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 2],
    pub local: [f32; 2],
    pub paint: u32,
    pub _pad: u32,
}

/// One textured vertex. `tint` is premultiplied sRGB.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct TexVertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub tint: [f32; 4],
}

/// Curve flattening tolerance in pixels. At the supersampled resolution this
/// is well below a device pixel, which is what keeps a 6px corner from
/// polygonising visibly.
pub const TOLERANCE: f32 = 0.05;

/// Growable output for a frame's vector geometry.
#[derive(Default)]
pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

impl Mesh {
    pub fn clear(&mut self) {
        self.vertices.clear();
        self.indices.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

/// Append a filled path. Returns the index range to draw, or `None` if the
/// path tessellated to nothing (a zero-area rect, a fully clamped radius).
pub fn fill(mesh: &mut Mesh, path: &Path, paint: u32, tolerance: f32) -> Option<std::ops::Range<u32>> {
    let bbox = path.bbox();
    let start = mesh.indices.len() as u32;
    let base = mesh.vertices.len() as u32;

    let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
    let mut tess = FillTessellator::new();
    let opts = FillOptions::tolerance(tolerance).with_fill_rule(lyon::path::FillRule::NonZero);
    let ok = tess
        .tessellate_path(
            &path.inner,
            &opts,
            &mut BuffersBuilder::new(&mut buffers, |v: FillVertex| {
                make_vertex(v.position().x, v.position().y, bbox, paint)
            }),
        )
        .is_ok();
    if !ok || buffers.indices.is_empty() {
        return None;
    }
    mesh.vertices.extend_from_slice(&buffers.vertices);
    mesh.indices.extend(buffers.indices.iter().map(|i| i + base));
    Some(start..mesh.indices.len() as u32)
}

/// Append a stroked path. Used for the 1px lit edges, and for the rig editor's
/// handles.
pub fn stroke(
    mesh: &mut Mesh,
    path: &Path,
    paint: u32,
    width: f32,
    tolerance: f32,
) -> Option<std::ops::Range<u32>> {
    // The gradient runs across the *shape*, not across the ribbon, so the
    // stroke shares the fill's bounding box. That is what makes a lit edge
    // brighten at the top-left corner instead of along its own length.
    let bbox = path.bbox();
    let start = mesh.indices.len() as u32;
    let base = mesh.vertices.len() as u32;

    let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
    let mut tess = StrokeTessellator::new();
    let opts = StrokeOptions::tolerance(tolerance)
        .with_line_width(width)
        .with_line_join(lyon::tessellation::LineJoin::Miter)
        .with_miter_limit(4.0);
    let ok = tess
        .tessellate_path(
            &path.inner,
            &opts,
            &mut BuffersBuilder::new(&mut buffers, |v: StrokeVertex| {
                make_vertex(v.position().x, v.position().y, bbox, paint)
            }),
        )
        .is_ok();
    if !ok || buffers.indices.is_empty() {
        return None;
    }
    mesh.vertices.extend_from_slice(&buffers.vertices);
    mesh.indices.extend(buffers.indices.iter().map(|i| i + base));
    Some(start..mesh.indices.len() as u32)
}

fn make_vertex(x: f32, y: f32, bbox: Rect, paint: u32) -> Vertex {
    let (lx, ly) = bbox.local(x, y);
    Vertex { pos: [x, y], local: [lx, ly], paint, _pad: 0 }
}

/// Growable output for a frame's textured quads.
#[derive(Default)]
pub struct TexMesh {
    pub vertices: Vec<TexVertex>,
    pub indices: Vec<u32>,
}

impl TexMesh {
    pub fn clear(&mut self) {
        self.vertices.clear();
        self.indices.clear();
    }
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Append an axis-aligned quad. `uv` is in 0..1 texture space.
    pub fn quad(&mut self, r: Rect, uv: Rect, tint: [f32; 4]) -> std::ops::Range<u32> {
        let start = self.indices.len() as u32;
        let b = self.vertices.len() as u32;
        let v = |x: f32, y: f32, u: f32, w: f32| TexVertex { pos: [x, y], uv: [u, w], tint };
        self.vertices.push(v(r.x, r.y, uv.x, uv.y));
        self.vertices.push(v(r.right(), r.y, uv.right(), uv.y));
        self.vertices.push(v(r.right(), r.bottom(), uv.right(), uv.bottom()));
        self.vertices.push(v(r.x, r.bottom(), uv.x, uv.bottom()));
        self.indices.extend_from_slice(&[b, b + 1, b + 2, b, b + 2, b + 3]);
        start..self.indices.len() as u32
    }

    /// Append a rounded-rect *mask* whose uv is the screen position — how the
    /// blurred backdrop is painted back through a floating layer's shape.
    pub fn masked_backdrop(
        &mut self,
        r: Rect,
        radius: f32,
        target: Rect,
        tint: [f32; 4],
    ) -> Option<std::ops::Range<u32>> {
        let path = Path::rounded_rect_px(r, radius);
        let start = self.indices.len() as u32;
        let base = self.vertices.len() as u32;
        let mut buffers: VertexBuffers<TexVertex, u32> = VertexBuffers::new();
        let mut tess = FillTessellator::new();
        let ok = tess
            .tessellate_path(
                &path.inner,
                &FillOptions::tolerance(TOLERANCE),
                &mut BuffersBuilder::new(&mut buffers, |v: FillVertex| {
                    let (u, w) = target.local(v.position().x, v.position().y);
                    TexVertex { pos: [v.position().x, v.position().y], uv: [u, w], tint }
                }),
            )
            .is_ok();
        if !ok || buffers.indices.is_empty() {
            return None;
        }
        self.vertices.extend_from_slice(&buffers.vertices);
        self.indices.extend(buffers.indices.iter().map(|i| i + base));
        Some(start..self.indices.len() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_theme::Radius;

    #[test]
    fn a_rounded_rect_tessellates_to_a_closed_mesh() {
        let mut m = Mesh::default();
        let p = Path::rounded_rect(Rect::new(0.0, 0.0, 100.0, 50.0), Radius::CARD);
        let range = fill(&mut m, &p, 0, TOLERANCE).expect("should tessellate");
        assert!(range.end > range.start);
        assert_eq!(m.indices.len() % 3, 0);
        assert!(m.vertices.len() >= 8, "a 6px radius needs arc segments on each corner");
    }

    #[test]
    fn local_coordinates_span_the_bounding_box() {
        let mut m = Mesh::default();
        let p = Path::rect(Rect::new(10.0, 20.0, 40.0, 80.0));
        fill(&mut m, &p, 0, TOLERANCE).unwrap();
        let min_x = m.vertices.iter().map(|v| v.local[0]).fold(f32::MAX, f32::min);
        let max_x = m.vertices.iter().map(|v| v.local[0]).fold(f32::MIN, f32::max);
        let max_y = m.vertices.iter().map(|v| v.local[1]).fold(f32::MIN, f32::max);
        assert!(min_x.abs() < 1e-4);
        assert!((max_x - 1.0).abs() < 1e-4);
        assert!((max_y - 1.0).abs() < 1e-4);
    }

    #[test]
    fn a_zero_area_path_tessellates_to_nothing() {
        let mut m = Mesh::default();
        let p = Path::rect(Rect::new(5.0, 5.0, 0.0, 0.0));
        assert!(fill(&mut m, &p, 0, TOLERANCE).is_none());
        assert!(m.is_empty());
    }

    #[test]
    fn appending_offsets_indices_so_batches_share_one_buffer() {
        let mut m = Mesh::default();
        let p = Path::rect(Rect::new(0.0, 0.0, 10.0, 10.0));
        let a = fill(&mut m, &p, 0, TOLERANCE).unwrap();
        let b = fill(&mut m, &p, 1, TOLERANCE).unwrap();
        assert_eq!(a.end, b.start);
        let max_a = m.indices[a.start as usize..a.end as usize].iter().max().unwrap();
        let min_b = m.indices[b.start as usize..b.end as usize].iter().min().unwrap();
        assert!(min_b > max_a, "the second batch must index its own vertices");
        assert_eq!(m.vertices[m.indices[b.start as usize] as usize].paint, 1);
    }

    #[test]
    fn a_stroke_shares_the_shapes_bounding_box_so_the_edge_stays_lit_corner_first() {
        let mut m = Mesh::default();
        let p = Path::rounded_rect(Rect::new(0.0, 0.0, 100.0, 100.0), Radius::CARD);
        stroke(&mut m, &p, 0, 1.0, TOLERANCE).unwrap();
        // The top-left ribbon vertices must be near local (0,0), not near the
        // middle of a ribbon-local parameterisation.
        let min = m.vertices.iter().map(|v| v.local[0] + v.local[1]).fold(f32::MAX, f32::min);
        assert!(min < 0.05, "the stroke's top-left must sit at the start of the gradient");
    }

    #[test]
    fn a_textured_quad_is_two_triangles() {
        let mut m = TexMesh::default();
        let r = m.quad(Rect::new(0.0, 0.0, 10.0, 10.0), Rect::from_size(1.0, 1.0), [1.0; 4]);
        assert_eq!(r, 0..6);
        assert_eq!(m.vertices.len(), 4);
        assert_eq!(m.vertices[2].uv, [1.0, 1.0]);
    }

    #[test]
    fn a_masked_backdrop_uses_screen_space_uvs() {
        let mut m = TexMesh::default();
        let target = Rect::from_size(200.0, 100.0);
        m.masked_backdrop(Rect::new(100.0, 50.0, 100.0, 50.0), 6.0, target, [1.0; 4]).unwrap();
        // Everything is in the lower-right quadrant of the target.
        assert!(m.vertices.iter().all(|v| v.uv[0] >= 0.5 - 1e-4 && v.uv[1] >= 0.5 - 1e-4));
    }
}
