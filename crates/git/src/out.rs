//! Where an encode writes (A-582, as slates-hg's): the output itself, or a running count of its length, so a caller
//! can charge an encode's exact size against its budget before it makes the bytes.

/// A destination for encoded bytes.
pub trait Out {
  /// Writes `bytes`.
  fn put(&mut self, bytes: &[u8]);
}

impl Out for Vec<u8> {
  fn put(&mut self, bytes: &[u8]) {
    self.extend_from_slice(bytes);
  }
}

/// Counts the bytes an encode would write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Measure(pub u64);

impl Out for Measure {
  fn put(&mut self, bytes: &[u8]) {
    self.0 = self.0.saturating_add(crate::to_u64(bytes.len()));
  }
}
