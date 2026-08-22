//! The layer surface itself.

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
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
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
    protocol::{wl_output, wl_pointer, wl_region, wl_seat, wl_surface},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wisp_paint::{Painter, Scene, TextEngine};
use wisp_proto::Tier;
use wisp_rig::{Polygon, RigFrame, Vec2};

use crate::ShellError;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// Her drawn size in logical pixels (F75).
    pub size_px: f32,
    /// Head-room around her so a throw or a stretch is not clipped.
    pub padding_px: f32,
    /// Minimum surface side, so a speech bubble beside her is not clipped.
    pub bubble_room_px: f32,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self { size_px: 160.0, padding_px: 80.0, bubble_room_px: 620.0 }
    }
}

/// What the surface did this dispatch, handed back to the host loop.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tick {
    pub configured: bool,
    pub drew: bool,
    /// Pointer position within the surface, if it is over us.
    pub pointer: Option<Vec2>,
    pub grabbed: bool,
    pub released: bool,
    pub closed: bool,
}

pub struct WispShell {
    // DROP ORDER IS LOAD-BEARING, twice over.
    //
    // `state` holds the wgpu Surface, whose teardown destroys a Vulkan
    // swapchain, which marshals a request down the Wayland connection. So the
    // connection must OUTLIVE the surface. With `conn` declared first every
    // clean exit segfaulted inside wl_proxy_marshal_flags, called from
    // destroy_swapchain, called from dropping State.
    //
    // Within State the same rule applies again: the wgpu surface is declared
    // before the LayerSurface that owns its wl_surface.
    state: State,
    queue: EventQueue<State>,
    conn: Connection,
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor: CompositorState,
    pointer: Option<wl_pointer::WlPointer>,

    // DROP ORDER IS LOad-BEARING. Rust drops fields in declaration order, and
    // the wgpu surface holds a raw pointer to the wl_surface that `layer` owns.
    // With `layer` declared first, every clean exit destroyed the wl_surface
    // while Vulkan still referenced it and the process dumped core on the way
    // out. wgpu first, wl_surface after.
    surface: wgpu::Surface<'static>,
    painter: Painter,
    text: TextEngine,
    scene: Scene,
    layer: LayerSurface,

    width: u32,
    height: u32,
    /// The output she is roaming, in logical pixels. Until the compositor tells
    /// us, we assume nothing and keep her inside the surface.
    output: Option<(i32, i32)>,
    /// The output the surface is currently on, once the compositor says so.
    on_output: Option<wl_output::WlOutput>,
    /// Surface position, as layer-shell margins from the top-left of the output.
    pos: (i32, i32),
    configured: bool,
    exit: bool,
    tick: Tick,
    /// Frames drawn, for the self-dump.
    frames: u64,
    /// The polygon currently installed as the input region, so we only rebuild
    /// the wl_region when the silhouette actually changed. Rebuilding it every
    /// frame is a round trip per frame for nothing.
    region_hash: u64,
}

