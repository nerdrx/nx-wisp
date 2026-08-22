//! Gravity, throwing and landing (F5, F72), as one pure step function.
//!
//! [`step`] takes an immutable state and an environment and returns a new
//! state plus at most one notable event. Nothing is mutated in place, there is
//! no interior state, no clock and no randomness — so a throw is reproducible
//! frame for frame in a test, and the same inputs give the same trajectory on
//! any machine.
//!
//! Coordinates are surface pixels with **y down**, matching Wayland: a larger
//! `y` is further towards the bottom of the screen, and gravity is positive.
//!
//! Terrain is a list of one-way [`Surface`] spans — the top edges of the
//! operator's actual windows (F4/F68), plus the screen floor. They are one-way
//! deliberately: she lands on a title bar coming down and passes up through it
//! going up, which is what makes "walks along the top of your IDE" work
//! without any notion of solid volumes.

use crate::math::{clamp, Rect, Vec2};

/// A horizontal ledge she can stand on. `id` is the caller's — normally a KWin
/// window id, so a landing event can say *what* she landed on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Surface {
    pub id: u64,
    /// The surface's y in screen pixels.
    pub y: f32,
    pub x0: f32,
    pub x1: f32,
}

impl Surface {
    pub fn spans(&self, x: f32) -> bool {
        x >= self.x0 && x <= self.x1
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhysicsParams {
    /// Downwards acceleration, px/s².
    pub gravity: f32,
    /// Air resistance as an exponential decay rate, 1/s.
    pub drag: f32,
    /// Bounce fraction kept on a landing, `0..=1`.
    pub restitution: f32,
    /// Horizontal velocity kept per second while on the ground, `0..=1`.
    pub friction: f32,
    /// Speed cap, px/s. Stops an absurd fling from putting her three screens
    /// away in one frame.
    pub max_speed: f32,
    /// Below this vertical speed a bounce becomes a rest.
    pub rest_speed: f32,
    /// Impacts at or above this speed are "hard" — she gets a recovery beat
    /// and a bigger squash.
    pub hard_landing_speed: f32,
    /// Seconds of recovery after a hard landing.
    pub recovery_time: f32,
    /// How much of the pointer's motion becomes velocity while grabbed,
    /// `0..=1`. Below 1 smooths a jittery mouse into a plausible throw.
    pub grab_transfer: f32,
}

impl Default for PhysicsParams {
    fn default() -> Self {
        PhysicsParams {
            gravity: 1800.0,
            drag: 0.6,
            restitution: 0.34,
            friction: 0.02,
            max_speed: 4000.0,
            rest_speed: 45.0,
            hard_landing_speed: 900.0,
            recovery_time: 0.45,
            grab_transfer: 0.65,
        }
    }
}

/// Everything acting on her this frame. This is the "forces" argument of
/// `step(state, dt, forces)`; it also carries the terrain, because a landing
/// is a force too.
#[derive(Debug, Clone, Copy, Default)]
pub struct Forces<'a> {
    pub params: PhysicsParams,
    /// One-way ledges, in any order.
    pub surfaces: &'a [Surface],
    /// Screen or output bounds. She bounces off the sides and cannot leave.
    pub bounds: Option<Rect>,
    /// Where the pointer is, while she is being dragged. `Some` means grabbed.
    pub grab: Option<Vec2>,
    /// Ambient push — a nudge from a window that moved into her (F4), or a
    /// breeze.
    pub wind: Vec2,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyState {
    pub pos: Vec2,
    pub vel: Vec2,
    /// True while the pointer holds her.
    pub grabbed: bool,
    /// True while resting on a surface.
    pub on_ground: bool,
    /// Which surface she is resting on, if any.
    pub ground: Option<u64>,
    /// Seconds since she last left the ground. Drives the "flail" clip.
    pub airborne: f32,
    /// Counts down after a hard landing. While > 0 she is recovering (F72).
    pub recovering: f32,
}

impl Default for BodyState {
    fn default() -> Self {
        BodyState {
            pos: Vec2::ZERO,
            vel: Vec2::ZERO,
            grabbed: false,
            on_ground: false,
            ground: None,
            airborne: 0.0,
            recovering: 0.0,
        }
    }
}

impl BodyState {
    pub fn at(pos: Vec2) -> BodyState {
        BodyState { pos, ..Default::default() }
    }

    /// Is she close enough to still that the rig may stop asking for frames?
    pub fn asleep(&self, eps: f32) -> bool {
        !self.grabbed && self.on_ground && self.vel.len() < eps && self.recovering <= 0.0
    }
}

/// Something worth reacting to. At most one per step — the interesting ones are
/// mutually exclusive in practice, and returning an `Option` keeps `step`
/// allocation-free.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PhysicsEvent {
    /// She touched down. `speed` is the impact speed, `hard` whether it earns a
    /// recovery beat.
    Landed { speed: f32, hard: bool, surface: Option<u64> },
    /// She left the ground under her own power or someone's throw.
    Launched { speed: f32 },
    /// The ground moved or closed out from under her (F4: the window closed).
    Dropped,
    /// The pointer let go. `speed` is what she is carrying away.
    Released { speed: f32 },
    /// She hit the left or right edge of the output.
    HitWall { speed: f32 },
}

pub struct StepResult {
    pub state: BodyState,
    pub event: Option<PhysicsEvent>,
}

/// Advance the body by `dt` seconds. Pure: `state` is not modified.
///
/// Order of operations, which matters for reproducibility:
/// grab → integrate → sweep against terrain → walls → timers.
pub fn step(state: &BodyState, dt: f32, f: &Forces<'_>) -> StepResult {
    let mut s = *state;
    let mut event = None;
    if !dt.is_finite() || dt <= 0.0 {
        return StepResult { state: s, event };
    }
    // Same stall guard as the springs: a 3-second frame is not motion.
    let dt = dt.min(crate::ease::MAX_STEP);
    let p = f.params;

    // --- grabbed: she follows the pointer, and the pointer writes velocity ---
    if let Some(target) = f.grab {
        let implied = (target - s.pos) / dt;
        let transfer = clamp(p.grab_transfer, 0.0, 1.0);
        s.vel = clamp_speed(s.vel.lerp(implied, transfer), p.max_speed);
        s.pos = target;
        if !s.grabbed {
            event = Some(PhysicsEvent::Launched { speed: 0.0 });
        }
        s.grabbed = true;
        s.on_ground = false;
        s.ground = None;
        s.airborne += dt;
        s.recovering = (s.recovering - dt).max(0.0);
        return StepResult { state: s, event };
    }
    if s.grabbed {
        s.grabbed = false;
        event = Some(PhysicsEvent::Released { speed: s.vel.len() });
    }

    let was_on_ground = s.on_ground;
    let prev = s.pos;

    // --- support check: is the ledge she was on still under her? ---
    if s.on_ground {
        let still_supported = s
            .ground
            .and_then(|id| f.surfaces.iter().find(|sf| sf.id == id))
            .is_some_and(|sf| sf.spans(s.pos.x) && (sf.y - s.pos.y).abs() < GROUND_SNAP);
        if !still_supported {
            s.on_ground = false;
            s.ground = None;
            if event.is_none() {
                event = Some(PhysicsEvent::Dropped);
            }
        }
    }

    // --- integrate ---
    let mut accel = f.wind;
    if !s.on_ground {
        accel.y += p.gravity;
    }
    s.vel += accel * dt;
    if s.on_ground {
        // Ground friction as an exponential decay, so it is frame-rate
        // independent.
        s.vel.x *= clamp(p.friction, 0.0, 1.0).powf(dt);
        s.vel.y = 0.0;
    } else {
        let decay = (-p.drag.max(0.0) * dt).exp();
        s.vel = s.vel * decay;
    }
    s.vel = clamp_speed(s.vel, p.max_speed);
    s.pos += s.vel * dt;

    // --- one-way ledges, swept so a fast fall cannot tunnel through ---
    if !s.on_ground && s.vel.y > 0.0 {
        let mut hit: Option<&Surface> = None;
        for sf in f.surfaces {
            if !sf.spans(s.pos.x) {
                continue;
            }
            // Crossed the plane this frame, coming down. The highest ledge
            // wins when two overlap.
            if prev.y <= sf.y + 1e-3 && s.pos.y >= sf.y && hit.is_none_or(|h| sf.y < h.y) {
                hit = Some(sf);
            }
        }
        if let Some(sf) = hit {
            let impact = s.vel.y;
            s.pos.y = sf.y;
            if impact.abs() < p.rest_speed {
                s.vel.y = 0.0;
                s.on_ground = true;
                s.ground = Some(sf.id);
            } else {
                s.vel.y = -impact * clamp(p.restitution, 0.0, 1.0);
                if s.vel.y.abs() < p.rest_speed {
                    s.vel.y = 0.0;
                    s.on_ground = true;
                    s.ground = Some(sf.id);
                }
            }
            let hard = impact >= p.hard_landing_speed;
            if hard {
                s.recovering = p.recovery_time;
            }
            event = Some(PhysicsEvent::Landed { speed: impact, hard, surface: Some(sf.id) });
        }
    }

    // --- walls and ceiling ---
    if let Some(b) = f.bounds {
        if s.pos.x < b.min.x {
            s.pos.x = b.min.x;
            if s.vel.x < 0.0 {
                let sp = s.vel.x.abs();
                s.vel.x = -s.vel.x * clamp(p.restitution, 0.0, 1.0);
                if event.is_none() {
                    event = Some(PhysicsEvent::HitWall { speed: sp });
                }
            }
        } else if s.pos.x > b.max.x {
            s.pos.x = b.max.x;
            if s.vel.x > 0.0 {
                let sp = s.vel.x;
                s.vel.x = -s.vel.x * clamp(p.restitution, 0.0, 1.0);
                if event.is_none() {
                    event = Some(PhysicsEvent::HitWall { speed: sp });
                }
            }
        }
        if s.pos.y < b.min.y {
            s.pos.y = b.min.y;
            s.vel.y = s.vel.y.max(0.0);
        }
        if s.pos.y > b.max.y {
            let impact = s.vel.y;
            s.pos.y = b.max.y;
            if impact > 0.0 {
                s.vel.y = if impact.abs() < p.rest_speed {
                    0.0
                } else {
                    -impact * clamp(p.restitution, 0.0, 1.0)
                };
                if s.vel.y.abs() < p.rest_speed {
                    s.vel.y = 0.0;
                    s.on_ground = true;
                    s.ground = None;
                }
                let hard = impact >= p.hard_landing_speed;
                if hard {
                    s.recovering = p.recovery_time;
                }
                event = Some(PhysicsEvent::Landed { speed: impact, hard, surface: None });
            }
        }
    }

    // --- timers ---
    if s.on_ground {
        s.airborne = 0.0;
    } else {
        s.airborne += dt;
        if was_on_ground && event.is_none() && s.vel.y < 0.0 {
            event = Some(PhysicsEvent::Launched { speed: s.vel.len() });
        }
    }
    s.recovering = (s.recovering - dt).max(0.0);

    if !s.pos.is_finite() || !s.vel.is_finite() {
        s.pos = state.pos;
        s.vel = Vec2::ZERO;
    }
    StepResult { state: s, event }
}

/// How close to a ledge counts as standing on it.
const GROUND_SNAP: f32 = 2.0;

fn clamp_speed(v: Vec2, max: f32) -> Vec2 {
    let l = v.len();
    if !l.is_finite() {
        return Vec2::ZERO;
    }
    if max > 0.0 && l > max {
        v * (max / l)
    } else {
        v
    }
}

/// Give her a push. Convenience for a hop, a shove from a window, or the
/// summon hotkey pulling her towards the cursor.
pub fn impulse(state: &BodyState, v: Vec2, p: PhysicsParams) -> BodyState {
    let mut s = *state;
    s.vel = clamp_speed(s.vel + v, p.max_speed);
    if s.vel.y < 0.0 {
        s.on_ground = false;
        s.ground = None;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floor() -> [Surface; 1] {
        [Surface { id: 1, y: 500.0, x0: -10_000.0, x1: 10_000.0 }]
    }

    fn env<'a>(surfaces: &'a [Surface]) -> Forces<'a> {
        Forces {
            params: PhysicsParams::default(),
            surfaces,
            bounds: None,
            grab: None,
            wind: Vec2::ZERO,
        }
    }

