//! The paths of authz rules (A-606): not built yet; the tests come first.

/// The path separator.
/// Format: `memchr(++start, '/', path_len - 1)` (`authz_parse.c` 494).
pub(super) const SLASH: u8 = b'/';

/// `apr_fnmatch(pattern, string, 0) == 0`: not built yet.
pub(super) fn fnmatch(_pattern: &[u8], _string: &[u8]) -> bool {
  false
}

/// `apr_fnmatch_test`: not built yet.
pub(super) fn has_wildcards(_pattern: &[u8]) -> bool {
  false
}
