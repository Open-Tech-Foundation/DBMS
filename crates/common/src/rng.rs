//! Injectable pseudo-random number generation.

use std::sync::atomic::{AtomicU64, Ordering};

/// A source of pseudo-random 64-bit values, injectable so scheduling, IO
/// timing, and fault placement can be made reproducible from a seed.
pub trait Rng: Send + Sync {
    /// Return the next pseudo-random `u64`.
    fn next_u64(&self) -> u64;
}

/// A deterministic [SplitMix64] generator. Seeding it identically reproduces
/// the exact stream, which is what makes simulated runs replayable.
///
/// [SplitMix64]: https://prng.di.unimi.it/splitmix64.c
///
/// # Examples
///
/// ```
/// use common::{Rng, SeededRng};
///
/// let a = SeededRng::new(42);
/// let b = SeededRng::new(42);
/// assert_eq!(a.next_u64(), b.next_u64());
/// ```
#[derive(Debug)]
pub struct SeededRng {
    state: AtomicU64,
}

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

impl SeededRng {
    /// Create a generator seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            state: AtomicU64::new(seed),
        }
    }
}

impl Rng for SeededRng {
    fn next_u64(&self) -> u64 {
        // Advance the state, then run the SplitMix64 finalizer. `fetch_add`
        // returns the previous value, so the post-increment state is `+ GAMMA`.
        let z = self
            .state
            .fetch_add(GAMMA, Ordering::Relaxed)
            .wrapping_add(GAMMA);
        let z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn same_seed_same_stream() {
        let a = SeededRng::new(7);
        let b = SeededRng::new(7);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seed_diverges() {
        let a = SeededRng::new(1);
        let b = SeededRng::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn no_immediate_collisions() {
        let rng = SeededRng::new(99);
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(rng.next_u64()), "unexpected early collision");
        }
    }
}
