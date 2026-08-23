//! The real compositor shell: `wisp-shell` behind the [`Shell`] seam.
//!
//! `Headless` is what CI and `--mock` run. This is what the operator sees.
//! It owns her position, because the shell owns the outputs and the terrain —
//! the rig asks where she is rather than telling it.

use std::time::Instant;

use std::collections::HashMap;

use wisp_proto::{
    sense::{Observation, SenseId},
    Tier, Utterance,
};
use wisp_rig::{
    physics::{step, BodyState, Forces, PhysicsParams, Surface},
    ContourOptions, Polygon, Rect, RigFrame, Vec2,
};
use wisp_shell::{bubble, palette, tell, Keysym, ShellConfig, WispShell};

use crate::shell::{FrameCtx, Shell};

pub struct LayerShellHost {
    shell: WispShell,
    body: BodyState,
    /// The floor of the screen — her fallback when she is over nothing.
    ground: Surface,
    /// The operator's real windows, by id, as (top edge y, left x, right x).
    /// F68: their top edges are the ledges she stands on.
    windows: HashMap<u64, (f32, f32, f32)>,
    /// Rebuilt from `windows` only when they change, not every frame.
    terrain: Vec<Surface>,
    terrain_dirty: bool,
    /// Where she was when the ledge set was last chosen. The cap picks the
    /// nearest windows, so the set goes stale as she travels — not just when
    /// the windows themselves change.
    terrain_at: Vec2,
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
    /// F41. Opened by a click on her — a click, not a drag.
    palette: palette::Palette,
    /// Where the pointer went down, to tell a click from the start of a throw.
    press_at: Option<Vec2>,
    /// Lines the operator submitted, drained by the app each frame.
    typed: Vec<String>,
    /// Process-start epoch for animation phases; a per-frame Instant would
    /// make every phase permanently zero.
    started: Instant,
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
            windows: HashMap::new(),
            terrain: Vec::new(),
            terrain_dirty: true,
            terrain_at: start,
            bounds: Rect {
                min: Vec2 { x: 0.0, y: 0.0 },
                max: Vec2 { x: ow, y: oh },
            },
            pending_say: None,
            showing: None,
            tells: Vec::new(),
            palette: palette::Palette::default(),
            press_at: None,
            typed: Vec::new(),
            started: Instant::now(),
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

    /// Turn the window rectangles into ledges.
    ///
    /// Only the TOP edge of a window is standable, and the rig's surfaces are
    /// one-way, so she lands on a title bar and passes up through it from
    /// below — which is what you want from a creature climbing your desktop.
    ///
    /// Capped and sorted by nearness: a busy desktop can have dozens of
    /// windows, and the physics step is O(surfaces) on every frame at every
    /// tier. Her floor is always included, so a cap can never drop her out of
    /// the world.
    fn rebuild_terrain(&mut self) {
        // Re-pick when the windows changed, OR when she has travelled far
        // enough that "nearest" means something different. Without the second
        // test she would sail past the cap and fall through a window she was
        // heading straight for.
        const RECHOOSE_PX: f32 = 64.0;
        let moved = (self.body.pos.x - self.terrain_at.x).abs()
            + (self.body.pos.y - self.terrain_at.y).abs();
        if !self.terrain_dirty && moved < RECHOOSE_PX {
            return;
        }
        self.terrain_dirty = false;
        self.terrain_at = self.body.pos;
        const MAX_LEDGES: usize = 24;

        self.terrain.clear();
        self.terrain.push(self.ground);
        let here = self.body.pos;
        let mut near: Vec<(f32, Surface)> = self
            .windows
            .iter()
            .map(|(id, (y, x0, x1))| {
                let dx = if here.x < *x0 {
                    x0 - here.x
                } else if here.x > *x1 {
                    here.x - x1
                } else {
                    0.0
                };
                let d = dx * dx + (here.y - y) * (here.y - y);
                (d, Surface { id: *id, y: *y, x0: *x0, x1: *x1 })
            })
            .collect();
        near.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        self.terrain.extend(near.into_iter().take(MAX_LEDGES).map(|(_, s)| s));
        tracing::debug!(
            windows = self.windows.len(),
            ledges = self.terrain.len(),
            "terrain rebuilt"
        );
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
            self.press_at = self.cursor;
        }
        if tick.released {
            // A press and release that never travelled is a click: summon her.
            // Anything that moved was a drag, and stays a throw.
            if let (Some(a), Some(b)) = (self.press_at.take(), self.cursor) {
                let dist = (a.x - b.x).abs() + (a.y - b.y).abs();
                if dist < 6.0 && !self.palette.is_open() {
                    self.palette.open();
                    self.shell.set_keyboard_interactive(true);
                }
            }
            self.grab = None;
        }

