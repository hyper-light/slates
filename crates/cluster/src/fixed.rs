//! Deterministic fixed-point arithmetic for the failure detector's timing (§4.8) — specifically the
//! Lifeguard suspicion-timeout curve `max − (max−min)·log(C+1)/log(K+1)`, which needs a logarithm. The
//! codebase is integer-only for tuning and the deterministic simulation replays failure histories from a
//! seed, so the timeout must be **bit-identical on every host**; a floating-point `ln()` is not
//! (IEEE 754 does not require a correctly-rounded log, so libm implementations differ in the last place).
//! This module computes the logarithm in fixed point with integer operations alone, so the same seed
//! produces the same history everywhere.
//!
//! Evidence: Clay Turner, "A Fast Binary Logarithm Algorithm", IEEE Signal Processing Magazine, 2010
//! (tier A) — the iterative-squaring binary logarithm implemented here.

/// The number of fractional bits in the fixed-point logarithm (a Q16.16 result).
/// Shape: the largest bit width whose mantissa (below `2^(bits+1)`) squares without overflowing `u64`
/// with margin — `2^17` squared is `2^34`, far below `2^64` — while giving ample fractional precision
/// for a period count. It is a representation width, not a tuning value.
const FRACTION_BITS: u32 = 16;

/// `log2(x)` scaled by `2^FRACTION_BITS`, computed with integer operations only, for `x >= 1`
/// (`log2(1) = 0`). A power of two is exact; other values are the iterative-squaring approximation.
pub(crate) fn log2_fixed(x: u64) -> u64 {
  if x <= 1 {
    return 0;
  }
  // The integer part is the position of the highest set bit; x >= 2 here, so it is at least one.
  let integer_part = (u64::BITS - 1) - x.leading_zeros();
  let mut result = u64::from(integer_part) << FRACTION_BITS;

  // Normalise x to the mantissa in [1, 2), scaled to Q(FRACTION_BITS): shift so the leading one lands at
  // bit FRACTION_BITS.
  let mut mantissa = if integer_part >= FRACTION_BITS {
    x >> (integer_part - FRACTION_BITS)
  } else {
    x << (FRACTION_BITS - integer_part)
  };
  let one = 1u64 << FRACTION_BITS;

  // Refine one fractional bit per iteration by repeated squaring (Turner's algorithm): squaring the
  // mantissa and testing whether it crossed 2.0 recovers the next bit of the fraction.
  for bit in (0..FRACTION_BITS).rev() {
    mantissa = (mantissa * mantissa) >> FRACTION_BITS;
    if mantissa >= (one << 1) {
      mantissa >>= 1;
      result |= 1u64 << bit;
    }
  }
  result
}

/// The Lifeguard suspicion window in periods for `confirmations` independent suspicions of a member
/// (§4.8 "suspicion timeout `max − (max−min)·log(C+1)/log(K+1)`"): with no confirmations it is the full
/// `max_periods`; as independent peers confirm the suspicion it shrinks along the logarithm toward the
/// `min_periods` floor, reaching it once `confirmations` meets the `expected` count — so a
/// well-corroborated failure is declared dead sooner, while a lone suspicion waits the full window. The
/// ratio's `2^FRACTION_BITS` scale cancels, so the result is exact integer arithmetic.
pub(crate) fn suspicion_window(
  confirmations: u64,
  max_periods: u32,
  min_periods: u32,
  expected: u32,
) -> u32 {
  let max = max_periods;
  let min = min_periods.min(max);
  if max == min {
    return max;
  }
  // At least one confirmation is expected (a zero would make the denominator `log2(1) = 0`).
  let expected = expected.max(1);
  let numerator = log2_fixed(confirmations.saturating_add(1));
  let denominator = log2_fixed(u64::from(expected).saturating_add(1));
  let span = u64::from(max - min);
  let reduction = if numerator >= denominator {
    span
  } else {
    // span · log2(C+1) / log2(K+1); the fixed-point scale cancels in the ratio.
    span.saturating_mul(numerator) / denominator
  };
  let reduction = u32::try_from(reduction).unwrap_or(u32::MAX).min(max - min);
  max - reduction
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
    // 1.585 · 2^16 ≈ 103872; allow a small approximation tolerance.
    assert!(three.abs_diff(103_872) < 64, "log2(3) ≈ 1.585 in Q16.16");
  }

  /// The suspicion window is the full max with no confirmations, shrinks monotonically as confirmations
  /// accumulate, and reaches the floor once the expected count is met.
  #[test]
  fn the_window_shrinks_from_max_to_min_with_confirmations() {
    let (max, min, expected) = (10u32, 2u32, 4u32);
    assert_eq!(
      suspicion_window(0, max, min, expected),
      max,
      "no confirmations: the full window"
    );
    assert_eq!(
      suspicion_window(u64::from(expected), max, min, expected),
      min,
      "the expected count: the floor"
    );
    assert_eq!(
      suspicion_window(100, max, min, expected),
      min,
      "past the expected count: still the floor"
    );

    // Monotonically non-increasing between.
    let mut previous = max;
    for c in 0..=expected {
      let window = suspicion_window(u64::from(c), max, min, expected);
      assert!(window <= previous, "non-increasing at {c} confirmations");
      assert!(window >= min && window <= max, "within [min, max] at {c}");
      previous = window;
    }
  }

  /// A degenerate window (`min == max`) is constant — the confirmation curve is off.
  #[test]
  fn an_equal_floor_and_ceiling_is_constant() {
    for c in 0..10u64 {
      assert_eq!(suspicion_window(c, 5, 5, 3), 5);
    }
  }
}
