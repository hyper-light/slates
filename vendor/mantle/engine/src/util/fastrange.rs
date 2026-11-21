//! Mapping a hash onto `[0, range)` by the high half of `hash · range`, Lemire's "fastrange":
//! RocksDB's `util/fastrange.h`. It keys filter probes and Ribbon starts, so it is part of
//! their formats (docs/research/24 §1.2).

use crate::util::hash::upper32of64;
use crate::util::math128::upper64of128;

/// `FastRange32` [R util/fastrange.h:107-110]: `(range · hash) >> 32`.
pub fn fast_range32(hash: u32, range: u32) -> u32 {
    upper32of64(u64::from(range).wrapping_mul(u64::from(hash)))
}

/// `FastRange64` [R util/fastrange.h:101-104]: `(range · hash) >> 64`.
pub fn fast_range64(hash: u64, range: usize) -> usize {
    // usize is at most 64 bits on every target; the result is below `range`, so it fits back.
    let wide = u128::from(range as u64).wrapping_mul(u128::from(hash));
    usize::try_from(upper64of128(wide)).unwrap_or(range)
}

/// `FastRangeGeneric` [R util/fastrange.h:34-94] for a 32- or 64-bit hash and an unsigned
/// range no wider than the hash; the result is below `range`.
pub trait FastRangeHash: Copy {
    fn fast_range_generic<R>(self, range: R) -> R
    where
        R: Copy + Into<Self> + TryFrom<Self>;
}

impl FastRangeHash for u32 {
    fn fast_range_generic<R>(self, range: R) -> R
    where
        R: Copy + Into<u32> + TryFrom<u32>,
    {
        R::try_from(fast_range32(self, range.into())).unwrap_or(range)
    }
}

impl FastRangeHash for u64 {
    fn fast_range_generic<R>(self, range: R) -> R
    where
        R: Copy + Into<u64> + TryFrom<u64>,
    {
        let wide = u128::from(range.into()).wrapping_mul(u128::from(self));
        R::try_from(upper64of128(wide)).unwrap_or(range)
    }
}
