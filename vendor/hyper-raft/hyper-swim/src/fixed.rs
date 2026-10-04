//! Deterministic fixed-point arithmetic for the failure detector: the logarithm in SWIM §4.1's
//! dissemination budget ([`crate::detector::gossip_transmits`]). The deterministic simulation
//! replays failure histories from a seed, so the budget must be **bit-identical on every host**; a
//! floating-point `ln()` is not
//! (IEEE 754 does not require a correctly-rounded log, so libm implementations differ in the last place).
//! This module computes the logarithm in fixed point with integer operations alone, so the same seed
//! produces the same history everywhere. The rest of the budget's arithmetic is IEEE 754's
//! correctly rounded operations.
//!
//! Evidence: Clay Turner, "A Fast Binary Logarithm Algorithm", IEEE Signal Processing Magazine, 2010
//! (tier A) — the iterative-squaring binary logarithm implemented here.

/// The number of fractional bits in the fixed-point logarithm (a Q16.16 result).
/// Shape: the largest bit width whose mantissa (below `2^(bits+1)`) squares without overflowing `u64`
/// with margin — `2^17` squared is `2^34`, far below `2^64` — while giving ample fractional precision
/// for a period count. It is a representation width, not a tuning value.
pub(crate) const FRACTION_BITS: u32 = 16;

/// `log2(x)` scaled by `2^FRACTION_BITS`, computed with integer operations only, for `x >= 1`
/// (`log2(1) = 0`). A power of two is exact; other values are the iterative-squaring approximation.
pub(crate) fn log2_fixed(x: u64) -> u64 {
    if x <= 1 {
        return 0;
    }
    // The integer part is the position of the highest set bit; x >= 2 here, so it is at least one.
    let integer_part = (u64::BITS.saturating_sub(1)).saturating_sub(x.leading_zeros());
    let mut result = u64::from(integer_part) << FRACTION_BITS;

    // Normalise x to the mantissa in [1, 2), scaled to Q(FRACTION_BITS): shift so the leading one lands at
    // bit FRACTION_BITS.
    let mut mantissa = if integer_part >= FRACTION_BITS {
        x.checked_shr(integer_part.saturating_sub(FRACTION_BITS))
            .unwrap_or(0)
    } else {
        x.checked_shl(FRACTION_BITS.saturating_sub(integer_part))
            .unwrap_or(0)
    };
    let one = 1u64 << FRACTION_BITS;

    // Refine one fractional bit per iteration by repeated squaring (Turner's algorithm): squaring the
    // mantissa and testing whether it crossed 2.0 recovers the next bit of the fraction.
    for bit in (0..FRACTION_BITS).rev() {
        // The mantissa stays below `2 << FRACTION_BITS`, so its square fits a `u64` with room to spare.
        mantissa = mantissa.saturating_mul(mantissa) >> FRACTION_BITS;
        if mantissa >= (one << 1) {
            mantissa >>= 1;
            result |= 1u64 << bit;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A power of two has an exact fixed-point logarithm: `log2(2^k) = k`.
    #[test]
    fn powers_of_two_are_exact() {
        assert_eq!(log2_fixed(1), 0);
        for k in 1..40u32 {
            let value = 1u64 << k;
            assert_eq!(
                log2_fixed(value),
                u64::from(k) << 16,
                "log2(2^{k}) is exactly {k}"
            );
        }
    }

    /// The logarithm is monotone non-decreasing, and a non-power lands between its neighbours (log2(3) is
    /// between 1 and 2, nearer 2 — about 1.585).
    #[test]
    fn the_logarithm_is_monotone_and_bounded() {
        let mut previous = 0;
        for x in 1..2000u64 {
            let current = log2_fixed(x);
            assert!(current >= previous, "monotone at {x}");
            previous = current;
        }
        let three = log2_fixed(3);
        assert!(
            three > (1u64 << 16) && three < (2u64 << 16),
            "1 < log2(3) < 2"
        );
        // log2(3)·2^16 = 103872.23…: the algorithm's sixteen bits are its floor.
        assert_eq!(three, 103_872, "log2(3) in Q16.16");
    }
}
