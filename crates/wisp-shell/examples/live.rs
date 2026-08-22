//! First light: put her on the desktop.
//!
//!   cargo run -p wisp-shell --example live -- [seconds]
//!
//! Gravity, drag-and-throw, look-at, and a click-through input region cut to
//! her silhouette. No AI, no senses — just the creature.

use std::time::{Duration, Instant};

use wisp_rig::{
    contour::{trace_frame, ContourOptions},
    physics::{step, BodyState, Forces, PhysicsParams, Surface},
    skin::default_skin,
    Rect, Rig, RigInput, Vec2,
};
use wisp_shell::{ShellConfig, WispShell};

fn main() {
    let secs: f32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(20.0);

    let cfg = ShellConfig { size_px: 150.0, padding_px: 60.0 };
    let mut shell = match WispShell::new(&cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let mut rig = Rig::new(default_skin().expect("default skin"));

    let deadline = Instant::now() + Duration::from_secs(3);
    while !shell.is_configured() && Instant::now() < deadline {
        shell.block();
    }
    if !shell.is_configured() {
        eprintln!("the compositor never configured the layer surface");
        std::process::exit(3);
    }
    let (w, h) = shell.surface_size();
    println!("layer surface configured: {w}x{h}");

    let ground = Surface { id: 1, y: h as f32 - 12.0, x0: 0.0, x1: w as f32 };
    let bounds = Rect {
        min: Vec2 { x: 0.0, y: 0.0 },
        max: Vec2 { x: w as f32, y: h as f32 },
    };
    let mut body = BodyState {
        pos: Vec2 { x: w as f32 * 0.5, y: h as f32 * 0.35 },
        ..Default::default()
    };

    let params = PhysicsParams::default();
    let mut cursor: Option<Vec2> = None;
    let mut grab: Option<Vec2> = None;
    let mut last = Instant::now();
    let end = Instant::now() + Duration::from_secs_f32(secs);
    let mut frames = 0u32;

    while Instant::now() < end {
        let tick = shell.pump();
        if tick.closed {
            break;
        }
        if let Some(p) = tick.pointer {
            cursor = Some(p);
            if grab.is_some() {
                grab = Some(p);
            }
        }
        if tick.grabbed {
            grab = cursor;
        }
        if tick.released {
            grab = None;
        }

        let now = Instant::now();
        let dt = (now - last).as_secs_f32().min(0.05);
        last = now;

        let forces = Forces {
            params,
            surfaces: std::slice::from_ref(&ground),
            bounds: Some(bounds),
            grab,
            wind: Vec2::ZERO,
        };
        body = step(&body, dt, &forces).state;

        rig.update(
            dt,
            &RigInput {
                size_px: cfg.size_px,
                anchor: body.pos,
                velocity: body.vel,
                cursor,
                attention: cursor,
                grabbed: body.grabbed,
                on_ground: body.on_ground,
            },
        );

        let frame = rig.frame();
        let outline = trace_frame(frame, ContourOptions::default());
        shell.draw(frame, &outline);
        frames += 1;

        std::thread::sleep(Duration::from_millis(16));
    }
    println!("{frames} frames — she was on your desktop");
}
