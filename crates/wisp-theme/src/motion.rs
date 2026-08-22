//! DESIGN.md §6. Motion is liquid, brief, and interruptible.
//!
//! The three named curves are real cubic Béziers, evaluated the way a browser
//! evaluates `cubic-bezier()`: solve x(s) = t by Newton–Raphson with a
//! bisection fallback, then return y(s). Nothing interactive may exceed 320ms,
//! and that is asserted, not remembered.

/// A CSS `cubic-bezier(x1, y1, x2, y2)` curve. P0 is (0,0), P3 is (1,1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Easing {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

/// `--ease-spring` — overshoots. Only for playful, identity-bearing moves:
/// the tab indicator, a tile press.
pub const SPRING: Easing = Easing::new(0.32, 1.35, 0.42, 1.0);
/// `--ease-soft` — the workhorse, every default interaction.
pub const SOFT: Easing = Easing::new(0.2, 0.8, 0.2, 1.0);
/// `--ease-out` — entrances.
pub const OUT: Easing = Easing::new(0.16, 1.0, 0.3, 1.0);
/// What every decorative curve collapses to under reduced motion.
pub const LINEAR: Easing = Easing::new(0.0, 0.0, 1.0, 1.0);

/// `--dur-fast`: interaction feedback.
pub const DUR_FAST_MS: u32 = 150;
/// `--dur`: view changes.
pub const DUR_MS: u32 = 220;
/// `--dur-slow`: sheets, entrances.
pub const DUR_SLOW_MS: u32 = 320;
/// §6: "Nothing interactive exceeds 320ms."
pub const MAX_INTERACTIVE_MS: u32 = 320;

/// The view-switch recipe of §6: 180ms crossfade + an 8px slide.
pub const VIEW_SWITCH_MS: u32 = 180;
pub const VIEW_SWITCH_SLIDE_PX: f32 = 8.0;

/// The press recipe of §10, which is the same on every platform: scale to 0.96
/// on down in `--dur-fast`, release through the spring in `--dur`.
pub const PRESS_SCALE: f32 = 0.96;
/// The hover lift of §5, in px. Cards lift 2, buttons 1.
pub const HOVER_LIFT_CARD_PX: f32 = 2.0;
pub const HOVER_LIFT_BUTTON_PX: f32 = 1.0;

impl Easing {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Easing {
        Easing { x1, y1, x2, y2 }
    }

    fn bezier(a: f32, b: f32, s: f32) -> f32 {
        // 3(1-s)²s·a + 3(1-s)s²·b + s³   (P0 = 0, P3 = 1)
        let u = 1.0 - s;
        3.0 * u * u * s * a + 3.0 * u * s * s * b + s * s * s
    }

    fn bezier_slope(a: f32, b: f32, s: f32) -> f32 {
        let u = 1.0 - s;
        3.0 * u * u * a + 6.0 * u * s * (b - a) + 3.0 * s * s * (1.0 - b)
    }

    /// x as a function of the curve parameter.
    pub fn x_at(&self, s: f32) -> f32 {
        Self::bezier(self.x1, self.x2, s)
    }

    /// y as a function of the curve parameter.
    pub fn y_at(&self, s: f32) -> f32 {
        Self::bezier(self.y1, self.y2, s)
    }

    /// Evaluate the easing: progress in, eased progress out. `t` is clamped to
    /// 0..=1; the result is **not** clamped, because [`SPRING`] is supposed to
    /// overshoot past 1 and clamping it would silently delete the spring.
    pub fn eval(&self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        if t <= 0.0 || t >= 1.0 {
            return t;
        }
        self.y_at(self.solve_s(t))
    }

    /// Interpolate between two values through the curve.
    pub fn mix(&self, from: f32, to: f32, t: f32) -> f32 {
        from + (to - from) * self.eval(t)
    }

    /// Find the curve parameter `s` where x(s) == t.
    fn solve_s(&self, t: f32) -> f32 {
        let mut s = t;
        // Newton–Raphson: converges in a handful of steps for well-behaved
        // control points, which all three of ours are.
        for _ in 0..8 {
            let err = Self::bezier(self.x1, self.x2, s) - t;
            if err.abs() < 1e-6 {
                return s;
            }
            let d = Self::bezier_slope(self.x1, self.x2, s);
            if d.abs() < 1e-6 {
                break;
            }
            s -= err / d;
        }
        // Bisection fallback — always terminates, never leaves 0..1.
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        let mut s = t.clamp(0.0, 1.0);
        for _ in 0..32 {
            let x = Self::bezier(self.x1, self.x2, s);
            if (x - t).abs() < 1e-6 {
                break;
            }
            if x > t {
                hi = s;
            } else {
                lo = s;
            }
            s = (lo + hi) * 0.5;
        }
        s
    }
}

/// The operator's reduced-motion preference. §6: non-negotiable — nebula
/// frozen, sheens off, springs replaced, every transition collapses to opacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Motion {
    #[default]
    Full,
    Reduced,
}

impl Motion {
    /// The curve to actually use. Under reduce, the spring's overshoot — the
    /// whole point of the spring — is exactly what has to go.
    pub fn curve(self, wanted: Easing) -> Easing {
        match self {
            Motion::Full => wanted,
            Motion::Reduced => LINEAR,
        }
    }

    /// The duration to actually use. Under reduce, transforms collapse and
    /// only the opacity crossfade survives, at the fast duration.
    pub fn duration_ms(self, wanted: u32) -> u32 {
        match self {
            Motion::Full => wanted.min(MAX_INTERACTIVE_MS),
            Motion::Reduced => wanted.min(DUR_FAST_MS),
        }
    }

