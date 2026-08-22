//! The renderer. Owns the wgpu device and turns a [`Scene`] into pixels, on a
//! swapchain surface or into an offscreen texture.
//!
//! Backend is **forced to Vulkan** (SPEC §1) and the adapter is a parameter
//! (see [`crate::adapter`]) because the governor will move rendering between
//! the dGPU and the iGPU as tiers change.
//!
//! Antialiasing is supersampling, not MSAA: the scene renders into a scratch
//! texture at `ssaa`× and resolves with one bilinear tap. That keeps every
//! intermediate single-sampled, which is what makes the backdrop blur a plain
//! texture copy instead of a resolve dance — and it gives the governor a free
//! quality dial, since dropping to 1× at T2 cuts fragment cost by four.

use std::collections::HashMap;
use std::sync::Arc;

use wisp_proto::{Cost, Governed, Tier, TierReason};
use wisp_theme::{Blur, Color};

use crate::adapter::{self, AdapterPreference, AdapterSummary};
use crate::error::{PaintError, Result};
use crate::geom::Rect;
use crate::paint::PaintGpu;
use crate::pipelines::{Globals, Pipelines, COVERAGE_FORMAT, FORMAT};
use crate::scene::{Cmd, Scene};
use crate::tess::{self, Mesh, TexMesh};
use crate::texture::{Texture, TextureInner};

/// What the painter is allowed to draw at the current tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawMode {
    /// Full vector: paths, gradients, real backdrop blur.
    Vector,
    /// Sprite-atlas quads only. No tessellation, no blur, no compute — SPEC
    /// §3.1's "Lobotomised".
    SpriteOnly,
    /// Nothing at all. The surface still gets a transparent frame so she
    /// vanishes rather than freezing on screen.
    Nothing,
}

/// A CPU copy of a rendered frame. RGBA8, **premultiplied**.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

impl Image {
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.w + x) * 4) as usize;
        [self.data[i], self.data[i + 1], self.data[i + 2], self.data[i + 3]]
    }
    /// Premultiplied RGBA back to a straight-alpha theme colour.
    pub fn color(&self, x: u32, y: u32) -> Color {
        let [r, g, b, a] = self.pixel(x, y);
        if a == 0 {
            return Color::TRANSPARENT;
        }
        let un = |c: u8| ((c as f32 / a as f32) * 255.0).round().clamp(0.0, 255.0) as u8;
        Color::rgba(un(r), un(g), un(b), a)
    }
    pub fn alpha(&self, x: u32, y: u32) -> u8 {
        self.pixel(x, y)[3]
    }
    /// Mean absolute per-channel difference against another image of the same
    /// size, 0..=255. Used to compare the sprite path to the vector path.
    pub fn mean_abs_diff(&self, other: &Image) -> f32 {
        assert_eq!((self.w, self.h), (other.w, other.h));
        let sum: u64 = self
            .data
            .iter()
            .zip(&other.data)
            .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64)
            .sum();
        sum as f32 / self.data.len() as f32
    }
    /// Worst single-channel difference.
    pub fn max_abs_diff(&self, other: &Image) -> u8 {
        assert_eq!((self.w, self.h), (other.w, other.h));
        self.data
            .iter()
            .zip(&other.data)
            .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u8)
            .max()
            .unwrap_or(0)
    }
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Image({}x{})", self.w, self.h)
    }
}

/// An offscreen render target with readback. Every GPU test in the tree draws
/// into one of these — SPEC §4 forbids opening a window.
pub struct Offscreen {
    pub(crate) texture: wgpu::Texture,
    pub(crate) view: wgpu::TextureView,
    pub w: u32,
    pub h: u32,
}

impl Offscreen {
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }
}

struct Scratch {
    w: u32,
    h: u32,
    main: wgpu::Texture,
    main_view: wgpu::TextureView,
    main_bind: wgpu::BindGroup,
    a: wgpu::Texture,
    a_view: wgpu::TextureView,
    a_bind: wgpu::BindGroup,
    b_view: wgpu::TextureView,
    b_bind: wgpu::BindGroup,
}

struct Growable {
    buf: Option<wgpu::Buffer>,
    cap: u64,
    /// Bumped whenever the underlying buffer is replaced, so a cached bind
    /// group knows it has gone stale.
    generation: u64,
    usage: wgpu::BufferUsages,
    label: &'static str,
}

