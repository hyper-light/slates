//! A cursor over untrusted bytes (A-514; §4.9): every read checked against the bytes left, never an index, every
//! integer big-endian as Mercurial packs it (`struct.pack(">...")`).

use crate::{NODE_LEN, Node, Truncated};

/// Reads the bytes of one bundle, or of one block inside it, front to back. A clone reads on from the same place,
/// which is how a decode's counting pass walks what its building pass will.
#[derive(Clone)]
pub(crate) struct Cursor<'a> {
  /// The bytes being read.
  bytes: &'a [u8],
  /// How many of them have been read.
  at: usize,
}

impl<'a> Cursor<'a> {
  /// A cursor at the start of `bytes`.
  pub(crate) fn new(bytes: &'a [u8]) -> Self {
    Self { bytes, at: 0 }
  }

  /// How many bytes have been read.
  pub(crate) fn at(&self) -> usize {
    self.at
  }

  /// How many bytes are left.
  pub(crate) fn left(&self) -> usize {
    self.bytes.len().saturating_sub(self.at)
  }

  /// Whether every byte has been read.
  pub(crate) fn is_done(&self) -> bool {
    self.left() == 0
  }

  /// The next `len` bytes, or [`Truncated`] if fewer are left; nothing is read on a refusal.
  pub(crate) fn take(&mut self, len: usize, what: &'static str) -> Result<&'a [u8], Truncated> {
    let slice = self
      .at
      .checked_add(len)
      .and_then(|end| self.bytes.get(self.at..end));
    match slice {
      Some(slice) => {
        self.at = self.at.saturating_add(len);
        Ok(slice)
      }
      None => Err(self.truncated(len, what)),
    }
  }

  /// Every byte that is left.
  pub(crate) fn rest(&mut self) -> &'a [u8] {
    let rest = self.bytes.get(self.at..).unwrap_or_default();
    self.at = self.bytes.len();
    rest
  }

  /// The next `N` bytes as an array.
  pub(crate) fn array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N], Truncated> {
    let truncated = self.truncated(N, what);
    let slice = self.take(N, what)?;
    <[u8; N]>::try_from(slice).map_err(|_| truncated)
  }

  /// One byte.
  pub(crate) fn u8(&mut self, what: &'static str) -> Result<u8, Truncated> {
    Ok(u8::from_be_bytes(self.array(what)?))
  }

  /// A signed byte.
  pub(crate) fn i8(&mut self, what: &'static str) -> Result<i8, Truncated> {
    Ok(i8::from_be_bytes(self.array(what)?))
  }

  /// A big-endian unsigned 16-bit integer.
  pub(crate) fn u16(&mut self, what: &'static str) -> Result<u16, Truncated> {
    Ok(u16::from_be_bytes(self.array(what)?))
  }

  /// A big-endian unsigned 32-bit integer.
  pub(crate) fn u32(&mut self, what: &'static str) -> Result<u32, Truncated> {
    Ok(u32::from_be_bytes(self.array(what)?))
  }

  /// A big-endian signed 32-bit integer: how Mercurial reads every length (`>i`, `>l`).
  pub(crate) fn i32(&mut self, what: &'static str) -> Result<i32, Truncated> {
    Ok(i32::from_be_bytes(self.array(what)?))
  }

  /// A node's 20 bytes.
  pub(crate) fn node(&mut self, what: &'static str) -> Result<Node, Truncated> {
    self.array::<NODE_LEN>(what)
  }

  /// The refusal for a read of `needed` bytes from here.
  fn truncated(&self, needed: usize, what: &'static str) -> Truncated {
    Truncated {
      what,
      at: to_u64(self.at),
      needed: to_u64(needed),
      left: to_u64(self.left()),
    }
  }
}

/// A count or a length as `u64`. Every `usize` fits on the targets slates ships (Appendix B.2), and one that did not
/// would saturate, which every caller then refuses, rather than wrap.
pub(crate) fn to_u64(value: usize) -> u64 {
  u64::try_from(value).unwrap_or(u64::MAX)
}
