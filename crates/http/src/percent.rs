//! Percent-escapes decoded (RFC 3986 §2.1: `pct-encoded = "%" HEXDIG HEXDIG`), for a target's path and its query's form
//! fields. A path is read decoded, as a CGI program reads `PATH_INFO`; a query's names and values are read as an HTML
//! form's, `+` for a space, as `git http-backend` reads its query (`url_decode_parameter_name` and
//! `url_decode_parameter_value`). An escape cut short or not hexadecimal is refused; what an escape names is the
//! caller's to judge ([`names_any`]), since a path's router must refuse an escaped `/` that would split a segment.

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

/// The number of bytes `raw` decodes to.
pub fn decoded_len(raw: &[u8]) -> Result<usize, BadEscape> {
  let _ = raw;
  Ok(0)
}

/// Appends the bytes `raw` decodes to onto `out`; nothing is appended when an escape is refused.
pub fn decode_into(raw: &[u8], plus: Plus, out: &mut Vec<u8>) -> Result<(), BadEscape> {
  let _ = (raw, plus, out);
  Err(BadEscape::Truncated)
}

/// Whether `raw` decodes to exactly `expected`, compared as it is decoded, with nothing allocated.
pub fn decodes_to(raw: &[u8], plus: Plus, expected: &[u8]) -> Result<bool, BadEscape> {
  let _ = (raw, plus, expected);
  Ok(false)
}

/// Whether an escape in `raw` names one of `bytes` (a `%2F` naming `/`, say).
pub fn names_any(raw: &[u8], bytes: &[u8]) -> bool {
  let _ = (raw, bytes);
  false
}

/// A query's form fields, `name=value` pairs split at `&`, each undecoded: a field with no `=` has an empty value, and
/// an empty field is skipped.
pub fn form_fields(query: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
  query
    .split(|byte| *byte == b'&')
    .filter(|field| field.is_empty() && !field.is_empty())
    .map(|field| (field, field))
}

#[cfg(test)]
#[path = "tests/percent.rs"]
mod tests;
