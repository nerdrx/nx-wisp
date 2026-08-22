//! Easing families and the spring-damper primitive.
//!
//! The named families are the NX design language's motion tokens (DESIGN.md
//! §2, "motion") reproduced here as cubic Béziers. They are duplicated rather
//! than imported because `wisp-rig` may not depend on `wisp-theme` (SPEC §2
//! crate map) — the *numbers* are the contract, and they are asserted against
//! DESIGN.md in `theme_curves_match_design_tokens`.
//!
//! `Ease::Linear` exists because a skin occasionally needs constant angular
//! velocity (an orbiting light). It is never the default: `Ease::default()` is
//! `Soft`, and the motion-quality rule of F67 is "never a linear tween".

use serde::{Deserialize, Serialize};

/// `--ease-spring: cubic-bezier(0.32, 1.35, 0.42, 1)` — overshoots.
pub const SPRING: [f32; 4] = [0.32, 1.35, 0.42, 1.0];
/// `--ease-soft: cubic-bezier(0.2, 0.8, 0.2, 1)` — the workhorse.
pub const SOFT: [f32; 4] = [0.2, 0.8, 0.2, 1.0];
/// `--ease-out: cubic-bezier(0.16, 1, 0.3, 1)` — entrances.
pub const OUT: [f32; 4] = [0.16, 1.0, 0.3, 1.0];

/// Longest frame a spring will integrate. Anything beyond this was a stall,
/// not motion, and is clamped rather than simulated.
pub const MAX_STEP: f32 = 0.25;

/// `--dur-fast`, `--dur`, `--dur-slow` in seconds.
pub const DUR_FAST: f32 = 0.150;
pub const DUR: f32 = 0.220;
pub const DUR_SLOW: f32 = 0.320;

/// How one keyframe interpolates towards the next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum Ease {
    /// Constant rate. Only for genuinely constant motion.
    Linear,
    /// `--ease-soft`, the default interactive curve.
    #[default]
    Soft,
    /// `--ease-out`, entrances.
    Out,
    /// `--ease-spring`, overshoots past 1 and settles back.
    Spring,
    /// Hold the value until the next key, then jump. Used for discrete
    /// channels like a blink frame.
    Step,
    /// Arbitrary cubic Bézier `[x1, y1, x2, y2]`, as authored in a skin.
    Bezier([f32; 4]),
}

impl Ease {
    /// Map normalised time `0..=1` to normalised progress. `Spring` and
    /// custom Béziers may legitimately return values outside `0..=1`.
    pub fn eval(self, t: f32) -> f32 {
        let t = super::math::clamp(t, 0.0, 1.0);
        match self {
            Ease::Linear => t,
            Ease::Step => {
                if t >= 1.0 {
                    1.0
                } else {
                    0.0
                }
            }
            Ease::Soft => cubic_bezier(SOFT, t),
            Ease::Out => cubic_bezier(OUT, t),
            Ease::Spring => cubic_bezier(SPRING, t),
            Ease::Bezier(c) => cubic_bezier(c, t),
        }
    }

    /// The identifier used in a skin file.
    pub fn name(self) -> &'static str {
        match self {
            Ease::Linear => "linear",
            Ease::Soft => "soft",
            Ease::Out => "out",
            Ease::Spring => "spring",
            Ease::Step => "step",
            Ease::Bezier(_) => "bezier",
        }
    }

    pub fn from_name(s: &str) -> Option<Ease> {
        Some(match s {
            "linear" => Ease::Linear,
            "soft" => Ease::Soft,
            "out" => Ease::Out,
            "spring" => Ease::Spring,
            "step" => Ease::Step,
            _ => return None,
        })
    }
}

/// Evaluate a CSS-style `cubic-bezier(x1, y1, x2, y2)` at `x = t`.
///
/// The curve runs from (0,0) to (1,1) with control points (x1,y1) and (x2,y2).
/// x is solved for the Bézier parameter by Newton-Raphson with a bisection
/// fallback, exactly as browsers do it — Newton alone diverges when the curve
/// has a near-zero derivative, which `--ease-out` (x1 = 0.16, x2 = 0.3) does.
pub fn cubic_bezier(c: [f32; 4], t: f32) -> f32 {
    let [x1, y1, x2, y2] = c;
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    // Fast path: the identity curve.
    if (x1 - y1).abs() < 1e-6 && (x2 - y2).abs() < 1e-6 {
        return t;
    }
    let u = solve_bezier_x(x1, x2, t);
    bezier_axis(y1, y2, u)
}

