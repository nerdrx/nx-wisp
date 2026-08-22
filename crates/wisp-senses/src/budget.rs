//! What each sense is allowed to cost at each tier.
//!
//! SPEC §0.1: "she costs nothing when it matters", and §3.1: downgrades are
//! applied synchronously and a subsystem that cannot honour one **sheds the
//! work rather than queueing it**. Nothing in this crate queues — a sense that
//! is asked to cost less simply looks less often, and the samples it did not
//! take are gone rather than deferred.
//!
//! Every rule is a pure function of the tier, so the ladder is one table that
//! can be read, argued with, and tested without a governor.

use std::time::Duration;

use wisp_proto::Tier;

/// How long the KWin script coalesces geometry before one D-Bus call.
///
/// This is the sense that scales with what the operator is *doing*: dragging a
/// window at T1 is ~110 D-Bus calls a second, and at T3 a game owns the GPU and
/// she has no business costing KWin's main thread that much.
pub fn terrain_flush_ms(tier: Tier) -> u32 {
    match tier {
        Tier::Feral | Tier::Full => crate::kwin::script::DEFAULT_FLUSH_MS, // ~110/s
        Tier::Reduced => 16,                                              // ~60/s
        Tier::Lobotomised => 50,                                          // ~20/s
        // Not zero: she still needs to know where the ground is if she is
        // standing on it, she just does not need to know smoothly.
        Tier::Dormant => 250,
    }
}

/// How often vitals are sampled. The only polling sense in the crate.
pub fn vitals_interval(tier: Tier) -> Duration {
    match tier {
        Tier::Feral | Tier::Full => Duration::from_secs(5),
        Tier::Reduced => Duration::from_secs(15),
        Tier::Lobotomised => Duration::from_secs(30),
        Tier::Dormant => Duration::from_secs(120),
    }
}

/// How often the audio meter publishes a level.
pub fn audio_interval(tier: Tier) -> Duration {
    match tier {
        Tier::Feral | Tier::Full => Duration::from_millis(500),
        Tier::Reduced => Duration::from_millis(1000),
        Tier::Lobotomised => Duration::from_millis(2000),
        Tier::Dormant => Duration::from_millis(5000),
    }
}

/// Is the terrain feed worth running at all?
///
/// At `Dormant` she is silenced or the machine is in thermal trouble; the KWin
/// script is unloaded entirely rather than left ticking, because the cost we
/// care about there is KWin's, not ours.
pub fn terrain_runs(tier: Tier) -> bool {
    tier != Tier::Dormant
}

#[cfg(test)]
mod tests {
    use super::*;

    const LADDER: [Tier; 5] =
        [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant];

    #[test]
    fn every_budget_only_loosens_as_she_is_pushed_down() {
        for w in LADDER.windows(2) {
            assert!(
                terrain_flush_ms(w[1]) >= terrain_flush_ms(w[0]),
                "terrain got *more* expensive going {:?} -> {:?}",
                w[0],
                w[1]
            );
            assert!(vitals_interval(w[1]) >= vitals_interval(w[0]));
            assert!(audio_interval(w[1]) >= audio_interval(w[0]));
        }
    }

    #[test]
    fn t0_and_t1_are_the_same_budget() {
        // Feral is "operator away, do background work", not "spend more on
        // watching a desktop nobody is looking at".
        assert_eq!(terrain_flush_ms(Tier::Feral), terrain_flush_ms(Tier::Full));
        assert_eq!(vitals_interval(Tier::Feral), vitals_interval(Tier::Full));
    }

    #[test]
    fn the_full_tier_matches_the_measured_default() {
        assert_eq!(terrain_flush_ms(Tier::Full), crate::kwin::script::DEFAULT_FLUSH_MS);
    }

    #[test]
    fn a_game_owning_the_gpu_costs_kwin_almost_nothing() {
        // At T3 the feed is 20 Hz rather than 110 Hz: a fifth of the D-Bus
        // calls into the thread that is also compositing the game.
        assert!(terrain_flush_ms(Tier::Lobotomised) >= 50);
        assert!(vitals_interval(Tier::Lobotomised) >= Duration::from_secs(30));
    }

    #[test]
    fn dormant_unloads_the_script_rather_than_slowing_it() {
        assert!(!terrain_runs(Tier::Dormant));
        for t in [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised] {
            assert!(terrain_runs(t), "{t:?} should still see the terrain");
        }
    }

    #[test]
    fn nothing_is_ever_zero() {
        // A zero interval would be a busy loop, and a zero flush would be one
        // D-Bus call per KWin signal at the tier that can least afford it.
        for t in LADDER {
            assert!(terrain_flush_ms(t) > 0);
            assert!(vitals_interval(t) > Duration::ZERO);
            assert!(audio_interval(t) > Duration::ZERO);
        }
    }
}
