//! A normal application window, for the rig editor.
//!
//! She lives on a layer surface because a creature must place itself anywhere
//! on screen. The editor is the opposite: it is chrome, it wants a title bar,
//! it wants to be tiled and alt-tabbed and resized like anything else, and it
//! wants the keyboard. So it gets an `xdg_toplevel`, not a layer surface.

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
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
    Connection, EventQueue, Proxy, QueueHandle,
};
use wisp_paint::{Painter, Scene, TextEngine};

use crate::ShellError;

// The painter builds its pipelines for Rgba8Unorm and emits premultiplied
// sRGB itself, so the window must present in the same format as the layer
// surface. Bgra8UnormSrgb here cost a validation error: "the RenderPass uses
// textures with formats [Bgra8UnormSrgb] but the RenderPipeline uses
// [Rgba8Unorm]".
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// What happened since the last pump.
#[derive(Debug, Default, Clone)]
pub struct WindowTick {
    pub resized: bool,
    pub closed: bool,
    pub pointer: Option<(f32, f32)>,
    pub press: bool,
    pub release: bool,
    pub scroll: f32,
    /// Held modifiers at the time of the key events below.
    pub ctrl: bool,
    pub shift: bool,
    /// Keysyms pressed this pump, in order.
    pub keys: Vec<Keysym>,
}

pub struct EditorWindow {
    // Drop order, as in `layer.rs`: the wgpu surface marshals through the
    // connection when its swapchain is destroyed, so state must go first and
    // the connection last. Getting this wrong is a segfault on every clean
    // exit, and it is not obvious from reading the fields.
    state: State,
    queue: EventQueue<State>,
    conn: Connection,
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    modifiers: Modifiers,

    surface: wgpu::Surface<'static>,
    painter: Painter,
    text: TextEngine,
    scene: Scene,
    window: Window,

    width: u32,
    height: u32,
    configured: bool,
    exit: bool,
    tick: WindowTick,
}

impl EditorWindow {
    pub fn new(title: &str, w: u32, h: u32) -> Result<EditorWindow, ShellError> {
        let conn = Connection::connect_to_env().map_err(|_| ShellError::NoDisplay)?;
        let (globals, queue) =
            registry_queue_init::<State>(&conn).map_err(|_| ShellError::NoDisplay)?;
        let qh = queue.handle();

        let compositor =
            CompositorState::bind(&globals, &qh).map_err(|_| ShellError::NoLayerShell)?;
        let xdg = XdgShell::bind(&globals, &qh).map_err(|_| ShellError::NoLayerShell)?;

        let surface_wl = compositor.create_surface(&qh);
        let window = xdg.create_window(surface_wl, WindowDecorations::RequestServer, &qh);
        window.set_title(title);
        window.set_app_id("org.nx.Wisp.Editor");
        window.set_min_size(Some((900, 600)));
        window.commit();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(conn.backend().display_ptr() as *mut _).ok_or(ShellError::NoDisplay)?,
        ));
        let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(window.wl_surface().id().as_ptr() as *mut _)
                .ok_or(ShellError::NoDisplay)?,
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
        let painter = Painter::from_device(instance, adapter, device, gqueue);

        let state = State {
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            seat_state: SeatState::new(&globals, &qh),
            pointer: None,
            keyboard: None,
            modifiers: Modifiers::default(),
            surface,
            painter,
            text: TextEngine::new(),
            scene: Scene::new(),
            window,
            width: w,
            height: h,
            configured: false,
            exit: false,
            tick: WindowTick::default(),
        };
        let mut me = EditorWindow { state, queue, conn };
        for _ in 0..2 {
            let _ = me.queue.roundtrip(&mut me.state);
        }
        Ok(me)
    }

    pub fn is_configured(&self) -> bool {
        self.state.configured
    }
    pub fn size(&self) -> (u32, u32) {
        (self.state.width, self.state.height)
    }

    pub fn pump(&mut self) -> WindowTick {
        self.state.tick = WindowTick::default();
        let _ = self.queue.dispatch_pending(&mut self.state);
        let _ = self.conn.flush();
        self.state.tick.closed = self.state.exit;
        self.state.tick.clone()
    }

    pub fn block(&mut self) -> WindowTick {
        self.state.tick = WindowTick::default();
        let _ = self.queue.blocking_dispatch(&mut self.state);
        self.state.tick.closed = self.state.exit;
        self.state.tick.clone()
    }

    /// Build a frame. The closure gets the painter, the text engine and an
    /// empty scene — the same shape as the layer surface's overlay hook, so a
    /// host that can draw into one can draw into the other.
    pub fn draw(&mut self, build: impl FnOnce(&Painter, &mut TextEngine, &mut Scene)) {
        if !self.state.configured {
            return;
        }
        use wgpu::CurrentSurfaceTexture as Cst;
        let tex = match self.state.surface.get_current_texture() {
            Cst::Success(t) | Cst::Suboptimal(t) => t,
            other => {
                tracing::debug!("editor: skipping frame ({other:?})");
                return;
            }
        };
        self.state.scene.clear();
        build(&self.state.painter, &mut self.state.text, &mut self.state.scene);
        let view = tex.texture.create_view(&Default::default());
        if let Err(e) = self.state.painter.render_to_view(
            &view,
            self.state.width,
            self.state.height,
            &self.state.scene,
        ) {
            tracing::warn!("editor render failed: {e}");
        }
        let qh = self.queue.handle();
        self.state
            .window
            .wl_surface()
            .frame(&qh, FrameCallbackData(self.state.window.wl_surface().clone()));
        self.state.painter.queue().present(tex);
        self.state.window.commit();
        let _ = self.conn.flush();
    }

}

