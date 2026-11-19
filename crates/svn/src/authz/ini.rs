//! The authz file's lines (A-606): not built yet; the tests come first.

/// The NUL byte.
/// Format: ASCII 0.
pub(super) const NUL: u8 = 0;

/// The line the byte at `at` stands on: not built yet.
pub(super) fn line_of(_bytes: &[u8], _at: usize) -> usize {
  1
}
