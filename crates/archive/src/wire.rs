//! A bounds-checked sequential codec for the archive format (§2.6). Fields are written and read
//! in order through a [`Writer`] that appends and a [`Reader`] that advances a cursor, so no byte
//! offset appears as a literal and a truncated or oversized stream is a typed refusal, never a
//! panic or an out-of-bounds read (the design's hostile-input rule, §4.9). Little-endian
//! throughout; no `unsafe`, no cast of a byte slice to a struct.

use crate::format::ArchiveError;

/// Appends little-endian fields to a growing byte buffer.
#[derive(Debug, Default)]
pub struct Writer {
  bytes: Vec<u8>,
}

impl Writer {
  /// An empty writer.
  pub fn new() -> Writer {
    Writer { bytes: Vec::new() }
  }

  /// The bytes written so far.
  pub fn position(&self) -> u64 {
    self.bytes.len() as u64
  }

  /// The finished bytes.
  pub fn finish(self) -> Vec<u8> {
    self.bytes
  }

  /// A borrowed view of the bytes written so far (for hashing a prefix).
  pub fn as_slice(&self) -> &[u8] {
    &self.bytes
  }

  /// Writes one byte.
  pub fn u8(&mut self, value: u8) {
    self.bytes.push(value);
  }

  /// Writes a little-endian `u16`.
  pub fn u16(&mut self, value: u16) {
    self.bytes.extend_from_slice(&value.to_le_bytes());
  }

  /// Writes a little-endian `u32`.
  pub fn u32(&mut self, value: u32) {
    self.bytes.extend_from_slice(&value.to_le_bytes());
  }

  /// Writes a little-endian `u64`.
  pub fn u64(&mut self, value: u64) {
    self.bytes.extend_from_slice(&value.to_le_bytes());
  }

  /// Writes a 32-byte identity (a BLAKE3 hash).
  pub fn hash(&mut self, value: &[u8; 32]) {
    self.bytes.extend_from_slice(value);
  }

  /// Writes raw bytes.
  pub fn raw(&mut self, value: &[u8]) {
    self.bytes.extend_from_slice(value);
  }
}

/// Reads little-endian fields from a byte buffer, refusing to read past its end.
#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
  bytes: &'a [u8],
  cursor: usize,
}

impl<'a> Reader<'a> {
  /// A reader over `bytes`.
  pub fn new(bytes: &'a [u8]) -> Reader<'a> {
    Reader { bytes, cursor: 0 }
  }

  /// A reader starting at `offset`, or a truncation refusal when the offset is past the end.
  pub fn at(bytes: &'a [u8], offset: u64) -> Result<Reader<'a>, ArchiveError> {
    let cursor = usize::try_from(offset).unwrap_or(usize::MAX);
    if cursor > bytes.len() {
      return Err(ArchiveError::Truncated);
    }
    Ok(Reader { bytes, cursor })
  }

  /// The current offset.
  pub fn position(&self) -> u64 {
    self.cursor as u64
  }

  /// The bytes not yet read.
  pub fn remaining(&self) -> usize {
    self.bytes.len().saturating_sub(self.cursor)
  }

  /// Advances past `count` bytes, returning them, or refuses when they run past the end.
  fn take(&mut self, count: usize) -> Result<&'a [u8], ArchiveError> {
    let end = self
      .cursor
      .checked_add(count)
      .ok_or(ArchiveError::Truncated)?;
    let slice = self
      .bytes
      .get(self.cursor..end)
      .ok_or(ArchiveError::Truncated)?;
    self.cursor = end;
    Ok(slice)
  }

  /// Reads one byte.
  pub fn u8(&mut self) -> Result<u8, ArchiveError> {
    Ok(self.take(size_of::<u8>())?[0])
  }

  /// Reads a little-endian `u16`.
  pub fn u16(&mut self) -> Result<u16, ArchiveError> {
    let bytes = self.take(size_of::<u16>())?;
    Ok(u16::from_le_bytes(bytes.try_into().unwrap_or_default()))
  }

  /// Reads a little-endian `u32`.
  pub fn u32(&mut self) -> Result<u32, ArchiveError> {
    let bytes = self.take(size_of::<u32>())?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap_or_default()))
  }

  /// Reads a little-endian `u64`.
  pub fn u64(&mut self) -> Result<u64, ArchiveError> {
    let bytes = self.take(size_of::<u64>())?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap_or_default()))
  }

  /// Reads a 32-byte identity.
  pub fn hash(&mut self) -> Result<[u8; 32], ArchiveError> {
    let bytes = self.take(size_of::<[u8; 32]>())?;
    Ok(bytes.try_into().unwrap_or_default())
  }

  /// Reads `count` raw bytes.
  pub fn raw(&mut self, count: usize) -> Result<&'a [u8], ArchiveError> {
    self.take(count)
  }
}
