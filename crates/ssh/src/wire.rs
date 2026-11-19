//! SSH's data types (RFC 4251 §5), read from untrusted bytes and written for the peer (A-594).
//!
//! A [`Reader`] walks one message's payload. Every length is checked against the bytes left before anything past it is
//! read, and nothing is copied: strings and name-lists borrow the payload. [`Reader::finish`] refuses bytes left after
//! a message's last field, as OpenSSH's `sshpkt_get_end` does [C]. The writers append to a buffer and refuse only a
//! string whose length does not fit the format's `uint32`.
//!
//! A name-list is checked as RFC 4251 §5 and §6 define a name: at least one byte, at most 64, printable US-ASCII with no
//! comma and no space. An empty list is a list of no names, as §5 allows. An `mpint` is written from an unsigned
//! big-endian magnitude, the only form the key exchange needs (RFC 8731 §3.1): its leading zero bytes dropped, and a zero
//! byte put first when the top bit is set, so the value reads as positive.

/// The longest name RFC 4251 §6 allows: names "MUST NOT be longer than 64 characters".
pub const MAX_NAME: usize = 64;

/// The bytes of a `uint32`, and of the length that leads a `string` (RFC 4251 §5).
pub const UINT32_LEN: usize = 4;

/// Why bytes did not read, or a value did not write, as SSH's types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireRefusal {
  /// The bytes ended before the value did; `at` is where the value started.
  Truncated {
    /// The offset of the value that ran past the end.
    at: usize,
  },
  /// A name-list held an empty name, a name past [`MAX_NAME`] bytes, or a byte outside printable US-ASCII.
  BadNameList {
    /// The offset of the name-list's first name byte.
    at: usize,
  },
  /// Bytes were left after the message's last field.
  TrailingBytes {
    /// The offset of the first byte left over.
    at: usize,
  },
  /// A string to write is longer than a `uint32` can say.
  TooLong {
    /// The string's length.
    length: usize,
  },
}

/// A cursor over one message's payload.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
  /// The payload.
  bytes: &'a [u8],
  /// The offset of the next byte to read.
  at: usize,
}

impl<'a> Reader<'a> {
  /// A reader at the start of `bytes`.
  pub fn new(bytes: &'a [u8]) -> Self {
    Self { bytes, at: 0 }
  }

  /// The offset of the next byte to read.
  pub fn position(&self) -> usize {
    self.at
  }

  /// The next `count` bytes, borrowed.
  pub fn take(&mut self, count: usize) -> Result<&'a [u8], WireRefusal> {
    let start = self.at;
    let end = start
      .checked_add(count)
      .ok_or(WireRefusal::Truncated { at: start })?;
    let taken = self
      .bytes
      .get(start..end)
      .ok_or(WireRefusal::Truncated { at: start })?;
    self.at = end;
    Ok(taken)
  }

  /// A `byte`.
  pub fn byte(&mut self) -> Result<u8, WireRefusal> {
    let at = self.at;
    let taken = self.take(1)?;
    taken.first().copied().ok_or(WireRefusal::Truncated { at })
  }

  /// A `boolean`: RFC 4251 §5, "All non-zero values MUST be interpreted as TRUE".
  pub fn boolean(&mut self) -> Result<bool, WireRefusal> {
    Ok(self.byte()? != 0)
  }

  /// A `uint32`, most significant byte first.
  pub fn uint32(&mut self) -> Result<u32, WireRefusal> {
    let at = self.at;
    let taken = self.take(UINT32_LEN)?;
    let word: [u8; UINT32_LEN] = taken
      .try_into()
      .map_err(|_| WireRefusal::Truncated { at })?;
    Ok(u32::from_be_bytes(word))
  }

  /// A `string`: a `uint32` length, then that many bytes, borrowed. The length is checked against the bytes left before
  /// any is read, so a length of `u32::MAX` costs nothing.
  pub fn string(&mut self) -> Result<&'a [u8], WireRefusal> {
    let at = self.at;
    let length = self.uint32()?;
    let length = usize::try_from(length).map_err(|_| WireRefusal::Truncated { at })?;
    self.take(length).map_err(|_| WireRefusal::Truncated { at })
  }

  /// A `name-list`, its names checked (RFC 4251 §5, §6).
  pub fn name_list(&mut self) -> Result<NameList<'a>, WireRefusal> {
    let bytes = self.string()?;
    let at = self.at.saturating_sub(bytes.len());
    NameList::parse(bytes, at)
  }

  /// Ends the message: refuses bytes left after its last field.
  pub fn finish(self) -> Result<(), WireRefusal> {
    if self.at < self.bytes.len() {
      return Err(WireRefusal::TrailingBytes { at: self.at });
    }
    Ok(())
  }
}

