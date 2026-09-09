//! External Data Representation (XDR, RFC 4506): the wire encoding ONC RPC and NFSv3 are built on.
//! Everything is big-endian and padded so every field starts on a four-byte boundary; a variable
//! opaque or string is a four-byte length then the bytes then the padding. This module is a
//! bounds-checked sequential writer and reader over byte buffers — no allocation on a declared
//! length before it is checked against a caller-supplied cap and the bytes that remain, so a hostile
//! length is a typed refusal, never an allocation or a panic (§4.9's discipline, reused here).

/// A refusal from the XDR reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XdrError {
  /// The buffer ends before a field the caller asked for.
  Truncated,
  /// A declared length is larger than the caller's cap or the bytes that remain, or a string is not
  /// valid UTF-8.
  BadLength,
}

/// Format: XDR aligns every field to a four-byte boundary (RFC 4506).
const XDR_ALIGN: usize = 4;

/// The alignment padding for a field of `len` bytes.
fn padding(len: usize) -> usize {
  (XDR_ALIGN - (len % XDR_ALIGN)) % XDR_ALIGN
}

/// A sequential XDR writer building a big-endian, four-byte-aligned byte buffer.
#[derive(Debug, Default)]
pub struct XdrWriter {
  out: Vec<u8>,
}

impl XdrWriter {
  /// An empty writer.
  pub fn new() -> XdrWriter {
    XdrWriter::default()
  }

  /// Writes a 32-bit unsigned integer.
  pub fn u32(&mut self, value: u32) {
    self.out.extend_from_slice(&value.to_be_bytes());
  }

  /// Writes a 32-bit signed integer.
  pub fn i32(&mut self, value: i32) {
    self.out.extend_from_slice(&value.to_be_bytes());
  }

  /// Writes a 64-bit unsigned integer.
  pub fn u64(&mut self, value: u64) {
    self.out.extend_from_slice(&value.to_be_bytes());
  }

  /// Writes a boolean as a 32-bit `1` or `0`.
  pub fn bool(&mut self, value: bool) {
    self.u32(u32::from(value));
  }

  /// Writes a variable-length opaque: a four-byte length, the bytes, then padding.
  pub fn opaque(&mut self, bytes: &[u8]) {
    self.u32(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
    self.fixed(bytes);
  }

  /// Writes fixed-length bytes (the length is fixed by the type, not sent) followed by padding.
  pub fn fixed(&mut self, bytes: &[u8]) {
    self.out.extend_from_slice(bytes);
    self
      .out
      .extend(std::iter::repeat_n(0u8, padding(bytes.len())));
  }

  /// The bytes written so far.
  pub fn as_slice(&self) -> &[u8] {
    &self.out
  }

  /// The number of bytes written so far.
  pub fn len(&self) -> usize {
    self.out.len()
  }

  /// Whether nothing has been written.
  pub fn is_empty(&self) -> bool {
    self.out.is_empty()
  }

  /// The finished buffer.
  pub fn into_bytes(self) -> Vec<u8> {
    self.out
  }
}

/// A sequential XDR reader over a byte buffer, bounds-checked.
#[derive(Debug)]
pub struct XdrReader<'a> {
  bytes: &'a [u8],
  pos: usize,
}

impl<'a> XdrReader<'a> {
  /// A reader over `bytes`.
  pub fn new(bytes: &'a [u8]) -> XdrReader<'a> {
    XdrReader { bytes, pos: 0 }
  }

  /// The bytes not yet read.
  pub fn remaining(&self) -> usize {
    self.bytes.len().saturating_sub(self.pos)
  }

  /// The reader's position.
  pub fn position(&self) -> usize {
    self.pos
  }

  /// The bytes not yet read, so a caller can peek a leading field through a fresh reader without
  /// consuming this one (the multi-volume router reads the leading file handle to route, then hands
  /// the untouched reader to the chosen volume's export).
  pub fn rest(&self) -> &'a [u8] {
    self.bytes.get(self.pos..).unwrap_or(&[])
  }

  /// Takes `count` bytes, or refuses when the buffer is too short.
  fn take(&mut self, count: usize) -> Result<&'a [u8], XdrError> {
    let end = self.pos.checked_add(count).ok_or(XdrError::BadLength)?;
    let slice = self.bytes.get(self.pos..end).ok_or(XdrError::Truncated)?;
    self.pos = end;
    Ok(slice)
  }

  /// Reads a 32-bit unsigned integer.
  pub fn u32(&mut self) -> Result<u32, XdrError> {
    let word = self.take(size_of::<u32>())?;
    Ok(u32::from_be_bytes(
      word.try_into().map_err(|_| XdrError::Truncated)?,
    ))
  }

  /// Reads a 32-bit signed integer.
  pub fn i32(&mut self) -> Result<i32, XdrError> {
    let word = self.take(size_of::<i32>())?;
    Ok(i32::from_be_bytes(
      word.try_into().map_err(|_| XdrError::Truncated)?,
    ))
  }

  /// Reads a 64-bit unsigned integer.
  pub fn u64(&mut self) -> Result<u64, XdrError> {
    let word = self.take(size_of::<u64>())?;
    Ok(u64::from_be_bytes(
      word.try_into().map_err(|_| XdrError::Truncated)?,
    ))
  }

  /// Reads a boolean (any nonzero 32-bit value is true).
  pub fn bool(&mut self) -> Result<bool, XdrError> {
    Ok(self.u32()? != 0)
  }

  /// Reads fixed-length bytes and skips the padding.
  pub fn fixed(&mut self, count: usize) -> Result<&'a [u8], XdrError> {
    let slice = self.take(count)?;
    let pad = padding(count);
    self.pos = self.pos.saturating_add(pad).min(self.bytes.len());
    Ok(slice)
  }

  /// Reads a variable-length opaque, refusing a length past `max` or the bytes that remain before
  /// allocating for it.
  pub fn opaque(&mut self, max: usize) -> Result<&'a [u8], XdrError> {
    let len = usize::try_from(self.u32()?).map_err(|_| XdrError::BadLength)?;
    if len > max || len > self.remaining() {
      return Err(XdrError::BadLength);
    }
    self.fixed(len)
  }

  /// Reads a variable-length string (a variable opaque validated as UTF-8), capped at `max`.
  pub fn string(&mut self, max: usize) -> Result<&'a str, XdrError> {
    let bytes = self.opaque(max)?;
    std::str::from_utf8(bytes).map_err(|_| XdrError::BadLength)
  }
}
