//! Statistics for measurements: medians, percentiles as integer rationals, bootstrap intervals
//! with a seeded generator, and the Kalibera-Jones stopping rule.
//!
//! Everything here is integer arithmetic on nanosecond samples. Percentiles are rationals
//! (`numerator / denominator`) rather than floats so that no narrowing cast exists on the path
//! and the same samples give the same answer on every target. The bootstrap resamples with a
//! xorshift generator seeded from a fixed value, so a recorded sample set reproduces its interval
//! exactly [A: Kalibera & Jones, "Rigorous benchmarking in reasonable time", ISMM 2013].

use serde::{Deserialize, Serialize};

/// A percentile as an integer rational: `numerator / denominator` of the way through the sorted
/// samples. `Percentile::P99` is `99 / 100`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Percentile {
  /// Numerator.
  pub numerator: u64,
  /// Denominator, never zero.
  pub denominator: u64,
}

impl Percentile {
  /// The median.
  pub const P50: Percentile = Percentile {
    numerator: 1,
    denominator: 2,
  };
  /// Shape: the tail slates ratchets on everywhere (Part 6); one in a hundred.
  pub const P99: Percentile = Percentile {
    numerator: 99,
    denominator: 100,
  };
  /// Shape: the far tail reported beside p99; one in a thousand.
  pub const P999: Percentile = Percentile {
    numerator: 999,
    denominator: 1000,
  };
  /// Shape: the lower edge of a 95% bootstrap interval.
  pub const LOWER_95: Percentile = Percentile {
    numerator: 25,
    denominator: 1000,
  };
  /// Shape: the upper edge of a 95% bootstrap interval.
  pub const UPPER_95: Percentile = Percentile {
    numerator: 975,
    denominator: 1000,
  };

  /// The index of this percentile in a sorted sample of `len` items (nearest-rank, clamped).
  pub fn index(self, len: usize) -> usize {
    if len == 0 {
      return 0;
    }
    let last = u64::try_from(len - 1).unwrap_or(u64::MAX);
    let scaled = (last * self.numerator) / self.denominator;
    // `scaled <= last < len`, so the conversion cannot fail; the fallback is never taken.
    usize::try_from(scaled).unwrap_or(0)
  }
}

/// A sorted sample of nanosecond readings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sample {
  values: Vec<u64>,
}

impl Sample {
  /// Builds a sample, sorting the readings once.
  pub fn new(mut values: Vec<u64>) -> Self {
    values.sort_unstable();
    Self { values }
  }

  /// Adds a reading, keeping the sample sorted.
  pub fn push(&mut self, value: u64) {
    let at = self.values.partition_point(|v| *v <= value);
    self.values.insert(at, value);
  }

  /// Number of readings.
  pub fn len(&self) -> usize {
    self.values.len()
  }

  /// Whether the sample is empty.
  pub fn is_empty(&self) -> bool {
    self.values.is_empty()
  }

  /// The reading at a percentile (nearest rank), or `None` for an empty sample.
  pub fn percentile(&self, p: Percentile) -> Option<u64> {
    self.values.get(p.index(self.values.len())).copied()
  }

  /// The median, or `None` for an empty sample.
  pub fn median(&self) -> Option<u64> {
    self.percentile(Percentile::P50)
  }

  /// The smallest reading.
  pub fn min(&self) -> Option<u64> {
    self.values.first().copied()
  }

  /// The sorted readings.
  pub fn values(&self) -> &[u64] {
    &self.values
  }
}

/// A 95% bootstrap interval around the median.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interval {
  /// The median of the sample.
  pub median: u64,
  /// The lower edge of the interval.
  pub lower: u64,
  /// The upper edge of the interval.
  pub upper: u64,
}

impl Interval {
  /// The relative width of the interval as an integer per-mille of the median (0 when the
  /// median is 0, which only a broken clock produces).
  pub fn width_permille(&self) -> u64 {
    if self.median == 0 {
      return 0;
    }
    let width = u128::from(self.upper.saturating_sub(self.lower));
    let permille = width * u128::from(PERMILLE) / u128::from(self.median);
    u64::try_from(permille).unwrap_or(u64::MAX)
  }
}

/// Shape: parts per thousand, the unit every relative width in this crate is expressed in.
const PERMILLE: u64 = 1000;

/// A xorshift64* generator, seeded, allocation-free; only for resampling, never for anything
/// that needs unpredictability.
#[derive(Clone, Debug)]
pub struct Xorshift(u64);

impl Xorshift {
  /// Shape: the fixed seed every bootstrap uses so recorded samples reproduce their intervals.
  pub const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

  /// A generator from a seed; zero is replaced because xorshift cannot leave zero.
  pub const fn new(seed: u64) -> Self {
    Self(if seed == 0 { Self::SEED } else { seed })
  }