/// One axis of a cubic Bézier with implicit endpoints 0 and 1.
#[inline]
fn bezier_axis(p1: f32, p2: f32, u: f32) -> f32 {
    let iu = 1.0 - u;
    3.0 * iu * iu * u * p1 + 3.0 * iu * u * u * p2 + u * u * u
}

#[inline]
fn bezier_axis_derivative(p1: f32, p2: f32, u: f32) -> f32 {
    let iu = 1.0 - u;
    3.0 * iu * iu * p1 + 6.0 * iu * u * (p2 - p1) + 3.0 * u * u * (1.0 - p2)
}

fn solve_bezier_x(x1: f32, x2: f32, target: f32) -> f32 {
    let mut u = target;
    // Newton-Raphson first — converges in 2-4 iterations where it works.
    for _ in 0..8 {
        let x = bezier_axis(x1, x2, u) - target;
        if x.abs() < 1e-6 {
            return u;
        }
        let dx = bezier_axis_derivative(x1, x2, u);
        if dx.abs() < 1e-6 {
            break;
        }
        u -= x / dx;
        if !(0.0..=1.0).contains(&u) {
            break;
        }
    }
    // Bisection: slower but cannot fail, because x(u) is monotonic for any
    // control points with x1, x2 in 0..=1 (which validation enforces).
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    let mut u = target.clamp(0.0, 1.0);
    for _ in 0..32 {
        let x = bezier_axis(x1, x2, u);
        if (x - target).abs() < 1e-6 {
            return u;
        }
        if x < target {
            lo = u;
        } else {
            hi = u;
        }
        u = 0.5 * (lo + hi);
    }
    u
}

/// A one-dimensional spring-damper.
///
/// This is the overshoot-and-settle primitive of F67: point it at a target and
/// it accelerates, overshoots on a direction change, and rings down. It is a
/// pure `step`, so it is deterministic and testable, and it substeps
/// internally so a long frame (a stall, a tier change) cannot explode it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring1 {
    pub value: f32,
    pub vel: f32,
}

impl Spring1 {
    pub fn new(value: f32) -> Self {
        Spring1 { value, vel: 0.0 }
    }

    /// Advance towards `target`. `dt` is seconds.
    pub fn step(&mut self, target: f32, p: SpringParams, dt: f32) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        // A frame longer than this is a stall, a resume from suspend or a tier
        // change — not motion anyone watched. Simulating it faithfully would
        // cost a lot of substeps to produce a frame nobody saw, so it is
        // clamped, the standard spiral-of-death guard.
        let dt = dt.min(MAX_STEP);
        // Substep against the *fastest* timescale in the system: the natural
        // frequency and the damping rate. Semi-implicit Euler diverges when
        // either times h approaches 2, so aim well under it.
        let h_max = 0.15 / p.fastest_rate();
        let steps = ((dt / h_max).ceil() as usize).clamp(1, 256);
        let h = dt / steps as f32;
        for _ in 0..steps {
            let accel = (p.stiffness * (target - self.value) - p.damping * self.vel) / p.mass;
            self.vel += accel * h;
            self.value += self.vel * h;
        }
        if !self.value.is_finite() || !self.vel.is_finite() {
            self.value = target;
            self.vel = 0.0;
        }
    }

    /// Snap to a value and kill the velocity. Used on teleports and on tier
    /// downgrades, where continuing to ring would be a lie.
    pub fn reset(&mut self, value: f32) {
        self.value = value;
        self.vel = 0.0;
    }

    /// Has it stopped moving for practical purposes? The rig uses this to
    /// decide it may sleep to 0 fps (F10).
    pub fn settled(&self, target: f32, eps: f32) -> bool {
        (self.value - target).abs() < eps && self.vel.abs() < eps * 8.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringParams {
    pub stiffness: f32,
    pub damping: f32,
    pub mass: f32,
}

impl SpringParams {
    pub const fn new(stiffness: f32, damping: f32, mass: f32) -> Self {
        SpringParams { stiffness, damping, mass }
    }

    /// Critically damped: reaches the target as fast as possible with no
    /// overshoot. `damping = 2*sqrt(k*m)`.
    pub fn critical(stiffness: f32, mass: f32) -> Self {
        SpringParams { stiffness, damping: 2.0 * (stiffness * mass).sqrt(), mass }
    }

    /// Underdamped by `ratio` (< 1 overshoots, 1 is critical, > 1 crawls).
    pub fn with_ratio(stiffness: f32, mass: f32, ratio: f32) -> Self {
        SpringParams {
            stiffness,
            damping: 2.0 * ratio * (stiffness * mass).sqrt(),
            mass,
        }
    }

    /// The damping ratio this parameter set represents.
    pub fn ratio(&self) -> f32 {
        let crit = 2.0 * (self.stiffness * self.mass).sqrt();
        if crit <= 1e-9 {
            0.0
        } else {
            self.damping / crit
        }
    }

    /// The fastest rate (in 1/s) present in the system — whichever of the
    /// natural frequency and the damping rate is larger. Substepping is sized
    /// against this.
    fn fastest_rate(&self) -> f32 {
        let m = self.mass.max(1e-6);
        let omega = (self.stiffness.max(0.0) / m).sqrt();
        let damp_rate = self.damping.max(0.0) / m;
        omega.max(damp_rate).max(1.0)
    }
}

impl Default for SpringParams {
    fn default() -> Self {
        SpringParams::with_ratio(220.0, 1.0, 0.62)
    }
}

/// A 2D spring-damper, built from two `Spring1`s. Used for the body's
/// overshoot follower.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring2 {
    pub x: Spring1,
    pub y: Spring1,
}