impl Growable {
    fn new(label: &'static str, usage: wgpu::BufferUsages) -> Growable {
        Growable { buf: None, cap: 0, generation: 0, usage, label }
    }
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bytes: &[u8],
    ) -> Option<(wgpu::Buffer, u64)> {
        if bytes.is_empty() {
            return None;
        }
        // Round up so a resizing scene does not reallocate every frame.
        let need = (bytes.len() as u64).next_power_of_two().max(1024);
        if self.cap < need {
            self.buf = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: need,
                usage: self.usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.cap = need;
            self.generation += 1;
        }
        let buf = self.buf.as_ref().unwrap();
        queue.write_buffer(buf, 0, bytes);
        Some((buf.clone(), self.generation))
    }
}

#[derive(Debug)]
enum Batch {
    Vector { range: std::ops::Range<u32>, clip: Option<Rect> },
    Textured { alpha: bool, tex: Option<Texture>, range: std::ops::Range<u32>, clip: Option<Rect> },
    Blur { blur: Blur },
}

pub struct Painter {
    #[allow(dead_code)]
    instance: wgpu::Instance,
    #[allow(dead_code)]
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    info: AdapterSummary,
    pipelines: Pipelines,
    sampler: wgpu::Sampler,

    tier: Tier,
    base_ssaa: u32,

    scratch: Option<Scratch>,
    globals_buf: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    blur_pool: Vec<(wgpu::Buffer, wgpu::BindGroup)>,
    empty_paints: wgpu::Buffer,

    verts: Growable,
    idx: Growable,
    tex_verts: Growable,
    tex_idx: Growable,
    paints_buf: Growable,
    paint_bind_cache: HashMap<u64, wgpu::BindGroup>,

    mesh: Mesh,
    tex_mesh: TexMesh,

    last_blur_count: usize,
    last_draw_calls: usize,
}

