//! A little-endian sequential reader and writer for the FUSE ABI (§4.6). Fields are read and
//! written in order, so a struct's layout is its field sequence and no byte offset appears as a
//! literal; the reader is bounds-checked, so a truncated message is a typed refusal, never an
//! out-of-bounds read. This is the shape record.rs uses for the op log, applied to the kernel
//! ABI.

use crate::error::FuseError;

/// A cursor over a request body, reading fields in order.
pub struct Reader<'a> {
  bytes: &'a [u8],
  at: usize,
}

impl<'a> Reader<'a> {
  /// A reader over `bytes`.
  pub fn new(bytes: &'a [u8]) -> Reader<'a> {
    Reader { bytes, at: 0 }
  }

  /// The bytes still unread.
  pub fn remaining(&self) -> usize {
    self.bytes.len().saturating_sub(self.at)
  }

  fn take(&mut self, n: usize, opcode: u32) -> Result<&'a [u8], FuseError> {
    let end = self.at.saturating_add(n);
    if end > self.bytes.len() {
      return Err(FuseError::ShortBody {
        opcode,
        have: self.bytes.len(),
        need: end,
      });
    }
    let slice = &self.bytes[self.at..end];
    self.at = end;
    Ok(slice)
  }

  /// Reads the next `u32`.
  pub fn u32(&mut self, opcode: u32) -> Result<u32, FuseError> {
    let b = self.take(size_of::<u32>(), opcode)?;
    Ok(u32::from_le_bytes(b.try_into().unwrap_or_default()))
  }

  /// Reads the next `u64`.
  pub fn u64(&mut self, opcode: u32) -> Result<u64, FuseError> {
    let b = self.take(size_of::<u64>(), opcode)?;
    Ok(u64::from_le_bytes(b.try_into().unwrap_or_default()))
  }

  /// Skips `n` bytes (padding or fields slates does not use).
  pub fn skip(&mut self, n: usize, opcode: u32) -> Result<(), FuseError> {
    self.take(n, opcode).map(|_| ())
  }

  /// The rest of the body, unread.
  pub fn rest(&self) -> &'a [u8] {
    &self.bytes[self.at.min(self.bytes.len())..]
  }
}

/// A buffer that appends fields in order.
#[derive(Default)]
pub struct Writer {
  bytes: Vec<u8>,
}

impl Writer {
  /// An empty writer.
  pub fn new() -> Writer {
    Writer { bytes: Vec::new() }
  }

  /// Appends a `u32`.
  pub fn u32(&mut self, value: u32) {
    self.bytes.extend_from_slice(&value.to_le_bytes());
  }

  /// Appends a `u64`.
  pub fn u64(&mut self, value: u64) {
    self.bytes.extend_from_slice(&value.to_le_bytes());
  }

  /// Appends `n` zero bytes (padding or reserved fields).
  pub fn pad(&mut self, n: usize) {
    self.bytes.resize(self.bytes.len() + n, 0);
  }

  /// Appends raw bytes (a name, or nested data).
  pub fn bytes(&mut self, data: &[u8]) {
    self.bytes.extend_from_slice(data);
  }

  /// The accumulated bytes.
  pub fn into_bytes(self) -> Vec<u8> {
    self.bytes
  }

  /// The bytes so far.
  pub fn as_bytes(&self) -> &[u8] {
    &self.bytes
  }
}
