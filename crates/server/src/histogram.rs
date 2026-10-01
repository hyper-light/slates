//! A bounded histogram of durations (AUD-29-25's percentile lane; §4.3, §4.14 "histograms in `_ns`"): what a
//! shard records about the slices of its granted landings, so a run publishes their p50/p99/p999 and maximum
//! without keeping every sample. Log-linear buckets — eight per power of two — so its memory is fixed (512
//! counters) whatever the run's length, a recorded value lands in its bucket in constant time, and a
//! percentile reads back as its bucket's upper bound: never below the true value and within one eighth above
//! it (HdrHistogram's one-significant-digit class, Tene; the exact maximum is kept beside it). Values under
//! eight are exact.

/// Format: mantissa bits per power of two — eight sub-buckets an octave, the precision stated above.
const SUB_BUCKET_BITS: u32 = 3;
/// Format: the sub-buckets an octave holds (`1 << SUB_BUCKET_BITS`).
const SUB_BUCKETS: u64 = 8;
/// Format: the buckets that cover every `u64`: values under eight get one each (eight), and each octave from
/// `2^3` to `2^63` gets eight (61 × 8) — 496, held in 512 for a whole number of cache lines.
const BUCKETS: usize = 512;
/// Format: parts per million, the unit a quantile is asked in (p999 = 999,000).
pub const PPM: u64 = 1_000_000;

/// The histogram.
#[derive(Clone, Debug)]
pub struct DurationHistogram {
  counts: Vec<u64>,
  total: u64,
  max: u64,
}

impl Default for DurationHistogram {
  fn default() -> DurationHistogram {
    DurationHistogram {
      counts: vec![0; BUCKETS],
      total: 0,
      max: 0,
    }
  }
}

/// The bucket a value falls in.
fn bucket_of(value: u64) -> usize {
  if value < SUB_BUCKETS {
    return usize::try_from(value).unwrap_or(0);
  }
  // The value's octave (its highest set bit, at least 3) and its top three bits below that.
  let octave = u64::BITS
    .saturating_sub(1)
    .saturating_sub(value.leading_zeros());
  let shift = octave.saturating_sub(SUB_BUCKET_BITS);
  let top = value.checked_shr(shift).unwrap_or(0);
  let index = u64::from(octave.saturating_sub(SUB_BUCKET_BITS).saturating_add(1))
    .saturating_mul(SUB_BUCKETS)
    .saturating_add(top.saturating_sub(SUB_BUCKETS));
  usize::try_from(index).unwrap_or(BUCKETS.saturating_sub(1))
}

/// The largest value a bucket holds: the upper bound a quantile reads back.
fn upper_bound(bucket: usize) -> u64 {
  let bucket = u64::try_from(bucket).unwrap_or(u64::MAX);
  if bucket < SUB_BUCKETS {
    return bucket;
  }
  let octave_rank = bucket / SUB_BUCKETS;
  let top = SUB_BUCKETS.saturating_add(bucket % SUB_BUCKETS);
  let shift = u32::try_from(octave_rank.saturating_sub(1)).unwrap_or(u32::MAX);
  // In 128 bits: the top octave's bound (`16 << 60`) does not fit 64, and a shift drops bits silently.
  u128::from(top.saturating_add(1))
    .checked_shl(shift)
    .and_then(|bound| u64::try_from(bound.saturating_sub(1)).ok())
    .unwrap_or(u64::MAX)
}

impl DurationHistogram {
  /// Records one value.
  pub fn record(&mut self, value: u64) {
    if let Some(count) = self.counts.get_mut(bucket_of(value)) {
      *count = count.saturating_add(1);
    }
    self.total = self.total.saturating_add(1);
    self.max = self.max.max(value);
  }

  /// The values recorded.
  pub fn count(&self) -> u64 {
    self.total
  }

  /// The largest value recorded, exactly.
  pub fn max(&self) -> u64 {
    self.max
  }

  /// The value at quantile `ppm` (parts per million; p99 = 990,000): the upper bound of the bucket holding
  /// the `⌈total × ppm / 10⁶⌉`-th smallest value, capped at the exact maximum; 0 when nothing was recorded.
  pub fn quantile(&self, ppm: u64) -> u64 {
    let wanted = u128::from(self.total)
      .saturating_mul(u128::from(ppm.min(PPM)))
      .div_ceil(u128::from(PPM))
      .max(1);
    let mut seen = 0u128;
    for (bucket, count) in self.counts.iter().enumerate() {
      seen = seen.saturating_add(u128::from(*count));
      if seen >= wanted {
        return upper_bound(bucket).min(self.max);
      }
    }
    self.max
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The census: every value under eight, and in every octave from `2^3` to `2^63` each sub-bucket's lowest
  /// and highest value — so every bucket the histogram has is reached, at both its edges.
  fn census() -> Vec<u64> {
    let mut values: Vec<u64> = (0..SUB_BUCKETS).collect();
    for octave in SUB_BUCKET_BITS..u64::BITS {
      let step = 1u64 << (octave - SUB_BUCKET_BITS);
      for sub in 0..SUB_BUCKETS {
        let low = (SUB_BUCKETS + sub) * step;
        values.push(low);
        values.push(low + (step - 1));
      }
    }
    values
  }

  /// AUD-29-25. Do: record the census (every bucket at both edges), then ask every quantile a run can name —
  /// the rank of each recorded value — and the published p50, p99 and p999. Expect: each reads back at or
  /// above the exact value and within one eighth above it, the maximum is exact, and every bucket the census
  /// reached is distinct (no two octaves share a bucket).
  #[test]
  fn a_quantile_reads_back_within_an_eighth_above_the_exact_value() {
    let mut values = census();
    let mut histogram = DurationHistogram::default();
    for value in &values {
      histogram.record(*value);
    }
    values.sort_unstable();
    let total = values.len() as u64;
    let mut ranks: Vec<u64> = (1..=total).map(|rank| rank * PPM / total).collect();
    ranks.extend([PPM / 2, 990_000, 999_000, PPM]);
    for ppm in ranks {
      let rank = (u128::from(total) * u128::from(ppm))
        .div_ceil(u128::from(PPM))
        .max(1);
      let exact = values[usize::try_from(rank).unwrap() - 1];
      let read = histogram.quantile(ppm);
      assert!(read >= exact, "p{ppm}: {read} below the exact {exact}");
      assert!(
        u128::from(read) <= u128::from(exact) + u128::from(exact) / 8,
        "p{ppm}: {read} more than an eighth above the exact {exact}"
      );
    }
    assert_eq!(histogram.max(), u64::MAX);
    assert_eq!(histogram.count(), total);
    let mut buckets: Vec<usize> = values.iter().map(|v| bucket_of(*v)).collect();
    buckets.dedup();
    assert_eq!(
      buckets.len(),
      8 + 61 * 8,
      "every bucket reached, each once in order"
    );
    assert!(buckets.iter().all(|b| *b < BUCKETS));
  }

  /// AUD-29-25. Do: ask an empty histogram for a quantile, then record one value and ask again. Expect: 0
  /// when nothing was recorded; the one value, exactly (capped at the maximum), at every quantile after.
  #[test]
  fn an_empty_histogram_reads_zero_and_one_value_reads_itself() {
    let mut histogram = DurationHistogram::default();
    assert_eq!(histogram.quantile(990_000), 0);
    histogram.record(1_234_567);
    for ppm in [0, PPM / 2, 999_000, PPM] {
      assert_eq!(histogram.quantile(ppm), 1_234_567);
    }
  }
}