impl Painter {
    /// Bring up Vulkan, pick an adapter and build every pipeline.
    pub fn new(pref: AdapterPreference) -> Result<Painter> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
        let summaries: Vec<AdapterSummary> =
            adapters.iter().map(|a| AdapterSummary::from(&a.get_info())).collect();
        let chosen = adapter::select(&summaries, &pref)?;
        let adapter = adapters.into_iter().nth(chosen).expect("index came from the same list");
        let info = summaries[chosen].clone();

        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("nx-wisp"),
                ..Default::default()
            }))?;
        Ok(Painter::assemble(instance, adapter, device, queue, info))
    }

    /// Adopt a device someone else made — `wisp-shell` creates the surface and
    /// therefore the device, and hands it here rather than opening a second
    /// one.
    pub fn from_device(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
    ) -> Painter {
        let info = AdapterSummary::from(&adapter.get_info());
        Painter::assemble(instance, adapter, device, queue, info)
    }

    fn assemble(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        info: AdapterSummary,
    ) -> Painter {
        let pipelines = Pipelines::new(&device);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("wisp.linear"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wisp.globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wisp.globals"),
            layout: &pipelines.layouts.globals,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() }],
        });
        let empty_paints = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wisp.paints.empty"),
            size: std::mem::size_of::<PaintGpu>() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Painter {
            instance,
            adapter,
            device,
            queue,
            info,
            pipelines,
            sampler,
            tier: Tier::Full,
            base_ssaa: 2,
            scratch: None,
            globals_buf,
            globals_bind,
            blur_pool: Vec::new(),
            empty_paints,
            verts: Growable::new("wisp.verts", wgpu::BufferUsages::VERTEX),
            idx: Growable::new("wisp.idx", wgpu::BufferUsages::INDEX),
            tex_verts: Growable::new("wisp.tex.verts", wgpu::BufferUsages::VERTEX),
            tex_idx: Growable::new("wisp.tex.idx", wgpu::BufferUsages::INDEX),
            paints_buf: Growable::new("wisp.paints", wgpu::BufferUsages::STORAGE),
            paint_bind_cache: HashMap::new(),
            mesh: Mesh::default(),
            tex_mesh: TexMesh::default(),
            last_blur_count: 0,
            last_draw_calls: 0,
        }
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }
    pub fn adapter_info(&self) -> &AdapterSummary {
        &self.info
    }
    pub fn tier(&self) -> Tier {
        self.tier
    }
    /// Real blurs the last frame spent. DESIGN.md §4 budgets ~10.
    pub fn last_blur_count(&self) -> usize {
        self.last_blur_count
    }
    pub fn last_draw_calls(&self) -> usize {
        self.last_draw_calls
    }

    /// The supersample factor actually in force. The governor owns this.
    pub fn ssaa(&self) -> u32 {
        match self.tier {
            Tier::Feral | Tier::Full => self.base_ssaa,
            _ => 1,
        }
    }

    /// What she is allowed to draw right now.
    pub fn draw_mode(&self) -> DrawMode {
        match self.tier {
            Tier::Feral | Tier::Full | Tier::Reduced => DrawMode::Vector,
            Tier::Lobotomised => DrawMode::SpriteOnly,
            Tier::Dormant => DrawMode::Nothing,
        }
    }

    /// Minimum interval between frames, from `Tier::target_fps`. `None` at T4.
    pub fn frame_interval(&self) -> Option<std::time::Duration> {
        let fps = self.tier.target_fps();
        (fps > 0).then(|| std::time::Duration::from_secs_f64(1.0 / fps as f64))
    }

    // ------------------------------------------------------------- resources

    pub fn offscreen(&self, w: u32, h: u32) -> Result<Offscreen> {
        if w == 0 || h == 0 {
            return Err(PaintError::EmptyTarget { w, h });
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("wisp.offscreen"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Ok(Offscreen { texture, view, w, h })
    }

    /// Upload premultiplied RGBA8. The atlas and any decoded image.
    pub fn upload_rgba(&self, w: u32, h: u32, data: &[u8]) -> Texture {
        self.upload(w, h, data, FORMAT, 4, false)
    }

    /// Upload single-channel coverage. Glyph rasters.
    pub fn upload_coverage(&self, w: u32, h: u32, data: &[u8]) -> Texture {
        self.upload(w, h, data, COVERAGE_FORMAT, 1, true)
    }

    fn upload(
        &self,
        w: u32,
        h: u32,
        data: &[u8],
        format: wgpu::TextureFormat,
        bpp: u32,
        alpha_only: bool,
    ) -> Texture {
        let (w, h) = (w.max(1), h.max(1));
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("wisp.upload"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        if !data.is_empty() {
            self.queue.write_texture(
                texture.as_image_copy(),
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * bpp),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }
        self.wrap_texture(texture, w, h, alpha_only)
    }

    pub(crate) fn wrap_texture(
        &self,
        texture: wgpu::Texture,
        w: u32,
        h: u32,
        alpha_only: bool,
    ) -> Texture {
        let view = texture.create_view(&Default::default());
        let bind = self.texture_bind(&view);
        Texture(Arc::new(TextureInner { texture, view, bind, w, h, alpha_only }))
    }

    fn texture_bind(&self, view: &wgpu::TextureView) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wisp.texture"),
            layout: &self.pipelines.layouts.texture,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        })
    }

    fn scratch_texture(&self, label: &str, w: u32, h: u32) -> (wgpu::Texture, wgpu::TextureView, wgpu::BindGroup) {
        let t = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let v = t.create_view(&Default::default());
        let b = self.texture_bind(&v);
        (t, v, b)
    }

    fn ensure_scratch(&mut self, w: u32, h: u32) {
        if self.scratch.as_ref().is_some_and(|s| s.w == w && s.h == h) {
            return;
        }
        let (main, main_view, main_bind) = self.scratch_texture("wisp.scratch.main", w, h);
        let (a, a_view, a_bind) = self.scratch_texture("wisp.scratch.a", w, h);
        let (_b, b_view, b_bind) = self.scratch_texture("wisp.scratch.b", w, h);
        self.scratch = Some(Scratch { w, h, main, main_view, main_bind, a, a_view, a_bind, b_view, b_bind });
    }

    fn ensure_blur_pool(&mut self, n: usize) {
        while self.blur_pool.len() < n {
            let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("wisp.globals.blur"),
                size: std::mem::size_of::<Globals>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("wisp.globals.blur"),
                layout: &self.pipelines.layouts.globals,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() }],
            });
            self.blur_pool.push((buf, bind));
        }
    }

    // --------------------------------------------------------------- drawing

    /// Render into an offscreen target.
    pub fn render(&mut self, target: &Offscreen, scene: &Scene) -> Result<()> {
        let (w, h) = (target.w, target.h);
        self.render_to_view(&target.view, w, h, scene)
    }

    /// Render into any view — a swapchain texture from `wisp-shell`, or an
    /// atlas slot from the baker.
    pub fn render_to_view(
        &mut self,
        view: &wgpu::TextureView,
        w: u32,
        h: u32,
        scene: &Scene,
    ) -> Result<()> {
        if w == 0 || h == 0 {
            return Err(PaintError::EmptyTarget { w, h });
        }
        let mode = self.draw_mode();
        let ss = self.ssaa();
        self.ensure_scratch(w * ss, h * ss);

        let (batches, paints, blur_params) = self.build(scene, w, h, ss, mode);
        self.last_blur_count = blur_params.len() / 2;
        self.ensure_blur_pool(blur_params.len());

        // Every uniform write lands before any command in the encoder, which is
        // exactly why each blur pass gets its own buffer instead of one being
        // rewritten between passes.
        let g = Globals::new(w as f32, h as f32, ss as f32);
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&g));
        for (i, p) in blur_params.iter().enumerate() {
            let mut gb = g;
            gb.blur = *p;
            self.queue.write_buffer(&self.blur_pool[i].0, 0, bytemuck::bytes_of(&gb));
        }

        let vb = self.verts.upload(&self.device, &self.queue, bytemuck::cast_slice(&self.mesh.vertices)).map(|(b, _)| b);
        let ib = self.idx.upload(&self.device, &self.queue, bytemuck::cast_slice(&self.mesh.indices)).map(|(b, _)| b);
        let tvb = self.tex_verts.upload(&self.device, &self.queue, bytemuck::cast_slice(&self.tex_mesh.vertices)).map(|(b, _)| b);
        let tib = self.tex_idx.upload(&self.device, &self.queue, bytemuck::cast_slice(&self.tex_mesh.indices)).map(|(b, _)| b);

        let paint_bind = if paints.is_empty() {
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("wisp.paints.empty"),
                layout: &self.pipelines.layouts.paints,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: self.empty_paints.as_entire_binding() }],
            })
        } else {
            let bytes: &[u8] = bytemuck::cast_slice(&paints);
            let (buf, generation) =
                self.paints_buf.upload(&self.device, &self.queue, bytes).expect("non-empty");
            self.paint_bind_cache.entry(generation).or_insert_with(|| {
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("wisp.paints"),
                    layout: &self.pipelines.layouts.paints,
                    entries: &[wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() }],
                })
            });
            // Cloning a bind group is a refcount bump.
            self.paint_bind_cache.get(&generation).expect("just inserted").clone()
        };

        let scratch = self.scratch.as_ref().expect("ensured above");
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("wisp.frame"),
        });
        let mut draw_calls = 0usize;
        let mut blur_i = 0usize;
        let mut first_pass = true;

        let mut i = 0;
        while i < batches.len() {
            if let Batch::Blur { blur } = &batches[i] {
                // 1. snapshot everything drawn so far
                enc.copy_texture_to_texture(
                    scratch.main.as_image_copy(),
                    scratch.a.as_image_copy(),
                    wgpu::Extent3d { width: scratch.w, height: scratch.h, depth_or_array_layers: 1 },
                );
                let _ = blur;
                // 2. horizontal a -> b, 3. vertical b -> a (with the saturate)
                for (src_bind, dst_view) in
                    [(&scratch.a_bind, &scratch.b_view), (&scratch.b_bind, &scratch.a_view)]
                {
                    let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("wisp.blur"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: dst_view,
                            resolve_target: None,
                            depth_slice: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        ..Default::default()
                    });
                    rp.set_pipeline(&self.pipelines.blur);
                    rp.set_bind_group(0, &self.blur_pool[blur_i].1, &[]);
                    rp.set_bind_group(1, src_bind, &[]);
                    rp.draw(0..3, 0..1);
                    blur_i += 1;
                    draw_calls += 1;
                }
                i += 1;
                continue;
            }

            // Collect the run of drawing batches up to the next blur.
            let start = i;
            while i < batches.len() && !matches!(batches[i], Batch::Blur { .. }) {
                i += 1;
            }
            let load = if first_pass {
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
            } else {
                wgpu::LoadOp::Load
            };
            first_pass = false;
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("wisp.main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &scratch.main_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            let mut clip_set: Option<Rect> = None;
            for b in &batches[start..i] {
                let (clip, _) = match b {
                    Batch::Vector { clip, .. } => (*clip, ()),
                    Batch::Textured { clip, .. } => (*clip, ()),
                    Batch::Blur { .. } => unreachable!(),
                };
                if clip != clip_set {
                    match clip {
                        Some(r) => {
                            let (x, y, cw, ch) = scissor(r, ss, scratch.w, scratch.h);
                            if cw == 0 || ch == 0 {
                                // Nothing can be visible; skip the batch below
                                // by setting a 1px scissor outside the draw.
                                rp.set_scissor_rect(0, 0, 1, 1);
                            } else {
                                rp.set_scissor_rect(x, y, cw, ch);
                            }
                        }
                        None => rp.set_scissor_rect(0, 0, scratch.w, scratch.h),
                    }
                    clip_set = clip;
                }
                match b {
                    Batch::Vector { range, .. } => {
                        let (Some(vb), Some(ib)) = (&vb, &ib) else { continue };
                        rp.set_pipeline(&self.pipelines.vector);
                        rp.set_bind_group(0, &self.globals_bind, &[]);
                        rp.set_bind_group(1, &paint_bind, &[]);
                        rp.set_vertex_buffer(0, vb.slice(..));
                        rp.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                        rp.draw_indexed(range.clone(), 0, 0..1);
                        draw_calls += 1;
                    }
                    Batch::Textured { alpha, tex, range, .. } => {
                        let (Some(tvb), Some(tib)) = (&tvb, &tib) else { continue };
                        rp.set_pipeline(if *alpha {
                            &self.pipelines.tex_alpha
                        } else {
                            &self.pipelines.tex_rgba
                        });
                        rp.set_bind_group(0, &self.globals_bind, &[]);
                        match tex {
                            Some(t) => rp.set_bind_group(1, t.bind(), &[]),
                            None => rp.set_bind_group(1, &scratch.a_bind, &[]),
                        }
                        rp.set_vertex_buffer(0, tvb.slice(..));
                        rp.set_index_buffer(tib.slice(..), wgpu::IndexFormat::Uint32);
                        rp.draw_indexed(range.clone(), 0, 0..1);
                        draw_calls += 1;
                    }
                    Batch::Blur { .. } => unreachable!(),
                }
            }
        }

        if first_pass {
            // Nothing drew at all — still clear, so a T4 frame is transparent
            // rather than stale.
            enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("wisp.main.clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &scratch.main_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }

        // Resolve into the real target.
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("wisp.resolve"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(&self.pipelines.resolve);
            rp.set_bind_group(0, &self.globals_bind, &[]);
            rp.set_bind_group(1, &scratch.main_bind, &[]);
            rp.draw(0..3, 0..1);
            draw_calls += 1;
        }

        self.queue.submit(Some(enc.finish()));
        self.last_draw_calls = draw_calls;
        Ok(())
    }

    /// Walk the scene once, tessellating into `self.mesh`/`self.tex_mesh` and
    /// producing the batch list, the paint records, and the blur parameters.
    fn build(
        &mut self,
        scene: &Scene,
        w: u32,
        h: u32,
        ss: u32,
        mode: DrawMode,
    ) -> (Vec<Batch>, Vec<PaintGpu>, Vec<[f32; 4]>) {
        self.mesh.clear();
        self.tex_mesh.clear();
        let mut batches: Vec<Batch> = Vec::new();
        let mut paints: Vec<PaintGpu> = Vec::new();
        let mut blur_params: Vec<[f32; 4]> = Vec::new();
        let mut clip: Option<Rect> = None;

        if mode == DrawMode::Nothing {
            return (batches, paints, blur_params);
        }
        // Finer flattening at 1× so the corners do not polygonise when the
        // governor takes supersampling away.
        let tol = tess::TOLERANCE * if ss > 1 { 1.0 } else { 0.5 };
        let target = Rect::from_size(w as f32, h as f32);

        for cmd in scene.cmds() {
            if mode == DrawMode::SpriteOnly && !cmd.survives_lobotomy() {
                continue;
            }
            match cmd {
                Cmd::Clip(c) => clip = *c,
                Cmd::Fill { path, paint } => {
                    let id = paints.len() as u32;
                    if let Some(range) = tess::fill(&mut self.mesh, path, id, tol) {
                        paints.push(PaintGpu::encode(paint, path.bbox()));
                        push_vector(&mut batches, range, clip);
                    }
                }
                Cmd::Stroke { path, paint, width } => {
                    let id = paints.len() as u32;
                    if let Some(range) = tess::stroke(&mut self.mesh, path, id, *width, tol) {
                        paints.push(PaintGpu::encode(paint, path.bbox()));
                        push_vector(&mut batches, range, clip);
                    }
                }
                Cmd::Text { rect, tex, color } => {
                    let range = self.tex_mesh.quad(*rect, Rect::from_size(1.0, 1.0), color.premul_srgb());
                    push_textured(&mut batches, true, Some(tex.clone()), range, clip);
                }
                Cmd::Sprite { rect, uv, tint, atlas } => {
                    let range = self.tex_mesh.quad(*rect, *uv, tint.premul_srgb());
                    push_textured(&mut batches, false, Some(atlas.clone()), range, clip);
                }
                Cmd::BlurBackdrop { blur } => {
                    let r = blur.radius_px * ss as f32;
                    // Horizontal first (no saturation), then vertical (with).
                    blur_params.push([1.0, 0.0, r, 1.0]);
                    blur_params.push([0.0, 1.0, r, blur.saturate]);
                    batches.push(Batch::Blur { blur: *blur });
                }
                Cmd::Backdrop { rect, radius } => {
                    if let Some(range) = self.tex_mesh.masked_backdrop(
                        *rect,
                        radius.px(),
                        target,
                        [1.0, 1.0, 1.0, 1.0],
                    ) {
                        push_textured(&mut batches, false, None, range, clip);
                    }
                }
            }
        }
        (batches, paints, blur_params)
    }

    // ------------------------------------------------------------- readback

    /// Copy a rendered target back to the CPU. SPEC §4's GPU-test pattern.
    pub fn read(&self, target: &Offscreen) -> Result<Image> {
        self.read_texture(&target.texture, target.w, target.h)
    }

    pub(crate) fn read_texture(&self, texture: &wgpu::Texture, w: u32, h: u32) -> Result<Image> {
        const ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let unpadded = w * 4;
        let padded = unpadded.div_ceil(ALIGN) * ALIGN;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wisp.readback"),
            size: (padded * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(enc.finish()));
        buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        self.device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
            .map_err(|e| PaintError::Readback(format!("{e:?}")))?;
        let view = buf
            .slice(..)
            .get_mapped_range()
            .map_err(|e| PaintError::Readback(format!("{e:?}")))?;
        let mut data = Vec::with_capacity((unpadded * h) as usize);
        for row in 0..h {
            let start = (row * padded) as usize;
            data.extend_from_slice(&view[start..start + unpadded as usize]);
        }
        drop(view);
        buf.unmap();
        Ok(Image { w, h, data })
    }
}