impl State {
    fn configure_surface(&mut self) {
        let device = self.painter.device();
        self.surface.configure(
            device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: FORMAT,
                width: self.width.max(1),
                height: self.height.max(1),
                color_space: wgpu::SurfaceColorSpace::Srgb,
                present_mode: wgpu::PresentMode::Fifo,
                desired_maximum_frame_latency: 2,
                // The editor is chrome: it is a normal opaque window, not a
                // creature that has to composite with the desktop.
                alpha_mode: wgpu::CompositeAlphaMode::Opaque,
                view_formats: vec![],
            },
        );
    }
}

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        c: WindowConfigure,
        _: u32,
    ) {
        let (w, h) = c.new_size;
        self.width = w.map(|v| v.get()).unwrap_or(self.width);
        self.height = h.map(|v| v.get()).unwrap_or(self.height);
        self.configure_surface();
        self.configured = true;
        self.tick.resized = true;
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState { &mut self.seat_state }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        match cap {
            Capability::Pointer if self.pointer.is_none() => {
                self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
            }
            Capability::Keyboard if self.keyboard.is_none() => {
                self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
            }
            _ => {}
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        match cap {
            Capability::Pointer => { if let Some(p) = self.pointer.take() { p.release(); } }
            Capability::Keyboard => { if let Some(k) = self.keyboard.take() { k.release(); } }
            _ => {}
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for State {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for e in events {
            let at = (e.position.0 as f32, e.position.1 as f32);
            match e.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.tick.pointer = Some(at);
                }
                PointerEventKind::Press { .. } => {
                    self.tick.pointer = Some(at);
                    self.tick.press = true;
                }
                PointerEventKind::Release { .. } => {
                    self.tick.pointer = Some(at);
                    self.tick.release = true;
                }
                PointerEventKind::Axis { vertical, .. } => {
                    self.tick.scroll += vertical.absolute as f32;
                }
                PointerEventKind::Leave { .. } => {}
            }
        }
    }
}

impl KeyboardHandler for State {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, ev: KeyEvent) {
        self.tick.keys.push(ev.keysym);
        self.tick.ctrl = self.modifiers.ctrl;
        self.tick.shift = self.modifiers.shift;
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    /// Held keys repeat — an editor without key repeat cannot nudge a point
    /// ten pixels without ten separate presses.
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, ev: KeyEvent) {
        self.tick.keys.push(ev.keysym);
        self.tick.ctrl = self.modifiers.ctrl;
        self.tick.shift = self.modifiers.shift;
    }
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, m: Modifiers, _: RawModifiers, _: u32) {
        self.modifiers = m;
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(State);
delegate_dispatch2!(State);
