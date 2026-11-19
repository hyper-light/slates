//! Decimal text as Python's `'%d'` writes it (A-569; A-545's reply numbers): the lengths SSH frames carry, a push's
//! result, a `reply:pushkey`'s numbers. Written into a fixed buffer a caller borrows, so writing one allocates nothing
//! and cannot fail.

/// Format: the most bytes a `u64` or an `i64` takes as decimal text: `u64::MAX` has 20 digits, `i64::MIN` 19 and a
/// sign.
const MAX_LEN: usize = 20;

/// Format: decimal's base.
const RADIX: u64 = 10;

/// Format: the digit zero, from which each digit's byte counts.
const ZERO: u8 = b'0';

/// Format: a negative number's sign.
const MINUS: u8 = b'-';

/// A number's decimal text, held in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Decimal {
  /// The text, right-aligned in the buffer.
  bytes: [u8; MAX_LEN],
  /// Where the text begins.
  start: usize,
}

impl Decimal {
  /// The text of an unsigned number.
  pub(crate) fn unsigned(value: u64) -> Self {
    Self::with_sign(value, false)
  }

  /// The text of a signed number.
  pub(crate) fn signed(value: i64) -> Self {
    Self::with_sign(value.unsigned_abs(), value < 0)
  }

  /// The digits of `magnitude`, right to left, and the sign before them when `negative`. Every `u64` has at most
  /// [`MAX_LEN`] digits, and a negative `i64`'s magnitude one fewer, so the buffer always holds the text.
  fn with_sign(magnitude: u64, negative: bool) -> Self {
    let mut bytes = [ZERO; MAX_LEN];
    let mut start = MAX_LEN;
    let mut rest = magnitude;
    loop {
      start = start.saturating_sub(1);
      let digit = u8::try_from(rest % RADIX).unwrap_or(0);
      if let Some(slot) = bytes.get_mut(start) {
        *slot = ZERO.saturating_add(digit);
      }
      rest /= RADIX;
      if rest == 0 {
        break;
      }
    }
    if negative {
      start = start.saturating_sub(1);
      if let Some(slot) = bytes.get_mut(start) {
        *slot = MINUS;
      }
    }
    Self { bytes, start }
  }

  /// The text.
  pub(crate) fn as_bytes(&self) -> &[u8] {
    self.bytes.get(self.start..).unwrap_or_default()
  }
}

#[cfg(test)]
mod tests {
  use super::Decimal;
  use proptest::prelude::*;

  /// A-569: do write zero, the extremes of both widths and a negative number. Expect Python's `'%d'` text of each,
  /// the widest 20 bytes long.
  #[test]
  fn the_extremes_are_written_whole() {
    assert_eq!(Decimal::unsigned(0).as_bytes(), b"0");
    assert_eq!(Decimal::unsigned(u64::MAX).as_bytes(), b"18446744073709551615");
    assert_eq!(Decimal::signed(i64::MIN).as_bytes(), b"-9223372036854775808");
    assert_eq!(Decimal::signed(-7).as_bytes(), b"-7");
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-569: do write any signed and any unsigned number. Expect the text Rust's own formatting writes.
    #[test]
    fn every_number_is_written_as_rust_writes_it(signed in any::<i64>(), unsigned in any::<u64>()) {
      prop_assert_eq!(Decimal::signed(signed).as_bytes(), signed.to_string().as_bytes());
      prop_assert_eq!(Decimal::unsigned(unsigned).as_bytes(), unsigned.to_string().as_bytes());
    }
  }
}