impl WispShell {
    pub fn new(cfg: &ShellConfig) -> Result<WispShell, ShellError> {
        let conn = Connection::connect_to_env().map_err(|_| ShellError::NoDisplay)?;
        let (globals, queue) =
            registry_queue_init::<State>(&conn).map_err(|_| ShellError::NoDisplay)?;
        let qh = queue.handle();

        let compositor =
            CompositorState::bind(&globals, &qh).map_err(|_| ShellError::NoLayerShell)?;
        let layer_shell = LayerShell::bind(&globals, &qh).map_err(|_| ShellError::NoLayerShell)?;

        // Big enough for her AND a speech bubble on any side of her, because
        // a bubble that spills past the surface is simply clipped away. Still
        // two orders of magnitude cheaper than a fullscreen overlay.
        let side = (cfg.size_px + cfg.padding_px * 2.0)
            .max(cfg.bubble_room_px)
            .ceil() as u32;
        let wl_surface = compositor.create_surface(&qh);
        let layer = layer_shell.create_layer_surface(
            &qh,
            wl_surface,
            Layer::Overlay,
            Some("nx-wisp"),
            None,
        );
        // Anchored to a corner so margins act as absolute placement — this is
        // the only way a Wayland client can put itself at a chosen coordinate.
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        layer.set_size(side, side);
        layer.set_margin(200, 0, 0, 200);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // She must never reserve screen space; maximised windows ignore her.
        layer.set_exclusive_zone(-1);
        // Before the first configure, an empty input region: no stealing clicks
        // while she has nothing drawn.
        set_input_region(&compositor, &qh, layer.wl_surface(), None, 1.0);
        layer.commit();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(conn.backend().display_ptr() as *mut _).ok_or(ShellError::NoDisplay)?,
        ));
        let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(layer.wl_surface().id().as_ptr() as *mut _).ok_or(ShellError::NoDisplay)?,
        ));
        let surface = unsafe {
            instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(raw_display),
                    raw_window_handle: raw_window,
                })
                .map_err(|e| ShellError::Wgpu(e.to_string()))?
        };
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|_| ShellError::NoAdapter)?;
        let (device, gqueue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| ShellError::Wgpu(e.to_string()))?;

        let caps = surface.get_capabilities(&adapter);
        let alpha = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            // Without this she is a black box. Proven present on KWin 6.7, but
            // say so plainly rather than drawing something ugly.
            tracing::error!("compositor offers no premultiplied alpha; she will have a background");
            caps.alpha_modes[0]
        };
        surface.configure(
            &device,
            &wgpu::SurfaceConfiguration {
                // COPY_SRC so she can photograph herself: NX_WISP_DUMP_FRAME
                // reads the swapchain back to a PNG. Nothing in this repo may
                // open a window on the operator's desktop, so a self-dump
                // inside a nested compositor is how we ever look at her.
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC,
                format: FORMAT,
                width: side,
                height: side,
                color_space: wgpu::SurfaceColorSpace::Srgb,
                present_mode: wgpu::PresentMode::Fifo,
                desired_maximum_frame_latency: 2,
                alpha_mode: alpha,
                view_formats: vec![],
            },
        );

        // Painter takes ownership; the surface was already created from this
        // instance, and wgpu keeps the objects alive behind the surface.
        let painter = Painter::from_device(instance, adapter, device, gqueue);

        let state = State {
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            seat_state: SeatState::new(&globals, &qh),
            compositor,
            pointer: None,
            surface,
            painter,
            text: TextEngine::new(),
            scene: Scene::new(),
            layer,
            width: side,
            height: side,
            pos: (200, 200),
            output: None,
            on_output: None,
            frames: 0,
            configured: false,
            exit: false,
            tick: Tick::default(),
            region_hash: u64::MAX,
        };
        let mut shell = WispShell { state, queue, conn };
        // registry_queue_init only BINDS the globals; the wl_output geometry,
        // mode and done events arrive afterwards. Without a roundtrip here
        // OutputState::info returns None and she concludes the screen is
        // exactly as big as her own surface — so she could never roam.
        // Two roundtrips: the first delivers the outputs, the second their
        // xdg_output logical sizes.
        for _ in 0..2 {
            let _ = shell.queue.roundtrip(&mut shell.state);
        }
        Ok(shell)
    }

    /// Pump the compositor. Non-blocking: returns what happened.
    pub fn pump(&mut self) -> Tick {
        self.state.tick = Tick::default();
        let _ = self.queue.dispatch_pending(&mut self.state);
        let _ = self.conn.flush();
        self.state.tick.closed = self.state.exit;
        self.state.tick
    }

    /// Block until the compositor has something to say. Used when she is
    /// asleep — at T3/T4 spinning a frame loop is exactly what we promised not
    /// to do.
    pub fn block(&mut self) -> Tick {
        self.state.tick = Tick::default();
        let _ = self.queue.blocking_dispatch(&mut self.state);
        self.state.tick.closed = self.state.exit;
        self.state.tick
    }

    pub fn is_configured(&self) -> bool {
        self.state.configured
    }

    /// Move her. Layer-shell margins are the only client-side placement
    /// Wayland offers, and they are in output-relative logical pixels.
    pub fn set_position(&mut self, x: i32, y: i32) {
        if self.state.pos == (x, y) {
            return;
        }
        self.state.pos = (x, y);
        self.state.layer.set_margin(y, 0, 0, x);
        self.state.layer.commit();
    }

    pub fn position(&self) -> (i32, i32) {
        self.state.pos
    }

    /// The roaming area in logical pixels — the output she is on, once the
    /// compositor has told us about it.
    pub fn output_size(&self) -> Option<(i32, i32)> {
        self.state.live_output_size()
    }

    /// Diagnostic: what the compositor has actually told us about outputs.
    pub fn describe_outputs(&self) -> Vec<String> {
        self.state
            .output_state
            .outputs()
            .map(|o| {
                match self.state.output_state.info(&o) {
                    Some(i) => format!(
                        "name={:?} logical={:?} modes={} current={:?}",
                        i.name,
                        i.logical_size,
                        i.modes.len(),
                        i.modes.iter().find(|m| m.current).map(|m| m.dimensions)
                    ),
                    None => "no info yet".to_string(),
                }
            })
            .collect()
    }

    /// Park the surface so it is centred on `at`, an OUTPUT-space position, and
    /// return where she should be drawn WITHIN the surface.
    ///
    /// A fullscreen overlay would be the obvious way to let her roam, but it
    /// clears a whole screen's worth of buffer every frame for a creature the
    /// size of a postage stamp — which is exactly the cost the charter forbids.
    /// So the surface stays small and follows her instead. We already commit
    /// once per frame to draw, so moving it costs no extra round trip.
    pub fn follow(&mut self, at: Vec2) -> Vec2 {
        let (ox, oy) = self.origin_for(at);
        self.set_position(ox, oy);
        Vec2 { x: at.x - ox as f32, y: at.y - oy as f32 }
    }

    /// Where the surface would be parked for `at`, without moving it. Pure, so
    /// the host can work out her in-surface position for the frame it is about
    /// to build and have it match what `follow` will do next frame exactly.
    pub fn origin_for(&self, at: Vec2) -> (i32, i32) {
        let (sw, sh) = (self.state.width as f32, self.state.height as f32);
        let mut ox = (at.x - sw * 0.5).round() as i32;
        let mut oy = (at.y - sh * 0.5).round() as i32;
        if let Some((ow, oh)) = self.state.live_output_size() {
            // Never park the surface off the edge of the output: the compositor
            // clamps it, and she would silently stop tracking.
            ox = ox.clamp(0, (ow - sw as i32).max(0));
            oy = oy.clamp(0, (oh - sh as i32).max(0));
        } else {
            ox = ox.max(0);
            oy = oy.max(0);
        }
        (ox, oy)
    }

    /// Her position within the surface, for a given output-space position.
    pub fn local_for(&self, at: Vec2) -> Vec2 {
        let (ox, oy) = self.origin_for(at);
        Vec2 { x: at.x - ox as f32, y: at.y - oy as f32 }
    }

    pub fn surface_size(&self) -> (u32, u32) {
        (self.state.width, self.state.height)
    }

    /// Draw calls in the last frame — proof that an overlay actually reached
    /// the GPU rather than being built and dropped.
    pub fn last_draw_calls(&self) -> usize {
        self.state.painter.last_draw_calls()
    }

    pub fn set_tier(&mut self, tier: Tier) {
        use wisp_proto::Governed;
        self.state.painter.set_tier(tier, &wisp_proto::TierReason::Idle);
    }

    /// Draw one posed frame and install its silhouette as the input region.
    pub fn draw(&mut self, frame: &RigFrame, outline: &Polygon) {
        self.draw_with(frame, outline, |_, _, _| {});
    }

    /// As [`draw`], plus a hook to append commands after her — speech bubbles
    /// and the invasive-sense tell. The closure gets the painter (for text
    /// rasterisation) and the scene, which is why it is a callback rather than
    /// a `&Scene` argument: both live inside the shell.
    pub fn draw_with(
        &mut self,
        frame: &RigFrame,
        outline: &Polygon,
        overlay: impl FnOnce(&Painter, &mut TextEngine, &mut Scene),
    ) {
        if !self.state.configured {
            return;
        }
        let qh = self.queue.handle();
        self.state.install_region(&qh, outline);
        self.state.render_with(frame, overlay);
        self.state
            .layer
            .wl_surface()
            .frame(&qh, FrameCallbackData(self.state.layer.wl_surface().clone()));
        self.state.layer.commit();
        let _ = self.conn.flush();
    }
}