    fn run(mut s: BodyState, f: &Forces<'_>, frames: usize) -> (BodyState, Vec<PhysicsEvent>) {
        let mut evs = Vec::new();
        for _ in 0..frames {
            let r = step(&s, 1.0 / 120.0, f);
            s = r.state;
            if let Some(e) = r.event {
                evs.push(e);
            }
        }
        (s, evs)
    }

    #[test]
    fn step_is_pure() {
        let s = BodyState::at(Vec2::new(0.0, 0.0));
        let sf = floor();
        let before = s;
        let _ = step(&s, 1.0 / 60.0, &env(&sf));
        assert_eq!(s, before, "step mutated its input");
    }

    #[test]
    fn a_dropped_body_accelerates_downwards() {
        let sf = floor();
        let f = env(&sf);
        let s = BodyState::at(Vec2::new(0.0, 0.0));
        let a = step(&s, 1.0 / 60.0, &f).state;
        let b = step(&a, 1.0 / 60.0, &f).state;
        assert!(a.vel.y > 0.0);
        assert!(b.vel.y > a.vel.y, "not accelerating");
        assert!(b.pos.y > a.pos.y);
    }

    #[test]
    fn she_lands_on_a_surface_and_stops() {
        let sf = floor();
        let (s, evs) = run(BodyState::at(Vec2::new(0.0, 0.0)), &env(&sf), 400);
        assert!(s.on_ground, "never landed: {s:?}");
        assert_eq!(s.ground, Some(1));
        assert!((s.pos.y - 500.0).abs() < 1.0, "settled at {}", s.pos.y);
        assert!(s.vel.len() < 1.0, "still moving: {:?}", s.vel);
        assert!(
            evs.iter().any(|e| matches!(e, PhysicsEvent::Landed { .. })),
            "no landing event"
        );
    }

