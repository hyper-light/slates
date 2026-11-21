//! A fixed-size log-linear latency histogram.
//!
//! Values are bucketed by their power of two and, within it, by the next `SUB_BITS` bits, as
//! HdrHistogram does (Tene, "HdrHistogram: A High Dynamic Range Histogram"), so the relative
//! error of any reported percentile is below 2^-SUB_BITS at every magnitude, in constant
//! memory. A percentile is its bucket's upper bound, so the error is one-sided: it never
//! under-reports a latency. Values are nanoseconds; anything at or above 2^41 ns (~37
//! minutes) lands in a final overflow bucket whose bound is `u64::MAX`.

/// 3.1% one-sided error in 1,185 buckets (9.5 KB): a third of the ±10% the benchmark's
/// repeated runs resolve, so quantization is not what decides a comparison between runs
/// (docs/research/11 §14, §16).
const SUB_BITS: u32 = 5;
const SUB: usize = 1 << SUB_BITS;
/// The largest power of two with its own row.
const MAX_EXP: u32 = 40;
/// Row 0 holds the values below `SUB` exactly; row r >= 1 holds exponent r - 1 + SUB_BITS.
const ROWS: usize = (MAX_EXP - SUB_BITS) as usize + 2;
const OVERFLOW: usize = ROWS * SUB;
const BUCKETS: usize = OVERFLOW + 1;
const SUB_MASK: u64 = SUB as u64 - 1;

#[derive(Clone)]
pub struct Histogram {
    counts: Box<[u64; BUCKETS]>,
    total: u64,
    max: u64,
}

impl std::fmt::Debug for Histogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Histogram")
            .field("count", &self.total)
            .field("p50", &self.p50())
            .field("p99", &self.p99())
            .field("max", &self.max)
            .finish()
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub fn new() -> Self {
        Self {
            counts: Box::new([0; BUCKETS]),
            total: 0,
            max: 0,
        }
    }

    pub fn record(&mut self, value: u64) {
        if let Some(slot) = self.counts.get_mut(index(value)) {
            *slot = slot.saturating_add(1);
        }
        self.total = self.total.saturating_add(1);
        self.max = self.max.max(value);
    }

    pub fn merge(&mut self, other: &Self) {
        for (mine, theirs) in self.counts.iter_mut().zip(other.counts.iter()) {
            *mine = mine.saturating_add(*theirs);
        }
        self.total = self.total.saturating_add(other.total);
        self.max = self.max.max(other.max);
    }

    pub fn count(&self) -> u64 {
        self.total
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    /// The smallest bucket upper bound at or below which the fraction `ppm / 1_000_000` of
    /// values fall; zero when empty. The bound is clamped to the largest value recorded.
    pub fn quantile(&self, ppm: u32) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let ppm = u128::from(ppm.min(1_000_000));
        // ceil(total * ppm / 1e6) in u128: total < 2^64 and ppm <= 1e6, so nothing overflows.
        let rank = u128::from(self.total)
            .saturating_mul(ppm)
            .div_ceil(1_000_000);
        let rank = u64::try_from(rank.max(1)).unwrap_or(u64::MAX);
        let mut seen = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            seen = seen.saturating_add(c);
            if seen >= rank {
                return upper_bound(i).min(self.max);
            }
        }
        self.max
    }

    pub fn p50(&self) -> u64 {
        self.quantile(500_000)
    }

    pub fn p99(&self) -> u64 {
        self.quantile(990_000)
    }

    pub fn p999(&self) -> u64 {
        self.quantile(999_000)
    }
}

fn index(value: u64) -> usize {
    if value < SUB as u64 {
        // Values below SUB have exact buckets in the first row.
        return usize::try_from(value).unwrap_or(0);
    }
    let exp = 63u32.saturating_sub(value.leading_zeros());
    if exp > MAX_EXP {
        return OVERFLOW;
    }
    let shift = exp.saturating_sub(SUB_BITS);
    let sub = usize::try_from(value.checked_shr(shift).unwrap_or(0) & SUB_MASK).unwrap_or(0);
    let row = usize::try_from(exp.saturating_sub(SUB_BITS).saturating_add(1)).unwrap_or(0);
    row.saturating_mul(SUB).saturating_add(sub)
}

fn upper_bound(index: usize) -> u64 {
    if index >= OVERFLOW {
        return u64::MAX;
    }
    let row = index / SUB;
    let sub = (index % SUB) as u64;
    if row == 0 {
        return sub;
    }
    let shift = u32::try_from(row.saturating_sub(1)).unwrap_or(u32::MAX);
    let base = (SUB as u64).checked_shl(shift).unwrap_or(u64::MAX);
    let width = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    base.saturating_add(sub.saturating_mul(width))
        .saturating_add(width.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn empty_is_zero() {
        let h = Histogram::new();
        assert_eq!(h.p99(), 0);
        assert_eq!(h.count(), 0);
    }

    #[test]
    fn small_values_are_exact() {
        let mut h = Histogram::new();
        for v in 0..8 {
            h.record(v);
        }
        assert_eq!(h.quantile(1_000_000), 7);
        assert_eq!(h.p50(), 3);
    }

    #[test]
    fn huge_values_saturate_into_the_last_bucket() {
        let mut h = Histogram::new();
        h.record(u64::MAX);
        assert_eq!(h.quantile(1_000_000), u64::MAX);
    }

    proptest! {
        #[test]
        fn percentiles_bound_the_true_value_within_one_thirty_second(
            mut values in proptest::collection::vec(1u64..(1 << 40), 1..2000),
            ppm in 1u32..=1_000_000,
        ) {
            let mut h = Histogram::new();
            for &v in &values { h.record(v); }
            values.sort_unstable();
            let rank = ((values.len() as u128 * ppm as u128).div_ceil(1_000_000)).max(1) as usize;
            let truth = values[rank - 1];
            let got = h.quantile(ppm);
            prop_assert!(got >= truth, "q{ppm}: {got} < true {truth}");
            prop_assert!(got as f64 <= truth as f64 * 1.03125 + 1.0, "q{ppm}: {got} >> true {truth}");
        }

        #[test]
        fn merge_equals_recording_everything(a in proptest::collection::vec(0u64..1_000_000, 0..500),
                                             b in proptest::collection::vec(0u64..1_000_000, 0..500)) {
            let (mut ha, mut hb, mut all) = (Histogram::new(), Histogram::new(), Histogram::new());
            for &v in &a { ha.record(v); all.record(v); }
            for &v in &b { hb.record(v); all.record(v); }
            ha.merge(&hb);
            for q in [10_000, 500_000, 900_000, 990_000, 999_000, 1_000_000] {
                prop_assert_eq!(ha.quantile(q), all.quantile(q));
            }
        }
    }
}
