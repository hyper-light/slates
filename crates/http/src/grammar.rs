//! The byte classes of HTTP's grammar that more than one reader here needs: tokens (RFC 9110 §5.6.2), field values
//! (§5.5), optional whitespace (§5.6.3), lists (§5.6.1), and the characters of a URI (RFC 3986 §2).

/// Format: hexadecimal's radix, read by a chunk's size (RFC 9112 §7.1: `chunk-size = 1*HEXDIG`) and a percent-escape
/// (RFC 3986 §2.1: `pct-encoded = "%" HEXDIG HEXDIG`).
pub(crate) const HEX_RADIX: u32 = 16;

/// Format: the characters of a token besides letters and digits (RFC 9110 §5.6.2: `tchar = "!" / "#" / "$" / "%" /
/// "&" / "'" / "*" / "+" / "-" / "." / "^" / "_" / "`" / "|" / "~" / DIGIT / ALPHA`).
const TCHAR_MARKS: &[u8] = b"!#$%&'*+-.^_`|~";

/// Format: the characters of a URI's path besides letters and digits (RFC 3986 §3.3: `pchar = unreserved /
/// pct-encoded / sub-delims / ":" / "@"`, with `unreserved` adding `-._~`, `sub-delims` being `!$&'()*+,;=`, and `/`
/// between segments); `%` is read apart, since two hexadecimal digits must follow it.
const PATH_MARKS: &[u8] = b"-._~!$&'()*+,;=:@/";

/// Format: the characters of an authority besides letters, digits and escapes (RFC 3986 §3.2: `reg-name`'s unreserved
/// and sub-delims, `:` before a port, and `[` `]` around an IP literal); `@`, which would open a userinfo, is not one.
const AUTHORITY_MARKS: &[u8] = b"-._~!$&'()*+,;=:[]";

/// Whether `byte` is a `tchar`.
pub(crate) fn is_tchar(byte: u8) -> bool {
  byte.is_ascii_alphanumeric() || TCHAR_MARKS.contains(&byte)
}

/// Whether `bytes` is a token: one `tchar` or more.
pub(crate) fn is_token(bytes: &[u8]) -> bool {
  !bytes.is_empty() && bytes.iter().all(|byte| is_tchar(*byte))
}

/// Whether `byte` may stand in a field value: anything but a control byte, HTAB excepted (RFC 9110 §5.5: `field-vchar
/// = VCHAR / obs-text`, with SP and HTAB between). CR, LF and NUL are "invalid and dangerous" (§5.5), and the other
/// control bytes have no meaning in a value.
pub(crate) fn is_field_byte(byte: u8) -> bool {
  byte == b'\t' || !byte.is_ascii_control()
}

/// Whether `byte` is optional whitespace (RFC 9110 §5.6.3: `OWS = *( SP / HTAB )`).
pub(crate) fn is_ows(byte: u8) -> bool {
  byte == b' ' || byte == b'\t'
}

/// `bytes` without its leading and trailing optional whitespace.
pub(crate) fn trim_ows(bytes: &[u8]) -> &[u8] {
  let start = bytes
    .iter()
    .position(|byte| !is_ows(*byte))
    .unwrap_or(bytes.len());
  let end = bytes
    .iter()
    .rposition(|byte| !is_ows(*byte))
    .map_or(start, |last| last.saturating_add(1));
  bytes.get(start..end).unwrap_or_default()
}

/// The elements of a comma-separated list (RFC 9110 §5.6.1), each without its surrounding whitespace, the empty ones
/// dropped ("a recipient MUST parse and ignore a reasonable number of empty list elements").
pub(crate) fn list_elements(value: &[u8]) -> impl Iterator<Item = &[u8]> {
  value
    .split(|byte| *byte == b',')
    .map(trim_ows)
    .filter(|element| !element.is_empty())
}

/// Whether `bytes` is made of URI characters of the kind `marks` adds to letters and digits, each `%` followed by two
/// hexadecimal digits (RFC 3986 §2.1).
fn is_uri_part(bytes: &[u8], marks: &[u8]) -> bool {
  let mut rest = bytes.iter();
  while let Some(&byte) = rest.next() {
    let allowed = match byte {
      b'%' => matches!(
        (rest.next(), rest.next()),
        (Some(high), Some(low)) if high.is_ascii_hexdigit() && low.is_ascii_hexdigit()
      ),
      other => other.is_ascii_alphanumeric() || marks.contains(&other),
    };
    if !allowed {
      return false;
    }
  }
  true
}

/// Whether `bytes` is a path of URI characters (RFC 3986 §3.3).
pub(crate) fn is_path(bytes: &[u8]) -> bool {
  is_uri_part(bytes, PATH_MARKS)
}

/// Whether `bytes` is a query of URI characters (RFC 3986 §3.4: `query = *( pchar / "/" / "?" )`).
pub(crate) fn is_query(bytes: &[u8]) -> bool {
  bytes
    .split(|byte| *byte == b'?')
    .all(|part| is_uri_part(part, PATH_MARKS))
}

/// Whether `bytes` is an authority without a userinfo (RFC 3986 §3.2; RFC 9110 §4.2.4: a recipient "SHOULD parse for
/// userinfo and treat its presence as an error").
pub(crate) fn is_authority(bytes: &[u8]) -> bool {
  is_uri_part(bytes, AUTHORITY_MARKS)
}