    #[test]
    fn a_fast_fall_does_not_tunnel_through_a_ledge() {
        let sf = floor();
        let mut f = env(&sf);
        f.params.max_speed = 100_000.0;
        let s = BodyState {
            pos: Vec2::new(0.0, 0.0),
            vel: Vec2::new(0.0, 40_000.0),
            ..Default::default()
        };
        let r = step(&s, 1.0 / 60.0, &f);
        assert!(
            r.state.pos.y <= 500.0 + 1e-3,
            "tunnelled to {}",
            r.state.pos.y
        );
        assert!(matches!(r.event, Some(PhysicsEvent::Landed { .. })));
    }

    #[test]
    fn a_hard_landing_sets_the_recovery_timer() {
        let sf = floor();
        let f = env(&sf);
        let s = BodyState {
            pos: Vec2::new(0.0, 499.0),
            vel: Vec2::new(0.0, 2000.0),
            ..Default::default()
        };
        let r = step(&s, 1.0 / 120.0, &f);
        match r.event {
            Some(PhysicsEvent::Landed { hard, speed, .. }) => {
                assert!(hard, "2000 px/s should be a hard landing");
                assert!(speed > 1000.0);
            }
            other => panic!("expected a landing, got {other:?}"),
        }
        assert!(r.state.recovering > 0.0);
    }