fn push_vector(batches: &mut Vec<Batch>, range: std::ops::Range<u32>, clip: Option<Rect>) {
    if let Some(Batch::Vector { range: prev, clip: pc }) = batches.last_mut() {
        if *pc == clip && prev.end == range.start {
            prev.end = range.end;
            return;
        }
    }
    batches.push(Batch::Vector { range, clip });
}

fn push_textured(
    batches: &mut Vec<Batch>,
    alpha: bool,
    tex: Option<Texture>,
    range: std::ops::Range<u32>,
    clip: Option<Rect>,
) {
    if let Some(Batch::Textured { alpha: pa, tex: pt, range: prev, clip: pc }) = batches.last_mut() {
        if *pa == alpha && *pt == tex && *pc == clip && prev.end == range.start {
            prev.end = range.end;
            return;
        }
    }
    batches.push(Batch::Textured { alpha, tex, range, clip });
}

/// Logical clip rect → integer scissor in supersampled target pixels.
fn scissor(r: Rect, ss: u32, tw: u32, th: u32) -> (u32, u32, u32, u32) {
    let s = ss as f32;
    let x0 = (r.x * s).floor().max(0.0) as u32;
    let y0 = (r.y * s).floor().max(0.0) as u32;
    let x1 = (r.right() * s).ceil().max(0.0) as u32;
    let y1 = (r.bottom() * s).ceil().max(0.0) as u32;
    let x0 = x0.min(tw);
    let y0 = y0.min(th);
    let x1 = x1.min(tw);
    let y1 = y1.min(th);
    (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
}

/// SPEC §3.1. Downgrades take effect on the very next frame — there is nothing
/// to shed asynchronously, because a frame is the unit of work.
impl Governed for Painter {
    fn set_tier(&mut self, tier: Tier, _reason: &TierReason) {
        self.tier = tier;
    }

    fn cost_at(tier: Tier) -> Cost {
        // The scratch chain is three full-size RGBA8 targets. At 1× on a
        // 512×512 surface that is ~3 MiB; at 2× it is ~12 MiB. Pipelines and
        // the atlas add a fixed slice on top.
        match tier {
            Tier::Feral | Tier::Full => Cost { ram_mib: 24, vram_mib: 32, cpu_centi_pct: 400 },
            Tier::Reduced => Cost { ram_mib: 16, vram_mib: 12, cpu_centi_pct: 150 },
            Tier::Lobotomised => Cost { ram_mib: 8, vram_mib: 8, cpu_centi_pct: 40 },
            Tier::Dormant => Cost::FREE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_draw_mode_ladder_matches_the_spec() {
        let modes = [
            (Tier::Feral, DrawMode::Vector),
            (Tier::Full, DrawMode::Vector),
            (Tier::Reduced, DrawMode::Vector),
            (Tier::Lobotomised, DrawMode::SpriteOnly),
            (Tier::Dormant, DrawMode::Nothing),
        ];
        for (t, want) in modes {
            let mode = match t {
                Tier::Feral | Tier::Full | Tier::Reduced => DrawMode::Vector,
                Tier::Lobotomised => DrawMode::SpriteOnly,
                Tier::Dormant => DrawMode::Nothing,
            };
            assert_eq!(mode, want, "{t:?}");
        }
    }

    #[test]
    fn cost_falls_monotonically_down_the_ladder() {
        let mut prev = u32::MAX;
        for t in [Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant] {
            let c = <Painter as Governed>::cost_at(t);
            assert!(c.vram_mib <= prev, "{t:?} costs more VRAM than the tier above");
            prev = c.vram_mib;
        }
        assert_eq!(<Painter as Governed>::cost_at(Tier::Dormant), Cost::FREE);
    }

    #[test]
    fn scissor_rects_are_clamped_into_the_target() {
        assert_eq!(scissor(Rect::new(-10.0, -10.0, 20.0, 20.0), 1, 100, 100), (0, 0, 10, 10));
        assert_eq!(scissor(Rect::new(10.0, 10.0, 10.0, 10.0), 2, 100, 100), (20, 20, 20, 20));
        // Entirely off-target collapses to nothing rather than wrapping.
        let (_, _, w, h) = scissor(Rect::new(500.0, 500.0, 10.0, 10.0), 1, 100, 100);
        assert_eq!((w, h), (0, 0));
    }

    #[test]
    fn contiguous_vector_batches_merge() {
        let mut b = Vec::new();
        push_vector(&mut b, 0..6, None);
        push_vector(&mut b, 6..12, None);
        assert_eq!(b.len(), 1);
        // A clip change breaks the run.
        push_vector(&mut b, 12..18, Some(Rect::from_size(10.0, 10.0)));
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn textured_batches_break_on_a_texture_change() {
        let mut b = Vec::new();
        push_textured(&mut b, true, None, 0..6, None);
        push_textured(&mut b, true, None, 6..12, None);
        assert_eq!(b.len(), 1);
        push_textured(&mut b, false, None, 12..18, None);
        assert_eq!(b.len(), 2, "an alpha/rgba switch is a pipeline switch");
    }

    #[test]
    fn an_image_unpremultiplies_back_to_the_colour_that_was_drawn() {
        let violet = wisp_theme::palette::VIOLET;
        let p = violet.premul_srgb();
        let px: Vec<u8> = p.iter().map(|c| (c * 255.0).round() as u8).collect();
        let img = Image { w: 1, h: 1, data: px };
        assert_eq!(img.color(0, 0), violet);
        assert_eq!(Image { w: 1, h: 1, data: vec![0, 0, 0, 0] }.color(0, 0), Color::TRANSPARENT);
    }

    #[test]
    fn image_difference_is_zero_against_itself() {
        let img = Image { w: 2, h: 1, data: vec![1, 2, 3, 4, 5, 6, 7, 8] };
        assert_eq!(img.mean_abs_diff(&img), 0.0);
        assert_eq!(img.max_abs_diff(&img), 0);
    }
}