  /// The next pseudo-random value.
  pub fn next_u64(&mut self) -> u64 {
    let mut x = self.0;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    self.0 = x;
    // Format: the xorshift64* output multiplier (Vigna, "An experimental exploration of
    // Marsaglia's xorshift generators, scrambled", 2016).
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
  }

  /// A uniformly distributed index below `bound` (`bound > 0`), by rejection on the top bits.
  pub fn below(&mut self, bound: usize) -> usize {
    let bound_u64 = u64::try_from(bound).unwrap_or(u64::MAX);
    if bound_u64 <= 1 {
      return 0;
    }
    // Lemire's multiply-shift with rejection of the biased zone.
    let threshold = bound_u64.wrapping_neg() % bound_u64;
    loop {
      let x = self.next_u64();
      let m = u128::from(x) * u128::from(bound_u64);
      let low = u64::try_from(m & u128::from(u64::MAX)).unwrap_or(0);
      if low >= threshold {
        return usize::try_from(m >> u64::BITS).unwrap_or(0);
      }
    }
  }
}

/// Shape: the number of bootstrap resamples; Kalibera and Jones use 1,000 and report that more
/// does not narrow the estimate for medians of this size.
pub const BOOTSTRAP_RESAMPLES: usize = 1000;

/// The 95% bootstrap interval around the median of `sample`, or `None` when empty.
pub fn bootstrap_interval(sample: &Sample, rng: &mut Xorshift) -> Option<Interval> {
  let values = sample.values();
  let median = sample.median()?;
  if values.len() == 1 {
    return Some(Interval {
      median,
      lower: median,
      upper: median,
    });
  }
  let mut medians: Vec<u64> = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
  let mut scratch: Vec<u64> = Vec::with_capacity(values.len());
  for _ in 0..BOOTSTRAP_RESAMPLES {
    scratch.clear();
    for _ in 0..values.len() {
      scratch.push(values[rng.below(values.len())]);
    }
    scratch.sort_unstable();
    medians.push(scratch[Percentile::P50.index(scratch.len())]);
  }
  medians.sort_unstable();
  let lower = medians[Percentile::LOWER_95.index(medians.len())];
  let upper = medians[Percentile::UPPER_95.index(medians.len())];
  Some(Interval {
    median,
    lower,
    upper,
  })
}

/// The stopping rule: the interval has converged when its width is at most this fraction of the
/// median. Shape: Kalibera and Jones's "narrow enough" bar, ratified as a shape constant in the
/// gap ledger (GAPS §5): one tenth of the median.
pub const CONVERGED_WIDTH_PERMILLE: u64 = 100;

/// Whether an interval satisfies the stopping rule.
pub fn converged(interval: &Interval) -> bool {
  interval.width_permille() <= CONVERGED_WIDTH_PERMILLE
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn percentiles_use_nearest_rank_without_floats() {
    let s = Sample::new((1..=100).collect());
    assert_eq!(s.median(), Some(50));
    assert_eq!(s.percentile(Percentile::P99), Some(99));
    assert_eq!(s.percentile(Percentile::P999), Some(99));
    assert_eq!(Sample::new(vec![]).median(), None);
    assert_eq!(Sample::new(vec![7]).percentile(Percentile::P99), Some(7));
  }

  #[test]
  fn push_keeps_the_sample_sorted() {
    let mut s = Sample::new(vec![5, 1, 9]);
    s.push(4);
    s.push(10);
    assert_eq!(s.values(), &[1, 4, 5, 9, 10]);
  }

  #[test]
  fn a_tight_sample_converges_and_a_noisy_one_does_not() {
    let tight = Sample::new(vec![100; 64]);
    let mut rng = Xorshift::new(Xorshift::SEED);
    let i = bootstrap_interval(&tight, &mut rng).unwrap();
    assert_eq!((i.lower, i.median, i.upper), (100, 100, 100));
    assert!(converged(&i));

    let noisy = Sample::new(
      (0..64)
        .map(|k| if k % 2 == 0 { 100 } else { 1000 })
        .collect(),
    );
    let j = bootstrap_interval(&noisy, &mut rng).unwrap();
    assert!(j.lower < j.upper, "{j:?}");
    assert!(!converged(&j), "{j:?}");
  }

  #[test]
  fn bootstrap_is_reproducible_for_a_fixed_seed() {
    let s = Sample::new((0..200).map(|k| 1000 + (k * 37) % 91).collect());
    let a = bootstrap_interval(&s, &mut Xorshift::new(Xorshift::SEED)).unwrap();
    let b = bootstrap_interval(&s, &mut Xorshift::new(Xorshift::SEED)).unwrap();
    assert_eq!(a, b);
  }

  #[test]
  fn below_is_unbiased_enough_to_hit_every_bucket() {
    let mut rng = Xorshift::new(Xorshift::SEED);
    let mut hits = [0u32; 7];
    for _ in 0..7000 {
      hits[rng.below(7)] += 1;
    }
    assert!(hits.iter().all(|h| *h > 700), "{hits:?}");
  }
}