/// Hash a polygon cheaply so we can skip identical regions.
fn hash_polygon(p: &Polygon) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for pt in &p.points {
        for v in [pt.x, pt.y] {
            h ^= (v * 4.0).round() as i64 as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

/// A wl_region approximating the polygon with rectangles.
///
/// Wayland input regions are unions of rectangles — there is no polygon
/// primitive — so a silhouette becomes horizontal spans. `None` means "accept
/// nothing", which is how she stays click-through before her first frame.
fn set_input_region(
    compositor: &CompositorState,
    qh: &QueueHandle<State>,
    surface: &wl_surface::WlSurface,
    poly: Option<&Polygon>,
    _scale: f32,
) {
    let region = compositor.wl_compositor().create_region(qh, ());
    if let Some(poly) = poly {
        for (y, x0, x1) in spans(poly) {
            region.add(x0, y, (x1 - x0).max(1), SPAN_H);
        }
    }
    surface.set_input_region(Some(&region));
    region.destroy();
}

/// Height of one input-region band. Four pixels keeps the region under a few
/// dozen rectangles for a 160px character while staying visually exact enough
/// that no one can tell where the clickable area ends.
const SPAN_H: i32 = 4;

/// Scanline the polygon into horizontal spans.
fn spans(poly: &Polygon) -> Vec<(i32, i32, i32)> {
    let pts = &poly.points;
    if pts.len() < 3 {
        return Vec::new();
    }
    let (mut top, mut bot) = (f32::MAX, f32::MIN);
    for p in pts {
        top = top.min(p.y);
        bot = bot.max(p.y);
    }
    let mut out = Vec::new();
    // Bands are laid on a fixed grid, and each is sampled at its midpoint. The
    // midpoint of the LAST band falls below the polygon, which used to leave a
    // dead strip along her bottom edge where she could not be grabbed — so the
    // sample is clamped just inside the extent instead.
    let eps = 0.01_f32;
    let mut y = (top.floor() as i32).div_euclid(SPAN_H) * SPAN_H;
    while (y as f32) < bot {
        let scan = (y as f32 + SPAN_H as f32 * 0.5).clamp(top + eps, bot - eps);
        let mut xs: Vec<f32> = Vec::new();
        for i in 0..pts.len() {
            let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
            if (a.y <= scan && b.y > scan) || (b.y <= scan && a.y > scan) {
                let t = (scan - a.y) / (b.y - a.y);
                xs.push(a.x + t * (b.x - a.x));
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        for pair in xs.chunks(2) {
            if let [x0, x1] = pair {
                out.push((y, x0.floor() as i32, x1.ceil() as i32));
            }
        }
        y += SPAN_H;
    }
    out
}

impl State {
    /// Remember how big the output is, so she knows where the edges are.
    ///
    /// `logical_size` is what we want — it is already scale-corrected, which
    /// the raw mode is not. On a mixed-DPI setup taking the mode would put her
    /// edges in the wrong place.
    fn learn_output(&mut self, o: &wl_output::WlOutput) {
        if let Some(size) = self.size_of(o) {
            self.output = Some(size);
        }
    }

    fn size_of(&self, o: &wl_output::WlOutput) -> Option<(i32, i32)> {
        let info = self.output_state.info(o)?;
        // logical_size is already scale-corrected; the raw mode is not, so on a
        // mixed-DPI setup taking the mode puts her edges in the wrong place.
        info.logical_size
            .or_else(|| info.modes.iter().find(|m| m.current).map(|m| m.dimensions))
    }

    /// Ask the compositor now, rather than trusting what we cached.
    ///
    /// sctk only fills `OutputInfo` after the output's `done` event, so reading
    /// it inside `new_output` yields an empty record — which is why she used to
    /// think the screen was exactly as big as her own surface.
    fn live_output_size(&self) -> Option<(i32, i32)> {
        if let Some(o) = &self.on_output {
            if let Some(s) = self.size_of(o) {
                return Some(s);
            }
        }
        self.output_state
            .outputs()
            .find_map(|o| self.size_of(&o))
            .or(self.output)
    }

    /// Install the silhouette as the surface's input region, so clicks land on
    /// her and pass through everywhere else (F2).
    ///
    /// Skipped when the outline is unchanged: rebuilding a wl_region is a round
    /// trip, and at 60fps most frames do not move her outline at all. The
    /// region takes effect on the next commit, which `draw` performs.
    fn install_region(&mut self, qh: &QueueHandle<State>, outline: &Polygon) {
        let h = hash_polygon(outline);
        if h == self.region_hash {
            return;
        }
        self.region_hash = h;
        let poly = if outline.is_empty() { None } else { Some(outline) };
        set_input_region(&self.compositor, qh, self.layer.wl_surface(), poly, 1.0);
    }

    fn render_with(
        &mut self,
        frame: &RigFrame,
        overlay: impl FnOnce(&Painter, &mut TextEngine, &mut Scene),
    ) {
        use wgpu::CurrentSurfaceTexture as Cst;
        let tex = match self.surface.get_current_texture() {
            Cst::Success(t) | Cst::Suboptimal(t) => t,
            other => {
                tracing::debug!("skipping frame: {other:?}");
                return;
            }
        };
        crate::bridge::scene_of(frame, &mut self.scene);
        // Split borrows: the painter is read while the scene and text engine
        // are written. Distinct fields, so this is fine.
        overlay(&self.painter, &mut self.text, &mut self.scene);
        let view = tex.texture.create_view(&Default::default());
        if let Err(e) =
            self.painter.render_to_view(&view, self.width, self.height, &self.scene)
        {
            tracing::warn!("render failed: {e}");
        }
        // Photograph ourselves before presenting, if asked. Deliberately after
        // the render and before present, so what lands in the file is exactly
        // the frame the compositor is about to show.
        self.frames += 1;
        if let Ok(path) = std::env::var("NX_WISP_DUMP_FRAME") {
            let after: u64 = std::env::var("NX_WISP_DUMP_AFTER")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60);
            if self.frames == after {
                self.dump(&tex.texture, &path);
            }
        }
        self.painter.queue().present(tex);
        self.tick.drew = true;
    }
}

impl State {
    /// Read the swapchain back and write a PNG. Slow and synchronous — this
    /// only ever runs for a test.
    fn dump(&self, tex: &wgpu::Texture, path: &str) {
        let (w, h) = (self.width, self.height);
        // Buffer rows must be a multiple of 256 bytes.
        let unpadded = w * 4;
        let pad = (256 - (unpadded % 256)) % 256;
        let padded = unpadded + pad;
        let device = self.painter.device();
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wisp.dump"),
            size: (padded * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            tex.as_image_copy(),
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
        self.painter.queue().submit(Some(enc.finish()));
        buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
        let data = match buf.slice(..).get_mapped_range() {
            Ok(d) => d.to_vec(),
            Err(e) => {
                tracing::warn!("frame dump failed to map: {e:?}");
                return;
            }
        };
        let mut rgba = Vec::with_capacity((unpadded * h) as usize);
        for row in 0..h {
            let start = (row * padded) as usize;
            rgba.extend_from_slice(&data[start..start + unpadded as usize]);
        }
        match std::fs::File::create(path) {
            Ok(f) => {
                let mut e = png::Encoder::new(std::io::BufWriter::new(f), w, h);
                e.set_color(png::ColorType::Rgba);
                e.set_depth(png::BitDepth::Eight);
                if let Ok(mut wr) = e.write_header() {
                    let _ = wr.write_image_data(&rgba);
                }
                tracing::info!(path, w, h, "wrote a frame dump");
            }
            Err(e) => tracing::warn!("frame dump could not be written: {e}"),
        }
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.tick.drew = false; // host draws on the next pump
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, o: &wl_output::WlOutput) {
        self.on_output = Some(o.clone());
    }
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, c: LayerSurfaceConfigure, _: u32) {
        if c.new_size.0 > 0 {
            self.width = c.new_size.0;
            self.height = c.new_size.1;
        }
        self.configured = true;
        self.tick.configured = true;
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState { &mut self.seat_state }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for State {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for e in events {
            let at = Vec2 { x: e.position.0 as f32, y: e.position.1 as f32 };
            match e.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.tick.pointer = Some(at);
                }
                PointerEventKind::Press { .. } => {
                    self.tick.pointer = Some(at);
                    self.tick.grabbed = true;
                }
                PointerEventKind::Release { .. } => {
                    self.tick.pointer = Some(at);
                    self.tick.released = true;
                }
                PointerEventKind::Leave { .. } => {}
                _ => {}
            }
        }
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, o: wl_output::WlOutput) {
        self.learn_output(&o);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, o: wl_output::WlOutput) {
        self.learn_output(&o);
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl Dispatch<wl_region::WlRegion, ()> for State {
    fn event(_: &mut Self, _: &wl_region::WlRegion, _: wl_region::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(State);
delegate_dispatch2!(State);

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x0: f32, y0: f32, x1: f32, y1: f32) -> Polygon {
        Polygon {
            points: vec![
                Vec2 { x: x0, y: y0 },
                Vec2 { x: x1, y: y0 },
                Vec2 { x: x1, y: y1 },
                Vec2 { x: x0, y: y1 },
            ],
        }
    }

    #[test]
    fn a_square_scanlines_into_bands_covering_it() {
        let s = spans(&square(10.0, 10.0, 50.0, 50.0));
        assert!(!s.is_empty());
        for (_, x0, x1) in &s {
            assert!(*x0 <= 10 && *x1 >= 50, "band {x0}..{x1} does not cover the square");
        }
        let first = s.iter().map(|(y, ..)| *y).min().unwrap();
        let last = s.iter().map(|(y, ..)| *y).max().unwrap();
        assert!(first <= 10, "bands start at {first}, below the square's top");
        assert!(
            last + SPAN_H >= 50,
            "bands end at {}, leaving a dead strip above the square's bottom edge",
            last + SPAN_H
        );
    }

    #[test]
    fn a_degenerate_polygon_produces_no_region_rather_than_panicking() {
        assert!(spans(&Polygon { points: vec![] }).is_empty());
        assert!(spans(&Polygon { points: vec![Vec2 { x: 0.0, y: 0.0 }] }).is_empty());
    }

    #[test]
    fn identical_outlines_hash_the_same_and_a_moved_one_does_not() {
        let a = square(0.0, 0.0, 20.0, 20.0);
        let b = square(0.0, 0.0, 20.0, 20.0);
        let c = square(1.0, 0.0, 21.0, 20.0);
        assert_eq!(hash_polygon(&a), hash_polygon(&b));
        assert_ne!(hash_polygon(&a), hash_polygon(&c));
    }
}
