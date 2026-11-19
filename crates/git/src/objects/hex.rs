//! An object id written as text where fsck reads one (A-590): git's `parse_oid_hex`, the repository's hexadecimal
//! digits of either case (`hexval`), then the byte that must follow them.

use crate::object_name::ObjectFormat;

/// The id `text` begins with when it is the format's count of hexadecimal digits followed by `then` (git's
/// `parse_oid_hex(buffer, &oid, &p) || *p != then`, inverted): the digits; none otherwise.
pub(crate) fn id_then(text: &[u8], format: ObjectFormat, then: u8) -> Option<&[u8]> {
  let digits = text.get(..format.hex_len())?;
  (digits.iter().all(u8::is_ascii_hexdigit) && text.get(format.hex_len()) == Some(&then))
    .then_some(digits)
}