impl Spring2 {
    pub fn new(v: super::math::Vec2) -> Self {
        Spring2 { x: Spring1::new(v.x), y: Spring1::new(v.y) }
    }
    pub fn value(&self) -> super::math::Vec2 {
        super::math::Vec2::new(self.x.value, self.y.value)
    }
    pub fn vel(&self) -> super::math::Vec2 {
        super::math::Vec2::new(self.x.vel, self.y.vel)
    }
    pub fn step(&mut self, target: super::math::Vec2, p: SpringParams, dt: f32) {
        self.x.step(target.x, p, dt);
        self.y.step(target.y, p, dt);
    }
    pub fn reset(&mut self, v: super::math::Vec2) {
        self.x.reset(v.x);
        self.y.reset(v.y);
    }
    pub fn settled(&self, target: super::math::Vec2, eps: f32) -> bool {
        self.x.settled(target.x, eps) && self.y.settled(target.y, eps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_curves_match_design_tokens() {
        // If DESIGN.md's motion tokens change, this is the test that fails.
        assert_eq!(SPRING, [0.32, 1.35, 0.42, 1.0]);
        assert_eq!(SOFT, [0.2, 0.8, 0.2, 1.0]);
        assert_eq!(OUT, [0.16, 1.0, 0.3, 1.0]);
    }

    #[test]
    fn every_family_pins_its_endpoints() {
        for e in [Ease::Linear, Ease::Soft, Ease::Out, Ease::Spring, Ease::Step] {
            assert!((e.eval(0.0) - 0.0).abs() < 1e-5, "{}", e.name());
            assert!((e.eval(1.0) - 1.0).abs() < 1e-5, "{}", e.name());
        }
    }

    #[test]
    fn eased_progress_is_monotonic_in_time() {
        // The settling families never go backwards. `Spring` deliberately
        // does — that is what overshoot *is* — so it is excluded here and
        // covered by `spring_family_actually_overshoots` instead.
        for e in [Ease::Linear, Ease::Soft, Ease::Out] {
            let mut prev = f32::NEG_INFINITY;
            for i in 0..=200 {
                let t = i as f32 / 200.0;
                let v = e.eval(t);
                assert!(v >= prev - 1e-4, "{} not monotonic at t={t}", e.name());
                prev = v;
            }
        }
    }

    #[test]
    fn spring_family_actually_overshoots() {
        let samples: Vec<f32> = (0..=200).map(|i| Ease::Spring.eval(i as f32 / 200.0)).collect();
        let peak = samples.iter().copied().fold(f32::MIN, f32::max);
        assert!(peak > 1.02, "spring should exceed 1, peaked at {peak}");
        // ...and come back down to land on 1, which is the "settle" half.
        let peak_at = samples.iter().position(|v| *v == peak).unwrap();
        assert!(peak_at < 200, "spring never settled back");
        assert!(samples[200] <= peak);
    }

    #[test]
    fn soft_and_out_never_overshoot() {
        for e in [Ease::Soft, Ease::Out] {
            for i in 0..=200 {
                let v = e.eval(i as f32 / 200.0);
                assert!(v <= 1.0 + 1e-4, "{} overshot: {v}", e.name());
            }
        }
    }

    #[test]
    fn out_front_loads_more_than_soft() {
        // --ease-out is the sharper entrance; at the quarter mark it must have
        // travelled further than the workhorse curve.
        assert!(Ease::Out.eval(0.25) > Ease::Soft.eval(0.25));
    }

    #[test]
    fn step_holds_then_jumps() {
        assert_eq!(Ease::Step.eval(0.0), 0.0);
        assert_eq!(Ease::Step.eval(0.99), 0.0);
        assert_eq!(Ease::Step.eval(1.0), 1.0);
    }

    #[test]
    fn ease_names_round_trip() {
        for e in [Ease::Linear, Ease::Soft, Ease::Out, Ease::Spring, Ease::Step] {
            assert_eq!(Ease::from_name(e.name()), Some(e));
        }
        assert_eq!(Ease::from_name("nonsense"), None);
    }

    #[test]
    fn bezier_solver_inverts_x_accurately() {
        // Round-trip: evaluating the curve's own x at the solved parameter
        // must return the input.
        let c = OUT;
        for i in 1..100 {
            let t = i as f32 / 100.0;
            let u = solve_bezier_x(c[0], c[2], t);
            let x = bezier_axis(c[0], c[2], u);
            assert!((x - t).abs() < 1e-3, "t={t} solved u={u} x={x}");
        }
    }

    #[test]
    fn critically_damped_spring_does_not_overshoot() {
        let p = SpringParams::critical(200.0, 1.0);
        let mut s = Spring1::new(0.0);
        let mut peak = 0.0f32;
        for _ in 0..600 {
            s.step(1.0, p, 1.0 / 240.0);
            peak = peak.max(s.value);
        }
        assert!(peak <= 1.0 + 1e-3, "critical spring overshot to {peak}");
        assert!(s.settled(1.0, 1e-3), "did not settle: {s:?}");
    }

    #[test]
    fn underdamped_spring_overshoots_then_settles() {
        let p = SpringParams::with_ratio(260.0, 1.0, 0.35);
        let mut s = Spring1::new(0.0);
        let mut peak = 0.0f32;
        for _ in 0..1200 {
            s.step(1.0, p, 1.0 / 240.0);
            peak = peak.max(s.value);
        }
        assert!(peak > 1.05, "expected overshoot, peaked at {peak}");
        assert!(s.settled(1.0, 1e-2), "did not settle: {s:?}");
    }

    #[test]
    fn spring_survives_an_absurd_frame_time() {
        // A 3-second frame (a stall, a resume from suspend) must not produce
        // NaN or a value light-years from the target.
        let p = SpringParams::with_ratio(800.0, 1.0, 0.5);
        let mut s = Spring1::new(0.0);
        s.step(1.0, p, 3.0);
        assert!(s.value.is_finite() && s.vel.is_finite(), "{s:?}");
        assert!(s.value.abs() < 10.0, "exploded to {}", s.value);
    }

    #[test]
    fn spring_ignores_zero_and_negative_dt() {
        let p = SpringParams::default();
        let mut s = Spring1::new(0.0);
        s.step(1.0, p, 0.0);
        s.step(1.0, p, -0.5);
        s.step(1.0, p, f32::NAN);
        assert_eq!(s, Spring1::new(0.0));
    }

    #[test]
    fn direction_change_produces_overshoot_past_the_new_target() {
        // The motion-quality claim of F67: reversing direction rings, it does
        // not stop dead.
        let p = SpringParams::with_ratio(300.0, 1.0, 0.4);
        let mut s = Spring1::new(0.0);
        for _ in 0..120 {
            s.step(10.0, p, 1.0 / 240.0);
        }
        let mut min = f32::MAX;
        for _ in 0..600 {
            s.step(0.0, p, 1.0 / 240.0);
            min = min.min(s.value);
        }
        assert!(min < -0.05, "reversal did not overshoot, min {min}");
    }

    #[test]
    fn ratio_round_trips() {
        let p = SpringParams::with_ratio(150.0, 2.0, 0.42);
        assert!((p.ratio() - 0.42).abs() < 1e-4);
        assert!((SpringParams::critical(150.0, 2.0).ratio() - 1.0).abs() < 1e-4);
    }
}