/// A checked name-list, borrowed from its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameList<'a> {
  /// The list's bytes: names separated by commas, each checked.
  bytes: &'a [u8],
}

impl<'a> NameList<'a> {
  /// Checks `bytes` as a name-list whose first byte sits at `at` in its payload.
  fn parse(bytes: &'a [u8], at: usize) -> Result<Self, WireRefusal> {
    if bytes.is_empty() {
      return Ok(Self { bytes });
    }
    if bytes.split(|byte| *byte == b',').all(is_name) {
      return Ok(Self { bytes });
    }
    Err(WireRefusal::BadNameList { at })
  }

  /// The names, in the sender's order of preference.
  pub fn names(&self) -> impl Iterator<Item = &'a [u8]> + use<'a> {
    let bytes = self.bytes;
    bytes
      .split(|byte| *byte == b',')
      .filter(|name| !name.is_empty())
  }

  /// Whether the list names `name`.
  pub fn contains(&self, name: &str) -> bool {
    self.names().any(|listed| listed == name.as_bytes())
  }

  /// The first name: the sender's preferred one (RFC 4253 §7.1).
  pub fn first(&self) -> Option<&'a [u8]> {
    self.names().next()
  }

  /// Whether the list names nothing.
  pub fn is_empty(&self) -> bool {
    self.bytes.is_empty()
  }
}

/// Whether `name` is a name RFC 4251 §6 allows: one to [`MAX_NAME`] printable US-ASCII bytes, no comma, no space.
fn is_name(name: &[u8]) -> bool {
  !name.is_empty()
    && name.len() <= MAX_NAME
    && name
      .iter()
      .all(|byte| byte.is_ascii_graphic() && *byte != b',')
}

/// Appends a `byte`.
pub fn put_byte(out: &mut Vec<u8>, value: u8) {
  out.push(value);
}

/// Appends a `boolean` as 0 or 1, the only values RFC 4251 §5 lets a sender store.
pub fn put_boolean(out: &mut Vec<u8>, value: bool) {
  out.push(u8::from(value));
}

/// Appends a `uint32`.
pub fn put_uint32(out: &mut Vec<u8>, value: u32) {
  out.extend_from_slice(&value.to_be_bytes());
}

/// Appends a `string`.
pub fn put_string(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), WireRefusal> {
  let length = u32::try_from(bytes.len()).map_err(|_| WireRefusal::TooLong {
    length: bytes.len(),
  })?;
  put_uint32(out, length);
  out.extend_from_slice(bytes);
  Ok(())
}

/// Appends a `name-list` of `names`, which the caller holds to RFC 4251 §6.
pub fn put_name_list(out: &mut Vec<u8>, names: &[&str]) -> Result<(), WireRefusal> {
  let joined = names.join(",");
  put_string(out, joined.as_bytes())
}

/// Appends an `mpint` holding the unsigned big-endian `magnitude` (RFC 4251 §5; RFC 8731 §3.1).
pub fn put_mpint_unsigned(out: &mut Vec<u8>, magnitude: &[u8]) -> Result<(), WireRefusal> {
  let significant = magnitude
    .iter()
    .position(|byte| *byte != 0)
    .map_or(&[][..], |first| magnitude.get(first..).unwrap_or(&[]));
  let sign_byte = significant.first().is_some_and(|byte| byte & 0x80 != 0);
  let length = significant
    .len()
    .checked_add(usize::from(sign_byte))
    .ok_or(WireRefusal::TooLong {
      length: significant.len(),
    })?;
  let length = u32::try_from(length).map_err(|_| WireRefusal::TooLong { length })?;
  put_uint32(out, length);
  if sign_byte {
    out.push(0);
  }
  out.extend_from_slice(significant);
  Ok(())
}
