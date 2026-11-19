//! Where an encode writes (A-514): the output itself, or a running count of its length, so a caller can charge an
//! encode's exact size before it makes it.

use crate::read::to_u64;

/// A destination for encoded bytes.
pub(crate) trait Out {
  /// Writes `bytes`.
  fn put(&mut self, bytes: &[u8]);
}

impl Out for Vec<u8> {
  fn put(&mut self, bytes: &[u8]) {
    self.extend_from_slice(bytes);
  }
}

/// Counts the bytes an encode would write.
pub(crate) struct Measure(pub(crate) u64);

impl Out for Measure {
  fn put(&mut self, bytes: &[u8]) {
    self.0 = self.0.saturating_add(to_u64(bytes.len()));
  }
}