    /// May a purely decorative animation run at all? The nebula drift, the
    /// starfield, an indeterminate sheen.
    pub fn decorative_allowed(self) -> bool {
        matches!(self, Motion::Full)
    }
}

/// A specular highlight's position. §1: **light rides motion — it never flashes
/// on command.** The sheen's position is a function of a continuous driver
/// (pointer, tilt, scroll, progress), not of elapsed time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sheen {
    /// Bound to something that is actually moving. `0..=1`.
    Driven(f32),
    /// The single exception §1 grants: nothing continuous exists to bind to
    /// (indeterminate progress). Freezes under reduced motion.
    Indeterminate,
    Off,
}

impl Sheen {
    /// Where the band sits, 0..=1, or `None` if it must not be drawn.
    pub fn position(self, motion: Motion, elapsed_ms: u64) -> Option<f32> {
        match self {
            Sheen::Off => None,
            Sheen::Driven(t) => match motion {
                Motion::Full => Some(t.clamp(0.0, 1.0)),
                Motion::Reduced => None,
            },
            Sheen::Indeterminate => {
                if !motion.decorative_allowed() {
                    return None;
                }
                const PERIOD_MS: u64 = 1400;
                Some((elapsed_ms % PERIOD_MS) as f32 / PERIOD_MS as f32)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURVES: [(&str, Easing); 4] =
        [("spring", SPRING), ("soft", SOFT), ("out", OUT), ("linear", LINEAR)];

    #[test]
    fn every_curve_pins_its_endpoints() {
        for (name, e) in CURVES {
            assert!((e.eval(0.0) - 0.0).abs() < 1e-5, "{name} at 0");
            assert!((e.eval(1.0) - 1.0).abs() < 1e-5, "{name} at 1");
        }
    }

    #[test]
    fn eval_inverts_x_correctly() {
        // The defining property: eval(x(s)) == y(s).
        for (name, e) in CURVES {
            for i in 1..20 {
                let s = i as f32 / 20.0;
                let x = e.x_at(s);
                let y = e.y_at(s);
                assert!((e.eval(x) - y).abs() < 1e-3, "{name} s={s} x={x} y={y}");
            }
        }
    }

    #[test]
    fn soft_and_out_never_overshoot() {
        for (name, e) in [("soft", SOFT), ("out", OUT), ("linear", LINEAR)] {
            for i in 0..=100 {
                let v = e.eval(i as f32 / 100.0);
                assert!((-1e-4..=1.0 + 1e-4).contains(&v), "{name} overshot to {v}");
            }
        }
    }

    #[test]
    fn soft_and_out_are_monotone() {
        for (name, e) in [("soft", SOFT), ("out", OUT)] {
            let mut prev = -1.0;
            for i in 0..=100 {
                let v = e.eval(i as f32 / 100.0);
                assert!(v >= prev - 1e-4, "{name} went backwards at {i}");
                prev = v;
            }
        }
    }

    #[test]
    fn out_is_the_fastest_out_of_the_gate() {
        // An entrance curve must have covered most of its distance early.
        assert!(OUT.eval(0.3) > SOFT.eval(0.3));
        assert!(SOFT.eval(0.3) > LINEAR.eval(0.3));
    }

    #[test]
    fn the_spring_actually_springs() {
        let peak = (0..=100).map(|i| SPRING.eval(i as f32 / 100.0)).fold(0.0f32, f32::max);
        assert!(peak > 1.02, "spring must overshoot; peaked at {peak}");
        assert!(peak < 1.35, "…but not launch off the screen; peaked at {peak}");
        // And it must come back and land.
        assert!((SPRING.eval(1.0) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn nothing_interactive_exceeds_320ms() {
        for d in [DUR_FAST_MS, DUR_MS, DUR_SLOW_MS, VIEW_SWITCH_MS] {
            assert!(d <= MAX_INTERACTIVE_MS, "{d}ms breaks §6");
        }
        assert_eq!(Motion::Full.duration_ms(1000), MAX_INTERACTIVE_MS);
    }

    #[test]
    fn reduced_motion_kills_the_spring_and_the_sheen() {
        assert_eq!(Motion::Reduced.curve(SPRING), LINEAR);
        assert!(!Motion::Reduced.decorative_allowed());
        assert_eq!(Sheen::Driven(0.5).position(Motion::Reduced, 0), None);
        assert_eq!(Sheen::Indeterminate.position(Motion::Reduced, 0), None);
        assert!(Motion::Reduced.duration_ms(320) <= DUR_FAST_MS);
    }

    #[test]
    fn a_driven_sheen_tracks_its_driver_and_does_not_tick() {
        // Same driver, different clocks: the highlight must not move.
        let a = Sheen::Driven(0.42).position(Motion::Full, 0);
        let b = Sheen::Driven(0.42).position(Motion::Full, 999_999);
        assert_eq!(a, b);
        assert_eq!(a, Some(0.42));
        // The one sanctioned exception is allowed to depend on the clock.
        assert_ne!(
            Sheen::Indeterminate.position(Motion::Full, 0),
            Sheen::Indeterminate.position(Motion::Full, 700)
        );
    }

    #[test]
    fn mix_walks_between_the_endpoints() {
        assert!((SOFT.mix(10.0, 20.0, 0.0) - 10.0).abs() < 1e-4);
        assert!((SOFT.mix(10.0, 20.0, 1.0) - 20.0).abs() < 1e-4);
    }
}
