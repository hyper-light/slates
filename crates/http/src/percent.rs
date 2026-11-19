//! Percent-escapes decoded (RFC 3986 §2.1: `pct-encoded = "%" HEXDIG HEXDIG`), for a target's path and its query's form
//! fields. A path is read decoded, as a CGI program reads `PATH_INFO`; a query's names and values are read as an HTML
//! form's, `+` for a space, as `git http-backend` reads its query (`url_decode_parameter_name` and
//! `url_decode_parameter_value`). An escape cut short or not hexadecimal is refused; what an escape names is the
//! caller's to judge ([`names_any`]), since a path's router must refuse an escaped `/` that would split a segment.

use crate::grammar::HEX_RADIX;

/// A malformed escape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadEscape {
  /// A `%` with fewer than two bytes after it.
  Truncated,
  /// A `%` followed by a byte that is not a hexadecimal digit.
  NotHex,
}

/// How a `+` reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plus {
  /// As itself: a path's.
  Plus,
  /// As a space: a form field's.
  Space,
}

/// Format: the bits one hexadecimal digit holds; an escape's first digit is the byte's high four.
const NIBBLE: u32 = 4;

/// The number of bytes `raw` decodes to.
pub fn decoded_len(raw: &[u8]) -> Result<usize, BadEscape> {
  let mut count = 0usize;
  walk(raw, Plus::Plus, |_| count = count.saturating_add(1))?;
  Ok(count)
}

/// Appends the bytes `raw` decodes to onto `out`; nothing is appended when an escape is refused.
pub fn decode_into(raw: &[u8], plus: Plus, out: &mut Vec<u8>) -> Result<(), BadEscape> {
  let start = out.len();
  let walked = walk(raw, plus, |byte| out.push(byte));
  if walked.is_err() {
    out.truncate(start);
  }
  walked
}

/// Whether `raw` decodes to exactly `expected`, compared as it is decoded, with nothing allocated.
pub fn decodes_to(raw: &[u8], plus: Plus, expected: &[u8]) -> Result<bool, BadEscape> {
  let mut wanted = expected.iter();
  let mut same = true;
  walk(raw, plus, |byte| same &= wanted.next() == Some(&byte))?;
  Ok(same && wanted.next().is_none())
}

/// Whether an escape in `raw` names one of `bytes` (a `%2F` naming `/`, say).
pub fn names_any(raw: &[u8], bytes: &[u8]) -> bool {
  let mut rest = raw.iter().copied();
  while let Some(byte) = rest.next() {
    if byte == b'%' && escaped(rest.next(), rest.next()).is_ok_and(|named| bytes.contains(&named)) {
      return true;
    }
  }
  false
}

/// A query's form fields, `name=value` pairs split at `&`, each undecoded: a field with no `=` has an empty value, and
/// an empty field is skipped.
pub fn form_fields(query: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
  query
    .split(|byte| *byte == b'&')
    .filter(|field| !field.is_empty())
    .map(|field| match field.iter().position(|byte| *byte == b'=') {
      Some(at) => (
        field.get(..at).unwrap_or_default(),
        field.get(at.saturating_add(1)..).unwrap_or_default(),
      ),
      None => (field, field.get(field.len()..).unwrap_or_default()),
    })
}

/// Hands each byte `raw` decodes to to `emit`, in order; refused at the first malformed escape.
fn walk(raw: &[u8], plus: Plus, mut emit: impl FnMut(u8)) -> Result<(), BadEscape> {
  let mut rest = raw.iter().copied();
  while let Some(byte) = rest.next() {
    emit(match byte {
      b'%' => escaped(rest.next(), rest.next())?,
      b'+' if plus == Plus::Space => b' ',
      other => other,
    });
  }
  Ok(())
}

/// The byte an escape's two digits name.
fn escaped(high: Option<u8>, low: Option<u8>) -> Result<u8, BadEscape> {
  let (Some(high), Some(low)) = (high, low) else {
    return Err(BadEscape::Truncated);
  };
  match (hex_value(high), hex_value(low)) {
    (Some(high), Some(low)) => Ok(high.wrapping_shl(NIBBLE) | low),
    _ => Err(BadEscape::NotHex),
  }
}

/// The value of a hexadecimal digit of either case.
fn hex_value(digit: u8) -> Option<u8> {
  char::from(digit)
    .to_digit(HEX_RADIX)
    .and_then(|value| u8::try_from(value).ok())
}

#[cfg(test)]
#[path = "tests/percent.rs"]
mod tests;