    #[test]
    fn a_gentle_landing_is_not_hard_and_needs_no_recovery() {
        let sf = floor();
        let f = env(&sf);
        let s = BodyState {
            pos: Vec2::new(0.0, 498.0),
            vel: Vec2::new(0.0, 30.0),
            ..Default::default()
        };
        let (after, evs) = run(s, &f, 20);
        let landing = evs
            .iter()
            .find(|e| matches!(e, PhysicsEvent::Landed { .. }))
            .copied();
        assert!(
            matches!(landing, Some(PhysicsEvent::Landed { hard: false, .. })),
            "expected a soft landing, got {landing:?}"
        );
        assert_eq!(after.recovering, 0.0);
        assert!(after.on_ground);
    }

    #[test]
    fn she_bounces_before_settling() {
        let sf = floor();
        let mut f = env(&sf);
        f.params.restitution = 0.6;
        let s = BodyState {
            pos: Vec2::new(0.0, 100.0),
            vel: Vec2::new(0.0, 0.0),
            ..Default::default()
        };
        let (_, evs) = run(s, &f, 600);
        let landings = evs
            .iter()
            .filter(|e| matches!(e, PhysicsEvent::Landed { .. }))
            .count();
        assert!(landings >= 2, "expected a bounce, got {landings} landing(s)");
    }

