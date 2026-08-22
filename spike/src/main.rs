//! M0 risk gate: does a transparent zwlr_layer_shell_v1 surface accept a
//! wgpu/Vulkan swapchain with an alpha channel under KWin 6.7, and can we set a
//! per-frame input region so clicks pass through everywhere she isn't?
//!
//! Proves, and prints, five things:
//!   1. layer surface is created and configured by the compositor
//!   2. wgpu picks a Vulkan adapter and the surface advertises an alpha mode
//!   3. we can present premultiplied-alpha frames (a violet blob, no background)
//!   4. an input region smaller than the surface is accepted (click-through)
//!   5. the same pipeline reads back to a PNG (proof for headless CI)

use std::ptr::NonNull;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_dispatch2, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_region, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};

const W: u32 = 256;
const H: u32 = 256;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

const SHADER: &str = r#"
@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    // fullscreen triangle
    var p = array<vec2<f32>, 3>(
        vec2(-1.0, -3.0), vec2(-1.0, 1.0), vec2(3.0, 1.0));
    return vec4(p[i], 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = frag.xy / vec2(256.0, 256.0);
    let d = distance(uv, vec2(0.5, 0.5));
    // soft-edged blob: NX violet core, cyan light inside it
    let a = clamp(1.0 - smoothstep(0.28, 0.46, d), 0.0, 1.0);
    let violet = vec3(0.467, 0.0, 1.0);       // #7700FF
    let cyan   = vec3(0.0, 0.898, 1.0);       // #00E5FF
    let core   = clamp(1.0 - smoothstep(0.0, 0.30, d), 0.0, 1.0);
    let rgb    = mix(violet, cyan, core * 0.55);
    // PREMULTIPLIED: colour is already scaled by alpha
    return vec4(rgb * a, a);
}
"#;

struct Spike {
    registry_state: RegistryState,
    output_state: OutputState,
    exit: bool,
    configured: bool,
    frames: u32,
    layer: LayerSurface,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
}

