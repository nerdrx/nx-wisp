//! Every wgpu pipeline the crate owns. There are five, and none of them is a
//! compute pipeline.

use bytemuck::{Pod, Zeroable};

use crate::tess::{TexVertex, Vertex};

/// Non-`Srgb` on purpose — see `wisp_theme::color`. The shader writes 8-bit
/// codes and blending happens in sRGB, matching the CSS the tokens came from,
/// and matching the M0 spike's swapchain format.
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Glyph coverage.
pub const COVERAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Globals {
    /// (logical width, logical height, supersample factor, unused)
    pub viewport: [f32; 4],
    /// (dir.x, dir.y, radius px, saturate)
    pub blur: [f32; 4],
}

impl Globals {
    pub fn new(w: f32, h: f32, ss: f32) -> Globals {
        Globals { viewport: [w, h, ss, 0.0], blur: [0.0, 0.0, 0.0, 1.0] }
    }
}

pub struct Layouts {
    pub globals: wgpu::BindGroupLayout,
    pub paints: wgpu::BindGroupLayout,
    pub texture: wgpu::BindGroupLayout,
}

pub struct Pipelines {
    pub layouts: Layouts,
    pub vector: wgpu::RenderPipeline,
    pub tex_rgba: wgpu::RenderPipeline,
    pub tex_alpha: wgpu::RenderPipeline,
    pub blur: wgpu::RenderPipeline,
    pub resolve: wgpu::RenderPipeline,
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

impl Pipelines {
    pub fn new(device: &wgpu::Device) -> Pipelines {
        let globals = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wisp.globals"),
            entries: &[uniform_entry(0)],
        });
        let paints = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wisp.paints"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let texture = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wisp.texture"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let vector_sh = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("wisp.vector"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/vector.wgsl").into()),
        });
        let tex_sh = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("wisp.textured"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/textured.wgsl").into()),
        });
        let post_sh = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("wisp.post"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/post.wgsl").into()),
        });

        let vector_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wisp.vector.layout"),
            bind_group_layouts: &[Some(&globals), Some(&paints)],
            ..Default::default()
        });
        let tex_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wisp.textured.layout"),
            bind_group_layouts: &[Some(&globals), Some(&texture)],
            ..Default::default()
        });

        let vector_attrs = [
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 },
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 8, shader_location: 1 },
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Uint32, offset: 16, shader_location: 2 },
        ];
        let tex_attrs = [
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 },
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 8, shader_location: 1 },
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16, shader_location: 2 },
        ];

        let blend_premul = Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);

        let vector = make_pipeline(
            device,
            "wisp.vector",
            &vector_layout,
            &vector_sh,
            "vs",
            &vector_sh,
            "fs",
            &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Vertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &vector_attrs,
            })],
            blend_premul,
        );
        let tex_vb = [Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TexVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &tex_attrs,
        })];
        let tex_rgba = make_pipeline(
            device, "wisp.tex.rgba", &tex_layout, &tex_sh, "vs", &tex_sh, "fs_rgba", &tex_vb,
            blend_premul,
        );
        let tex_alpha = make_pipeline(
            device, "wisp.tex.alpha", &tex_layout, &tex_sh, "vs", &tex_sh, "fs_alpha", &tex_vb,
            blend_premul,
        );
        let blur = make_pipeline(
            device, "wisp.blur", &tex_layout, &post_sh, "vs", &post_sh, "fs_blur", &[],
            Some(wgpu::BlendState::REPLACE),
        );
        let resolve = make_pipeline(
            device, "wisp.resolve", &tex_layout, &post_sh, "vs", &post_sh, "fs_resolve", &[],
            Some(wgpu::BlendState::REPLACE),
        );

        Pipelines {
            layouts: Layouts { globals, paints, texture },
            vector,
            tex_rgba,
            tex_alpha,
            blur,
            resolve,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn make_pipeline(
    device: &wgpu::Device,
    label: &str,
    layout: &wgpu::PipelineLayout,
    vs_module: &wgpu::ShaderModule,
    vs: &str,
    fs_module: &wgpu::ShaderModule,
    fs: &str,
    buffers: &[Option<wgpu::VertexBufferLayout>],
    blend: Option<wgpu::BlendState>,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: vs_module,
            entry_point: Some(vs),
            buffers,
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: fs_module,
            entry_point: Some(fs),
            targets: &[Some(wgpu::ColorTargetState {
                format: FORMAT,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            // Tessellated 2D output has no meaningful winding: lyon emits both
            // orientations depending on the path direction.
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_globals_block_is_the_size_the_shader_declares() {
        assert_eq!(std::mem::size_of::<Globals>(), 32);
    }

    #[test]
    fn the_target_format_is_not_srgb() {
        // If this ever becomes Rgba8UnormSrgb, every token in DESIGN.md shifts
        // and the readback tests stop matching the CSS they were copied from.
        assert_eq!(FORMAT, wgpu::TextureFormat::Rgba8Unorm);
    }

    #[test]
    fn the_vertex_strides_match_the_attribute_offsets() {
        assert_eq!(std::mem::size_of::<Vertex>(), 24);
        assert_eq!(std::mem::size_of::<TexVertex>(), 32);
    }
}
