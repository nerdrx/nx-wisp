//! The real compositor shell: `wisp-shell` behind the [`Shell`] seam.
//!
//! `Headless` is what CI and `--mock` run. This is what the operator sees.
//! It owns her position, because the shell owns the outputs and the terrain —
//! the rig asks where she is rather than telling it.

use std::time::Instant;

use wisp_proto::{sense::SenseId, Tier, Utterance};
use wisp_rig::{
    physics::{step, BodyState, Forces, PhysicsParams, Surface},
    ContourOptions, Polygon, Rect, RigFrame, Vec2,
};
use wisp_shell::{ShellConfig, WispShell};

use crate::shell::{FrameCtx, Shell};

pub struct LayerShellHost {
    shell: WispShell,
    body: BodyState,
    ground: Surface,
    bounds: Rect,
    cursor: Option<Vec2>,
    grab: Option<Vec2>,
    params: PhysicsParams,
    last: Instant,
    closed: bool,
}

impl LayerShellHost {
    pub fn new(size_px: f32) -> Result<LayerShellHost, wisp_shell::ShellError> {
        let cfg = ShellConfig { size_px, padding_px: (size_px * 0.4).max(48.0) };
        let mut shell = WispShell::new(&cfg)?;

        // The compositor must configure us before anything can be drawn.
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        while !shell.is_configured() && Instant::now() < deadline {
            shell.block();
        }

        let (w, h) = shell.surface_size();
        Ok(LayerShellHost {
            body: BodyState {
                pos: Vec2 { x: w as f32 * 0.5, y: h as f32 * 0.35 },
                ..Default::default()
            },
            ground: Surface { id: 1, y: h as f32 - 12.0, x0: 0.0, x1: w as f32 },
            bounds: Rect {
                min: Vec2 { x: 0.0, y: 0.0 },
                max: Vec2 { x: w as f32, y: h as f32 },
            },
            shell,
            cursor: None,
            grab: None,
            params: PhysicsParams::default(),
            last: Instant::now(),
            closed: false,
        })
    }

    pub fn closed(&self) -> bool {
        self.closed
    }

    fn pump(&mut self, dt: f32) {
        let tick = self.shell.pump();
        if tick.closed {
            self.closed = true;
        }
        if let Some(p) = tick.pointer {
            self.cursor = Some(p);
            if self.grab.is_some() {
                self.grab = Some(p);
            }
        }
        if tick.grabbed {
            self.grab = self.cursor;
        }
        if tick.released {
            self.grab = None;
        }
        let forces = Forces {
            params: self.params,
            surfaces: std::slice::from_ref(&self.ground),
            bounds: Some(self.bounds),
            grab: self.grab,
            wind: Vec2::ZERO,
        };
        self.body = step(&self.body, dt, &forces).state;
    }
}

impl Shell for LayerShellHost {
    fn present(&mut self, frame: &RigFrame, input_region: &Polygon, ctx: &FrameCtx) {
        let now = Instant::now();
        // Prefer the loop's dt, but never trust a stalled frame.
        let dt = if ctx.dt.is_finite() && ctx.dt > 0.0 {
            ctx.dt.min(0.05)
        } else {
            (now - self.last).as_secs_f32().min(0.05)
        };
        self.last = now;
        self.pump(dt);
        if self.closed {
            return;
        }
        self.shell.draw(frame, input_region);
    }

    fn set_tier(&mut self, tier: Tier) {
        self.shell.set_tier(tier);
    }

    fn cursor(&self) -> Option<(f32, f32)> {
        self.cursor.map(|c| (c.x, c.y))
    }

    fn anchor(&self) -> Option<(f32, f32)> {
        Some((self.body.pos.x, self.body.pos.y))
    }

    fn contour_options(&self) -> ContourOptions {
        ContourOptions::default()
    }

    fn say(&mut self, _u: &Utterance) {
        // Speech bubbles are the next milestone; until then she is silent on
        // screen rather than pretending otherwise. The flight recorder still
        // has every word.
    }

    fn invasive_tell(&mut self, _sense: SenseId, _active: bool) {
        // SPEC §0.3's visible tell needs a rig channel; the recorder logs it
        // meanwhile. Not shipping a fake tell.
    }

    fn shutdown(&mut self) {
        self.closed = true;
    }
}