        // Keys only arrive while the compositor has granted us the keyboard,
        // which only happens while the palette asked for it.
        for (sym, utf8, ctrl) in &tick.keys {
            if let Some(k) = decode_key(*sym, utf8.as_deref(), *ctrl) {
                match self.palette.key(k) {
                    palette::Outcome::Submitted(line) => {
                        self.typed.push(line);
                        self.shell.set_keyboard_interactive(false);
                    }
                    palette::Outcome::Dismissed => {
                        self.shell.set_keyboard_interactive(false);
                    }
                    _ => {}
                }
            }
        }
        // The compositor taking the keyboard back (the operator clicked
        // elsewhere) dismisses an open palette rather than leaving a bar that
        // cannot type.
        if tick.keyboard_left && self.palette.is_open() {
            self.palette.close();
            self.shell.set_keyboard_interactive(false);
        }
        self.rebuild_terrain();
        let forces = Forces {
            params: self.params,
            surfaces: &self.terrain,
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
        let phase = (self.started.elapsed().as_secs_f32() * 0.8).fract();
        let caret_phase = (self.started.elapsed().as_secs_f32() * 1.2).fract();
        let pal = &self.palette;
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
            pal.paint(at, size, bounds, caret_phase, painter, engine, scene);
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

    fn observed(&mut self, obs: &Observation) {
        let Observation::Window { id, x, y, w, h, gone } = obs else { return };
        if *gone {
            if self.windows.remove(id).is_some() {
                self.terrain_dirty = true;
            }
            return;
        }
        // A zero-sized window is not a ledge; a shaded or rolled-up one reports
        // as such and would otherwise become an invisible tightrope.
        if *w == 0 || *h == 0 {
            if self.windows.remove(id).is_some() {
                self.terrain_dirty = true;
            }
            return;
        }
        let ledge = (*y as f32, *x as f32, (*x + *w as i32) as f32);
        if self.windows.insert(*id, ledge) != Some(ledge) {
            self.terrain_dirty = true;
        }
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

    fn take_input(&mut self) -> Vec<String> {
        std::mem::take(&mut self.typed)
    }

    fn shutdown(&mut self) {
        self.closed = true;
    }
}

/// Compositor keys → palette keys. The palette knows nothing about keysyms,
/// and everything it does not recognise falls through untouched.
fn decode_key(sym: Keysym, utf8: Option<&str>, ctrl: bool) -> Option<palette::Key> {
    Some(match sym {
        Keysym::Return | Keysym::KP_Enter => palette::Key::Submit,
        Keysym::Escape => palette::Key::Dismiss,
        Keysym::BackSpace if ctrl => palette::Key::DeleteWord,
        Keysym::BackSpace => palette::Key::Backspace,
        Keysym::w | Keysym::W if ctrl => palette::Key::DeleteWord,
        Keysym::Left => palette::Key::Left,
        Keysym::Right => palette::Key::Right,
        Keysym::Home => palette::Key::Home,
        Keysym::End => palette::Key::End,
        _ => {
            let t = utf8?;
            if t.is_empty() || ctrl {
                return None;
            }
            palette::Key::Char(t.to_string())
        }
    })
}