    #[test]
    fn a_one_way_ledge_is_passable_from_below() {
        let sf = floor();
        let f = env(&sf);
        let s = BodyState {
            pos: Vec2::new(0.0, 600.0),
            vel: Vec2::new(0.0, -1500.0),
            ..Default::default()
        };
        let (after, _) = run(s, &f, 20);
        assert!(after.pos.y < 500.0, "blocked from below at {}", after.pos.y);
        assert!(!after.on_ground);
    }

    #[test]
    fn she_only_lands_on_the_part_of_a_window_that_is_there() {
        let surfaces = [Surface { id: 7, y: 300.0, x0: 100.0, x1: 200.0 }];
        let f = env(&surfaces);
        // Falling next to the window, not over it.
        let (miss, _) = run(BodyState::at(Vec2::new(50.0, 0.0)), &f, 200);
        assert!(!miss.on_ground, "landed on thin air at x = 50");
        assert!(miss.pos.y > 300.0);
        // Falling onto it.
        let (hit, _) = run(BodyState::at(Vec2::new(150.0, 0.0)), &f, 200);
        assert!(hit.on_ground);
        assert_eq!(hit.ground, Some(7));
    }

    #[test]
    fn the_highest_ledge_wins_when_two_overlap() {
        let surfaces = [
            Surface { id: 1, y: 400.0, x0: 0.0, x1: 500.0 },
            Surface { id: 2, y: 300.0, x0: 0.0, x1: 500.0 },
        ];
        let f = env(&surfaces);
        let (s, _) = run(BodyState::at(Vec2::new(100.0, 0.0)), &f, 300);
        assert_eq!(s.ground, Some(2), "should rest on the higher window");
    }

    #[test]
    fn when_the_window_under_her_closes_she_falls() {
        let surfaces = [Surface { id: 7, y: 300.0, x0: 100.0, x1: 200.0 }];
        let f = env(&surfaces);
        let (resting, _) = run(BodyState::at(Vec2::new(150.0, 0.0)), &f, 300);
        assert!(resting.on_ground);

        let gone = env(&[]);
        let r = step(&resting, 1.0 / 120.0, &gone);
        assert_eq!(r.event, Some(PhysicsEvent::Dropped));
        assert!(!r.state.on_ground);
        let (later, _) = run(r.state, &gone, 60);
        assert!(later.pos.y > resting.pos.y, "she did not fall");
    }

