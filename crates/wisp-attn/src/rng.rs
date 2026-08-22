//! A tiny, seeded, fully deterministic PRNG.
//!
//! This crate must reproduce the same antics from the same seed and the same
//! observation trace, so we do not pull in `rand` (whose algorithms and thread
//! locals are not part of any contract we control). This is `splitmix64` —
//! ~1.5ns per draw, statistically fine for picking which idle animation plays,
//! and, crucially, *stable forever*.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rng {
    state: u64,
}

impl Default for Rng {
    fn default() -> Self {
        // Arbitrary, but fixed: an unseeded runner is still deterministic.
        Rng::new(0x5750_5F41_5454_4E00)
    }
}

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Rng { state: seed }
    }

    /// The raw state, so a host can persist and restore an exact stream.
    pub const fn state(self) -> u64 {
        self.state
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`. Returns 0 for `n == 0`.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        // Lemire-style rejection-free-enough reduction. Bias is < 2^-53 for the
        // small `n` we ever use (list lengths, weights), and it is deterministic.
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32
    }

    /// True with probability `permille / 1000`.
    pub fn chance_permille(&mut self, permille: u16) -> bool {
        if permille == 0 {
            return false;
        }
        if permille >= 1000 {
            return true;
        }
        self.below(1000) < permille as u64
    }

    /// Index into a weighted list. Returns `None` when the list is empty or
    /// every weight is zero — callers must treat that as "no choice made"
    /// rather than silently picking the first entry.
    pub fn weighted(&mut self, weights: &[u32]) -> Option<usize> {
        let total: u64 = weights.iter().map(|w| *w as u64).sum();
        if total == 0 {
            return None;
        }
        let mut pick = self.below(total);
        for (i, w) in weights.iter().enumerate() {
            let w = *w as u64;
            if pick < w {
                return Some(i);
            }
            pick -= w;
        }
        // Unreachable given the sum above, but never panic in her head.
        weights.iter().rposition(|w| *w > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::isolate;

    #[test]
    fn same_seed_same_stream() {
        isolate();
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        let mut c = Rng::new(43);
        let sa: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let sb: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        let sc: Vec<u64> = (0..8).map(|_| c.next_u64()).collect();
        assert_eq!(sa, sb);
        assert_ne!(sa, sc);
    }

    #[test]
    fn below_is_in_range_and_covers() {
        isolate();
        let mut r = Rng::new(7);
        let mut seen = [0u32; 5];
        for _ in 0..2000 {
            let v = r.below(5) as usize;
            assert!(v < 5);
            seen[v] += 1;
        }
        assert!(seen.iter().all(|c| *c > 250), "poor coverage: {seen:?}");
        assert_eq!(Rng::new(1).below(0), 0);
    }

    #[test]
    fn unit_in_range() {
        isolate();
        let mut r = Rng::new(99);
        for _ in 0..1000 {
            let u = r.unit();
            assert!((0.0..1.0).contains(&u), "{u}");
        }
    }

    #[test]
    fn weighted_respects_zero_and_shape() {
        isolate();
        let mut r = Rng::new(5);
        assert_eq!(r.weighted(&[]), None);
        assert_eq!(r.weighted(&[0, 0, 0]), None);
        // A zero-weight entry must never be chosen.
        let mut counts = [0u32; 3];
        for _ in 0..3000 {
            let i = r.weighted(&[10, 0, 1]).unwrap();
            counts[i] += 1;
        }
        assert_eq!(counts[1], 0);
        assert!(counts[0] > counts[2] * 4, "{counts:?}");
    }

    #[test]
    fn chance_edges() {
        isolate();
        let mut r = Rng::new(3);
        assert!(!r.chance_permille(0));
        assert!(r.chance_permille(1000));
        assert!(r.chance_permille(2000));
    }
}