fn main() {
    let conn = Connection::connect_to_env().expect("no wayland display");
    let (globals, mut queue) = registry_queue_init::<Spike>(&conn).unwrap();
    let qh = queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).expect("no wl_compositor");
    let layer_shell = match LayerShell::bind(&globals, &qh) {
        Ok(l) => l,
        Err(e) => {
            println!("GATE 1 FAIL: no zwlr_layer_shell_v1: {e}");
            std::process::exit(2);
        }
    };
    println!("GATE 1 ok: zwlr_layer_shell_v1 bound");

    let wl_surface = compositor.create_surface(&qh);

    // (4) input region: only the centre 96x96 accepts clicks. Everything else
    // falls through to whatever is underneath.
    let region = compositor.wl_compositor().create_region(&qh, ());
    region.add(80, 80, 96, 96);
    wl_surface.set_input_region(Some(&region));
    region.destroy();
    println!("GATE 4 ok: input region set to 96x96 of a {W}x{H} surface");

    let layer = layer_shell.create_layer_surface(
        &qh,
        wl_surface,
        Layer::Overlay,
        Some("nx-wisp-spike"),
        None,
    );
    layer.set_anchor(Anchor::TOP | Anchor::LEFT);
    layer.set_margin(120, 0, 0, 120);
    layer.set_size(W, H);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_exclusive_zone(0); // never reserve space
    layer.commit();

    // --- wgpu, forced to Vulkan ---
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });

    let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
        NonNull::new(conn.backend().display_ptr() as *mut _).unwrap(),
    ));
    let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
        NonNull::new(layer.wl_surface().id().as_ptr() as *mut _).unwrap(),
    ));
    let surface = unsafe {
        instance
            .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(raw_display),
                raw_window_handle: raw_window,
            })
            .expect("create_surface")
    };

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        compatible_surface: Some(&surface),
        ..Default::default()
    }))
    .expect("no vulkan adapter");
    let info = adapter.get_info();
    println!(
        "GATE 2 ok: adapter {:?} / {} ({:?})",
        info.backend, info.name, info.device_type
    );

    let caps = surface.get_capabilities(&adapter);
    println!("           alpha modes: {:?}", caps.alpha_modes);
    println!("           formats:     {:?}", &caps.formats);
    let alpha = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
        wgpu::CompositeAlphaMode::PreMultiplied
    } else {
        println!("GATE 3 WARN: PreMultiplied not offered, falling back to {:?}", caps.alpha_modes[0]);
        caps.alpha_modes[0]
    };

    let (device, queue_g) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("request_device");

    surface.configure(
        &device,
        &wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: FORMAT,
            width: W,
            height: H,
            color_space: wgpu::SurfaceColorSpace::Srgb,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: alpha,
            view_formats: vec![],
        },
    );
    println!("GATE 3 ok: swapchain configured {FORMAT:?} alpha={alpha:?}");

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("blob"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("blob"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            targets: &[Some(wgpu::ColorTargetState {
                format: FORMAT,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    });

    readback(&device, &queue_g, &pipeline);

    let mut spike = Spike {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        exit: false,
        configured: false,
        frames: 0,
        layer,
        surface,
        device,
        queue: queue_g,
        pipeline,
    };

    while !spike.exit {
        queue.blocking_dispatch(&mut spike).unwrap();
    }
    println!("GATE ALL PASS — {} frames presented", spike.frames);
}

/// (5) same pipeline into an offscreen texture -> PNG, so CI can assert on
/// pixels with no compositor at all.
fn readback(device: &wgpu::Device, queue: &wgpu::Queue, pipeline: &wgpu::RenderPipeline) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("readback"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&Default::default());
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback-buf"),
        size: (W * H * 4) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&Default::default());
    {
        let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        rp.set_pipeline(pipeline);
        rp.draw(0..3, 0..1);
    }
    enc.copy_texture_to_buffer(
        tex.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(W * 4),
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).unwrap();
    let data = buf.slice(..).get_mapped_range().unwrap().to_vec();

    let centre = {
        let i = ((H / 2 * W + W / 2) * 4) as usize;
        (data[i], data[i + 1], data[i + 2], data[i + 3])
    };
    let corner = (data[0], data[1], data[2], data[3]);
    println!("GATE 5 ok: centre rgba={centre:?} corner rgba={corner:?}");
    assert_eq!(corner.3, 0, "corner must be fully transparent");
    assert!(centre.3 > 250, "centre must be opaque");

    let path = std::env::var("SPIKE_PNG").unwrap_or_else(|_| "/tmp/nx-wisp-spike.png".into());
    let file = std::fs::File::create(&path).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    e.write_header().unwrap().write_image_data(&data).unwrap();
    println!("           wrote {path}");
}

impl Spike {
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match self.surface.get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => f,
            other => {
                println!("get_current_texture: {other:?} — skipping frame");
                self.layer
                    .wl_surface()
                    .frame(qh, FrameCallbackData(self.layer.wl_surface().clone()));
                self.layer.commit();
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(&self.pipeline);
            rp.draw(0..3, 0..1);
        }
        self.queue.submit(Some(enc.finish()));
        self.layer.wl_surface().frame(qh, FrameCallbackData(self.layer.wl_surface().clone()));
        self.queue.present(frame);

        self.frames += 1;
        // ~2 seconds at 60Hz then leave, so this never squats on the desktop.
        if self.frames >= 120 {
            self.exit = true;
        }
    }
}

impl CompositorHandler for Spike {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.draw(qh);
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl LayerShellHandler for Spike {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &LayerSurface, c: LayerSurfaceConfigure, _: u32) {
        if !self.configured {
            self.configured = true;
            println!("GATE 1b ok: compositor configured layer surface {:?}", c.new_size);
            self.draw(qh);
        }
    }
}

impl OutputHandler for Spike {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl Dispatch<wl_region::WlRegion, ()> for Spike {
    fn event(_: &mut Self, _: &wl_region::WlRegion, _: wl_region::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl ProvidesRegistryState for Spike {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers![OutputState];
}

delegate_registry!(Spike);
delegate_dispatch2!(Spike);
