//! The simulation's only source of randomness.

use rand_chacha::ChaCha8Rng;
use rand_chacha::rand_core::{Rng, SeedableRng};

/// A seeded random stream: ChaCha8, expanded from one `u64` seed.
///
/// Every random choice in a simulation (message delays, faults, partitions, the entropy
/// handed to nodes) is drawn from one `SimRng`, in the order the run makes them, so a
/// seed names exactly one run. Nothing here reads the operating system's entropy.
#[derive(Clone, Debug)]
pub struct SimRng(ChaCha8Rng);

impl SimRng {
    /// The stream for `seed`.
    #[must_use]
    pub fn from_seed(seed: u64) -> Self {
        Self(ChaCha8Rng::seed_from_u64(seed))
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    /// A uniform value in `0..n`.
    ///
    /// # Panics
    ///
    /// If `n` is zero.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "SimRng::below(0)");
        // Reject the lowest `2^64 mod n` values so the rest divide evenly into `n` buckets.
        let reject = n.wrapping_neg() % n;
        loop {
            let x = self.next_u64();
            if x >= reject {
                return x % n;
            }
        }
    }

    /// A uniform value in `lo..=hi`.
    ///
    /// # Panics
    ///
    /// If `lo > hi`.
    pub fn between(&mut self, lo: u64, hi: u64) -> u64 {
        assert!(lo <= hi, "SimRng::between({lo}, {hi})");
        match (hi - lo).checked_add(1) {
            Some(span) => lo + self.below(span),
            None => self.next_u64(),
        }
    }

    /// True with probability `chance`.
    pub fn chance(&mut self, chance: Chance) -> bool {
        self.below(Chance::SCALE) < u64::from(chance.per_million)
    }

    /// Shuffles `items` in place (Fisher-Yates).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = usize::try_from(self.below(i as u64 + 1)).expect("index fits in usize");
            items.swap(i, j);
        }
    }
}

/// A probability, in parts per million. Integer, so it compares and draws identically on
/// every platform.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Chance {
    per_million: u32,
}

impl Chance {
    const SCALE: u64 = 1_000_000;

    /// Never happens.
    #[must_use]
    pub const fn never() -> Self {
        Self { per_million: 0 }
    }

    /// Always happens.
    #[must_use]
    pub const fn always() -> Self {
        Self {
            per_million: 1_000_000,
        }
    }

    /// `p` in a million; values above a million mean always.
    #[must_use]
    pub const fn per_million(p: u32) -> Self {
        Self {
            per_million: if p > 1_000_000 { 1_000_000 } else { p },
        }
    }

    /// `p` percent; values above 100 mean always.
    #[must_use]
    pub const fn percent(p: u32) -> Self {
        Self::per_million(p.saturating_mul(10_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a stream that is not a pure function of its seed (reading OS entropy or a
    /// clock), and two seeds that collapse onto one stream.
    #[test]
    fn a_seed_names_one_stream() {
        let draw = |seed| {
            let mut r = SimRng::from_seed(seed);
            (0..16).map(|_| r.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(draw(7), draw(7));
        assert_ne!(draw(7), draw(8));
    }

    /// Catches: an off-by-one in `between` (an end never drawn, or a value outside the
    /// range), and `chance` extremes that are not exact.
    #[test]
    fn ranges_are_inclusive_and_extremes_exact() {
        let mut r = SimRng::from_seed(1);
        let mut seen = [false; 4];
        for _ in 0..1_000 {
            let v = r.between(3, 6);
            assert!((3..=6).contains(&v), "{v} outside 3..=6");
            seen[usize::try_from(v - 3).unwrap()] = true;
        }
        assert_eq!(seen, [true; 4]);
        assert_eq!(r.between(9, 9), 9);
        let _ = r.between(0, u64::MAX);
        assert!((0..1_000).all(|_| r.chance(Chance::always())));
        assert!((0..1_000).all(|_| !r.chance(Chance::never())));
        assert_eq!(Chance::percent(250), Chance::always());
    }

    /// Catches: a shuffle that loses or duplicates items.
    #[test]
    fn shuffle_permutes() {
        let mut r = SimRng::from_seed(3);
        let mut v: Vec<u32> = (0..20).collect();
        r.shuffle(&mut v);
        let mut sorted = v.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..20).collect::<Vec<_>>());
        assert_ne!(v, sorted, "seed 3 should move something");
    }
}