    #[test]
    fn walking_off_the_edge_of_a_window_drops_her() {
        let surfaces = [Surface { id: 7, y: 300.0, x0: 100.0, x1: 200.0 }];
        let f = env(&surfaces);
        let (mut s, _) = run(BodyState::at(Vec2::new(190.0, 0.0)), &f, 300);
        assert!(s.on_ground);
        s.pos.x = 250.0; // stepped past the right edge
        let r = step(&s, 1.0 / 120.0, &f);
        assert_eq!(r.event, Some(PhysicsEvent::Dropped));
    }

    #[test]
    fn a_grab_pins_her_to_the_pointer_and_builds_velocity() {
        let sf = floor();
        let mut f = env(&sf);
        let mut s = BodyState::at(Vec2::new(0.0, 0.0));
        for i in 1..=10 {
            f.grab = Some(Vec2::new(i as f32 * 10.0, 0.0));
            let r = step(&s, 1.0 / 120.0, &f);
            s = r.state;
            assert_eq!(s.pos, f.grab.unwrap(), "she should be under the pointer");
        }
        assert!(s.grabbed);
        assert!(s.vel.x > 500.0, "the drag built no throw velocity: {:?}", s.vel);
    }

    #[test]
    fn releasing_a_drag_launches_her_with_the_carried_velocity() {
        let sf = floor();
        let mut f = env(&sf);
        let mut s = BodyState::at(Vec2::new(0.0, 0.0));
        for i in 1..=10 {
            f.grab = Some(Vec2::new(i as f32 * 12.0, 0.0));
            s = step(&s, 1.0 / 120.0, &f).state;
        }
        let carried = s.vel;
        f.grab = None;
        let r = step(&s, 1.0 / 120.0, &f);
        match r.event {
            Some(PhysicsEvent::Released { speed }) => {
                assert!((speed - carried.len()).abs() < 1.0)
            }
            other => panic!("expected Released, got {other:?}"),
        }
        assert!(!r.state.grabbed);
        // She keeps going after the release, and starts to fall.
        assert!(r.state.vel.x > 100.0);
        assert!(r.state.vel.y > 0.0);
    }

    #[test]
    fn a_throw_travels_and_eventually_lands() {
        let sf = floor();
        let f = env(&sf);
        let s = BodyState {
            pos: Vec2::new(0.0, 100.0),
            vel: Vec2::new(900.0, -400.0),
            ..Default::default()
        };
        let (after, evs) = run(s, &f, 600);
        assert!(after.pos.x > 300.0, "the throw went nowhere: {:?}", after.pos);
        assert!(after.on_ground, "never came down: {after:?}");
        assert!(evs.iter().any(|e| matches!(e, PhysicsEvent::Landed { .. })));
    }

    #[test]
    fn a_fling_is_capped_at_max_speed() {
        let sf = floor();
        let mut f = env(&sf);
        f.params.max_speed = 1200.0;
        let mut s = BodyState::at(Vec2::new(0.0, 0.0));
        f.grab = Some(Vec2::new(0.0, 0.0));
        s = step(&s, 1.0 / 120.0, &f).state;
        f.grab = Some(Vec2::new(90_000.0, 0.0));
        s = step(&s, 1.0 / 120.0, &f).state;
        assert!(s.vel.len() <= 1200.0 + 1e-2, "uncapped: {:?}", s.vel);
    }

    #[test]
    fn walls_bounce_her_back_inside_the_output() {
        let sf = floor();
        let mut f = env(&sf);
        f.bounds = Some(Rect::new(Vec2::new(0.0, 0.0), Vec2::new(1920.0, 1080.0)));
        let s = BodyState {
            pos: Vec2::new(1918.0, 100.0),
            vel: Vec2::new(2000.0, 0.0),
            ..Default::default()
        };
        let r = step(&s, 1.0 / 120.0, &f);
        assert!(r.state.pos.x <= 1920.0);
        assert!(r.state.vel.x < 0.0, "did not bounce: {:?}", r.state.vel);
        assert!(matches!(r.event, Some(PhysicsEvent::HitWall { .. })));
    }

