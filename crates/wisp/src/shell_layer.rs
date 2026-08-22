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
use wisp_shell::{bubble, tell, ShellConfig, WispShell};

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
    /// A queued utterance, built into a laid-out bubble on the next frame —
    /// laying it out needs the text engine, which lives inside the shell.
    pending_say: Option<(String, wisp_proto::Urgency)>,
    /// The bubble currently on screen, and when it went up.
    showing: Option<(bubble::Layout, Instant)>,
    /// Invasive senses that are live right now (SPEC §0.3's visible tell).
    tells: Vec<(wisp_proto::sense::SenseId, bool)>,
    /// Output-space position the NEXT frame was built for.
    pending: Vec2,
    /// That same position expressed within the surface — what `anchor` returns.
    local: Vec2,
}

impl LayerShellHost {
    pub fn new(size_px: f32) -> Result<LayerShellHost, wisp_shell::ShellError> {
        let cfg = ShellConfig {
            size_px,
            padding_px: (size_px * 0.4).max(48.0),
            ..ShellConfig::default()
        };
        let mut shell = WispShell::new(&cfg)?;

        // The compositor must configure us before anything can be drawn.
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        while !shell.is_configured() && Instant::now() < deadline {
            shell.block();
        }

        // She roams the whole OUTPUT; the surface is a small window that
        // follows her. Until the compositor announces an output we fall back to
        // the surface itself, so she is contained rather than lost.
        let (sw, sh) = shell.surface_size();
        let (ow, oh) = shell
            .output_size()
            .map(|(w, h)| (w as f32, h as f32))
            .unwrap_or((sw as f32, sh as f32));
        let start = Vec2 { x: ow * 0.5, y: oh * 0.35 };
        let local = shell.local_for(start);
        Ok(LayerShellHost {
            body: BodyState { pos: start, ..Default::default() },
            // The bottom of the screen is her floor until the terrain feed is
            // wired up and she can stand on your windows.
            ground: Surface { id: 1, y: oh - 12.0, x0: 0.0, x1: ow },
            bounds: Rect {
                min: Vec2 { x: 0.0, y: 0.0 },
                max: Vec2 { x: ow, y: oh },
            },
            pending_say: None,
            showing: None,
            tells: Vec::new(),
            pending: start,
            local,
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
        // Pointer arrives in SURFACE coordinates; her world is output space.
        let (ox, oy) = self.shell.origin_for(self.pending);
        let to_output = |p: Vec2| Vec2 { x: p.x + ox as f32, y: p.y + oy as f32 };
        if let Some(p) = tick.pointer.map(to_output) {
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

        // Park the surface to match the position this frame was actually built
        // for. Doing it in this order means the box and the creature never
        // disagree, even by one frame.
        self.shell.follow(self.pending);

        let (sw, sh) = self.shell.surface_size();
        let bounds = wisp_paint::Rect::from_size(sw as f32, sh as f32);
        let at = wisp_paint::Point { x: self.local.x, y: self.local.y };
        let size = ctx.size_px;
        let tier = ctx.tier;
        let pending = self.pending_say.take();
        let showing = &mut self.showing;
        let tells = &self.tells;
        // Pulse the tell off wall-clock so it is visibly alive; the shell owns
        // no clock, so the phase is handed in.
        let phase = (now.elapsed().as_secs_f32() * 0.0).fract();
        let elapsed_ms = showing.as_ref().map(|(_, t)| t.elapsed().as_millis() as u64);

        self.shell.draw_with(frame, input_region, |painter, engine, scene| {
            let mut sink = bubble::Live::new(painter, engine);
            if let Some((text, urgency)) = pending {
                *showing = Some((
                    bubble::Layout::new(&text, urgency, at, size, bounds, &mut sink),
                    Instant::now(),
                ));
            }
            if let Some((layout, _)) = showing.as_ref() {
                let ms = elapsed_ms.unwrap_or(0);
                if !layout.should_dismiss(ms) {
                    layout.paint(tier, layout.reveal_at(ms), &mut sink, scene);
                }
            }
            for (sense, active) in tells {
                tell::build(*sense, *active, at, size, phase, scene);
            }
        });

        // Retire a finished bubble so it does not sit there forever.
        if let (Some((layout, t)), true) = (self.showing.as_ref(), true) {
            if layout.should_dismiss(t.elapsed().as_millis() as u64) {
                self.showing = None;
            }
        }

        // Then advance the world, and work out where she will be drawn next.
        self.pump(dt);
        if self.closed {
            return;
        }
        self.pending = self.body.pos;
        self.local = self.shell.local_for(self.body.pos);
    }

    fn set_tier(&mut self, tier: Tier) {
        self.shell.set_tier(tier);
    }

    fn cursor(&self) -> Option<(f32, f32)> {
        // Kept in output space for the physics grab, but the rig poses in
        // surface space, so convert back or her look-at aims off-screen.
        let (ox, oy) = self.shell.origin_for(self.pending);
        self.cursor.map(|c| (c.x - ox as f32, c.y - oy as f32))
    }

    fn anchor(&self) -> Option<(f32, f32)> {
        // In-surface coordinates: the rig draws into the surface, not the screen.
        Some((self.local.x, self.local.y))
    }

    fn contour_options(&self) -> ContourOptions {
        ContourOptions::default()
    }

    fn say(&mut self, u: &Utterance) {
        // Queued, not laid out: wrapping needs shaped text widths, and the
        // text engine lives inside the shell. Built on the next frame.
        self.pending_say = Some((u.text.clone(), u.urgency));
    }

    fn invasive_tell(&mut self, sense: SenseId, active: bool) {
        self.tells.retain(|(s, _)| *s != sense);
        if active {
            self.tells.push((sense, true));
        }
    }

    fn shutdown(&mut self) {
        self.closed = true;
    }
}
