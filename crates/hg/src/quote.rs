//! Percent-encoding as Python's `urllib.parse` writes and reads it, on bytes (A-569; A-514's stream parameters, moved
//! here): `quote` and `unquote_to_bytes`, which Mercurial uses for bundle2's stream parameters and capability blob
//! (`urlreq.quote`, `safe='/'`), and the form encoding of HTTP arguments (`urlencode`, whose `quote_plus` writes a
//! space as `+`, and `parse_qs`, which reads one so). Everything writes through [`Out`], so a caller measures a
//! result's exact length before it allocates.

use crate::out::Out;

/// Format: hexadecimal, the base of an escape's two digits (RFC 3986 §2.1; Python writes them upper-case).
const HEX_RADIX: u32 = 16;

/// Format: the byte that begins an escape.
const ESCAPE: u8 = b'%';

/// Format: the form encoding's space (`quote_plus`, `parse_qsl`).
const PLUS: u8 = b'+';

/// Format: the space a form's `+` stands for.
const SPACE: u8 = b' ';

/// Format: the bytes `quote(s, safe='/')` leaves as they are besides letters and digits: Python's always-safe `_.-~`
/// and the `/` Mercurial's `urlreq.quote` keeps.
pub(crate) const PATH_SAFE: &[u8] = b"_.-~/";

/// Format: the bytes `urlencode` (`quote_plus(s, safe='')`) leaves as they are besides letters and digits.
pub(crate) const FORM_SAFE: &[u8] = b"_.-~";

/// Appends `raw` quoted: letters, digits and `safe` as they are, every other byte as `%XX`.
pub(crate) fn quote(raw: &[u8], safe: &[u8], out: &mut impl Out) {
  for &byte in raw {
    if byte.is_ascii_alphanumeric() || safe.contains(&byte) {
      out.put(&[byte]);
    } else {
      put_escape(byte, out);
    }
  }
}

/// Appends `raw` as `urlencode` writes a key or a value: a space as `+`, every other byte as [`quote`] writes it
/// with [`FORM_SAFE`].
pub(crate) fn quote_plus(raw: &[u8], out: &mut impl Out) {
  for &byte in raw {
    if byte == SPACE {
      out.put(&[PLUS]);
    } else if byte.is_ascii_alphanumeric() || FORM_SAFE.contains(&byte) {
      out.put(&[byte]);
    } else {
      put_escape(byte, out);
    }
  }
}

/// Writes one byte as `%XX`, its digits upper-case.
fn put_escape(byte: u8, out: &mut impl Out) {
  let digit = |value: u32| {
    char::from_digit(value, HEX_RADIX)
      .and_then(|character| u8::try_from(character.to_ascii_uppercase()).ok())
      .unwrap_or(ESCAPE)
  };
  out.put(&[
    ESCAPE,
    digit(u32::from(byte) / HEX_RADIX),
    digit(u32::from(byte) % HEX_RADIX),
  ]);
}

/// Appends `quoted` read as `unquote_to_bytes` reads it: `%` and two hexadecimal digits is that byte; any other `%`
/// stays as it is.
pub(crate) fn unquote(quoted: &[u8], out: &mut impl Out) {
  unquote_with(quoted, false, out);
}

/// Appends `quoted` read as `parse_qsl` reads a key or a value: each `+` a space, then [`unquote`].
pub(crate) fn unquote_plus(quoted: &[u8], out: &mut impl Out) {
  unquote_with(quoted, true, out);
}

/// [`unquote`], with `+` read as a space when `plus` is set.
fn unquote_with(quoted: &[u8], plus: bool, out: &mut impl Out) {
  let mut rest = quoted;
  while let Some((&byte, tail)) = rest.split_first() {
    let escaped = (byte == ESCAPE)
      .then(|| tail.get(..2).and_then(hex_pair))
      .flatten();
    match escaped {
      Some(value) => {
        out.put(&[value]);
        rest = tail.get(2..).unwrap_or_default();
      }
      None => {
        out.put(&[if plus && byte == PLUS { SPACE } else { byte }]);
        rest = tail;
      }
    }
  }
}

/// Two hexadecimal digits, in either case, as the byte they name.
pub(crate) fn hex_pair(pair: &[u8]) -> Option<u8> {
  let mut value: u32 = 0;
  for digit in pair {
    let digit = char::from(*digit).to_digit(HEX_RADIX)?;
    value = value.checked_mul(HEX_RADIX)?.checked_add(digit)?;
  }
  u8::try_from(value).ok()
}

#[cfg(test)]
mod tests {
  use super::{FORM_SAFE, PATH_SAFE, quote, quote_plus, unquote, unquote_plus};
  use proptest::prelude::*;

  /// A-569: do quote bytes as Python's `quote(s, safe='/')` and `urlencode`'s `quote_plus` do. Expect letters, digits
  /// and `_.-~` kept, `/` kept only by the first, a space as `%20` and as `+`, and every other byte `%XX` upper-case.
  #[test]
  fn bytes_are_quoted_as_python_quotes_them() {
    let raw = b"a b/c=d,e\n\xc3\xa9~";
    let mut path = Vec::new();
    quote(raw, PATH_SAFE, &mut path);
    assert_eq!(path, b"a%20b/c%3Dd%2Ce%0A%C3%A9~");
    let mut form = Vec::new();
    quote_plus(raw, &mut form);
    assert_eq!(form, b"a+b%2Fc%3Dd%2Ce%0A%C3%A9~");
    let mut kept = Vec::new();
    quote(b"AZaz09_.-~", FORM_SAFE, &mut kept);
    assert_eq!(kept, b"AZaz09_.-~");
  }

  /// AC-HG.9 (hostile input), A-569: do unquote escapes in either case, a `%` without two hexadecimal digits after it,
  /// and `+`. Expect the escapes read as bytes, the lone `%` kept, and `+` a space only for the form reading.
  #[test]
  fn escapes_unquote_to_bytes_and_a_broken_one_stays() {
    let mut plain = Vec::new();
    unquote(b"%41%e9%zz%4+%", &mut plain);
    assert_eq!(plain, b"A\xe9%zz%4+%");
    let mut form = Vec::new();
    unquote_plus(b"a+b%2B", &mut form);
    assert_eq!(form, b"a b+");
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-569: do quote arbitrary bytes both ways and read them back. Expect the same bytes.
    #[test]
    fn quoting_reads_back_to_the_same_bytes(raw in proptest::collection::vec(any::<u8>(), 0..64)) {
      let mut quoted = Vec::new();
      quote(&raw, PATH_SAFE, &mut quoted);
      let mut back = Vec::new();
      unquote(&quoted, &mut back);
      prop_assert_eq!(&back, &raw);
      let mut form = Vec::new();
      quote_plus(&raw, &mut form);
      let mut form_back = Vec::new();
      unquote_plus(&form, &mut form_back);
      prop_assert_eq!(form_back, raw);
    }
  }
}