    #[test]
    fn the_bottom_of_the_output_catches_her_even_with_no_windows() {
        let mut f = env(&[]);
        f.bounds = Some(Rect::new(Vec2::new(0.0, 0.0), Vec2::new(1920.0, 1080.0)));
        let (s, _) = run(BodyState::at(Vec2::new(500.0, 0.0)), &f, 800);
        assert!(s.on_ground, "fell out of the world: {s:?}");
        assert!((s.pos.y - 1080.0).abs() < 1.0);
    }

    #[test]
    fn wind_pushes_her_sideways() {
        let sf = floor();
        let mut f = env(&sf);
        f.wind = Vec2::new(600.0, 0.0);
        let (s, _) = run(BodyState::at(Vec2::new(0.0, 0.0)), &f, 60);
        assert!(s.pos.x > 0.5, "wind did nothing: {:?}", s.pos);
    }

    #[test]
    fn friction_brings_a_slide_to_a_stop() {
        let sf = floor();
        let mut f = env(&sf);
        f.params.friction = 0.02;
        let mut s = BodyState {
            pos: Vec2::new(0.0, 500.0),
            vel: Vec2::new(800.0, 0.0),
            on_ground: true,
            ground: Some(1),
            ..Default::default()
        };
        for _ in 0..240 {
            s = step(&s, 1.0 / 120.0, &f).state;
        }
        assert!(s.vel.x.abs() < 5.0, "still sliding at {:?}", s.vel);
        assert!(s.on_ground);
    }

    #[test]
    fn impulse_launches_her_off_the_ground() {
        let s = BodyState {
            pos: Vec2::new(0.0, 500.0),
            on_ground: true,
            ground: Some(1),
            ..Default::default()
        };
        let hopped = impulse(&s, Vec2::new(0.0, -700.0), PhysicsParams::default());
        assert!(!hopped.on_ground);
        assert_eq!(hopped.ground, None);
        assert!(hopped.vel.y < 0.0);
    }

    #[test]
    fn asleep_only_when_genuinely_still() {
        let mut s = BodyState { on_ground: true, ..Default::default() };
        assert!(s.asleep(1.0));
        s.vel = Vec2::new(50.0, 0.0);
        assert!(!s.asleep(1.0));
        s.vel = Vec2::ZERO;
        s.recovering = 0.2;
        assert!(!s.asleep(1.0));
        s.recovering = 0.0;
        s.grabbed = true;
        assert!(!s.asleep(1.0));
    }

    #[test]
    fn a_stalled_frame_is_clamped_not_simulated() {
        let sf = floor();
        let f = env(&sf);
        let s = BodyState::at(Vec2::new(0.0, 0.0));
        let r = step(&s, 30.0, &f);
        assert!(r.state.pos.is_finite());
        // 30 seconds of gravity would be kilometres; the clamp keeps it sane.
        assert!(r.state.pos.y < 2000.0, "clamp missing: {:?}", r.state.pos);
    }

    #[test]
    fn bad_dt_is_a_no_op() {
        let sf = floor();
        let f = env(&sf);
        let s = BodyState::at(Vec2::new(3.0, 4.0));
        for dt in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(step(&s, dt, &f).state, s, "dt = {dt}");
        }
    }

    #[test]
    fn simulation_is_deterministic() {
        let sf = floor();
        let f = env(&sf);
        let run_once = || {
            let mut s = BodyState {
                pos: Vec2::new(10.0, 20.0),
                vel: Vec2::new(430.0, -777.0),
                ..Default::default()
            };
            for _ in 0..1000 {
                s = step(&s, 1.0 / 144.0, &f).state;
            }
            s
        };
        assert_eq!(run_once(), run_once());
    }
}
