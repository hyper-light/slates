//! A whole object as a pack entry's zlib stream of stored blocks (A-665): the one encoding of an
//! object the view makes itself, chosen so that every byte of it follows from the object's length
//! and its own bytes.
//!
//! **Why stored blocks.** A pack entry's data is the object deflated as a zlib stream \[B:
//! gitformat-pack, "compressed data"\], and git inflates whatever valid stream it finds there. A
//! compressed stream's length is known only once the stream is made, so a view that placed one
//! would compress an object, and every object before it, to say where an entry begins. A stream
//! of stored blocks (deflate's block type 0) is zlib's two-byte header, five bytes before each
//! block of at most 65535 bytes, the object's own bytes, and their Adler-32 \[B: RFC 1950 §2.2,
//! RFC 1951 §3.2.4\]: its length is a function of the object's, and any window of it is the
//! object's bytes copied from the store with the framing between them. Only the last four bytes,
//! the Adler-32, need the object read whole; the view keeps them with its unit's seal.
//!
//! **Adler-32** as zlib's `adler32.c` takes it: two sums modulo 65521, the largest prime below
//! 2^16, reduced every 5552 bytes, the most whose sums cannot pass 32 bits (zlib's `NMAX`) \[C:
//! zlib `adler32.c`; B: RFC 1950 §8.2\].

use crate::to_u64;

use super::ReadRefused;

/// Format: zlib's header for a stream of stored blocks: CMF 0x78 (deflate, a 32 KiB window) and
/// FLG 0x01 (level flags 0, no preset dictionary, and the check bits that make CMF * 256 + FLG a
/// multiple of 31), what zlib writes at level 0 \[B: RFC 1950 §2.2; C: zlib `deflate.c`\].
pub(super) const ZLIB_HEADER: [u8; 2] = [0x78, 0x01];

/// Format: the most bytes one stored block holds, its LEN being 16 bits \[B: RFC 1951 §3.2.4\].
pub(super) const BLOCK_MAX: u64 = 0xffff;

/// Format: a stored block's header when the block begins on a byte: BFINAL and BTYPE 00 in one
/// byte, then LEN and NLEN, two bytes each, little-endian \[B: RFC 1951 §3.2.3, §3.2.4\].
const BLOCK_HEADER_LEN: u64 = 5;

/// Format: the BFINAL bit of a block's first byte, set on a stream's last block \[B: RFC 1951
/// §3.2.3\].
const FINAL: u8 = 1;

/// Format: the Adler-32 that ends a zlib stream, four bytes, most significant first \[B: RFC 1950
/// §2.2\].
pub(super) const ADLER_LEN: u64 = 4;

/// Format: Adler-32's modulus, the largest prime below 2^16 \[B: RFC 1950 §8.2; C: zlib
/// `adler32.c` `BASE`\].
const ADLER_BASE: u32 = 65_521;

/// Derived: the most bytes Adler-32's two 32-bit sums take between reductions without passing 32
/// bits, the largest n with 255 n (n + 1) / 2 + (n + 1) (BASE - 1) <= 2^32 - 1 \[C: zlib
/// `adler32.c` `NMAX`\].
const ADLER_RUN: usize = 5552;

/// Format: where Adler-32's second sum sits in its value, the upper 16 bits \[B: RFC 1950 §8.2\].
const ADLER_HIGH_SHIFT: u32 = 16;

/// An Adler-32 taken over bytes as they pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Adler32 {
  low: u32,
  high: u32,
}

impl Default for Adler32 {
  /// Format: the sums Adler-32 starts from, one and zero \[B: RFC 1950 §8.2\].
  fn default() -> Self {
    Adler32 { low: 1, high: 0 }
  }
}

impl Adler32 {
  /// Takes `bytes` into the sums.
  pub(super) fn update(&mut self, bytes: &[u8]) {
    for run in bytes.chunks(ADLER_RUN) {
      for byte in run {
        // Never wraps: within a run of `ADLER_RUN` bytes both sums stay below 2^32, which is
        // what the run's length is derived from.
        self.low = self.low.wrapping_add(u32::from(*byte));
        self.high = self.high.wrapping_add(self.low);
      }
      self.low = self.low.checked_rem(ADLER_BASE).unwrap_or(self.low);
      self.high = self.high.checked_rem(ADLER_BASE).unwrap_or(self.high);
    }
  }

  /// The value: the second sum above the first.
  pub(super) fn value(self) -> u32 {
    self.high.checked_shl(ADLER_HIGH_SHIFT).unwrap_or(0) | self.low
  }
}

/// The Adler-32 of `bytes` (the tests' check of [`Adler32`]).
#[cfg(test)]
pub(super) fn adler32(bytes: &[u8]) -> u32 {
  let mut sum = Adler32::default();
  sum.update(bytes);
  sum.value()
}

/// The stored blocks the stream of an object `len` bytes long has: one for each `BLOCK_MAX` bytes
/// or part of them, and one, empty, for an empty object.
fn blocks(len: u64) -> u64 {
  len.div_ceil(BLOCK_MAX).max(1)
}

/// The bytes the stream of an object `len` bytes long takes: zlib's header, each block's header,
/// the object's bytes and the Adler-32.
pub(super) fn stream_len(len: u64) -> u64 {
  to_u64(ZLIB_HEADER.len())
    .saturating_add(blocks(len).saturating_mul(BLOCK_HEADER_LEN))
    .saturating_add(len)
    .saturating_add(ADLER_LEN)
}

/// The object's bytes block `block` of an object `len` bytes long holds.
fn block_data(len: u64, block: u64) -> u64 {
  len
    .saturating_sub(block.saturating_mul(BLOCK_MAX))
    .min(BLOCK_MAX)
}

/// The header of block `block` of an object `len` bytes long: its final bit, LEN and NLEN.
fn block_header(len: u64, block: u64) -> [u8; 5] {
  let final_bit = if block.saturating_add(1) >= blocks(len) {
    FINAL
  } else {
    0
  };
  let held = u16::try_from(block_data(len, block)).unwrap_or(u16::MAX);
  let [low, high] = held.to_le_bytes();
  let [not_low, not_high] = (!held).to_le_bytes();
  [final_bit, low, high, not_low, not_high]
}

/// What a byte of a stream is part of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Piece {
  /// zlib's header.
  Zlib,
  /// A block's header.
  BlockHeader {
    /// The block.
    block: u64,
  },
  /// The object's bytes, from its byte `first` at the piece's start.
  Data {
    /// The object's byte the piece begins with.
    first: u64,
  },
  /// The Adler-32.
  Adler,
}

/// The piece of the stream of an object `len` bytes long that holds the stream's byte `at`, with
/// where the piece begins and ends in the stream; `None` past the stream's end.
fn piece_at(len: u64, at: u64) -> Option<(Piece, u64, u64)> {
  let zlib = to_u64(ZLIB_HEADER.len());
  if at < zlib {
    return Some((Piece::Zlib, 0, zlib));
  }
  let span = BLOCK_HEADER_LEN.saturating_add(BLOCK_MAX);
  let last = blocks(len).saturating_sub(1);
  let block = at
    .saturating_sub(zlib)
    .checked_div(span)
    .unwrap_or(0)
    .min(last);
  let block_start = zlib.saturating_add(block.saturating_mul(span));
  let data_start = block_start.saturating_add(BLOCK_HEADER_LEN);
  let block_end = data_start.saturating_add(block_data(len, block));
  if at < data_start {
    return Some((Piece::BlockHeader { block }, block_start, data_start));
  }
  if at < block_end {
    let first = block.saturating_mul(BLOCK_MAX);
    return Some((Piece::Data { first }, data_start, block_end));
  }
  let adler_end = block_end.saturating_add(ADLER_LEN);
  (block == last && at < adler_end).then_some((Piece::Adler, block_end, adler_end))
}

/// Where a stream's bytes come from: the object's bytes `from..to`, appended to `out`, and its
/// Adler-32.
pub(super) trait Source {
  /// Appends the object's bytes `from..to` to `out`.
  fn data(&mut self, from: u64, to: u64, out: &mut Vec<u8>) -> Result<(), ReadRefused>;
  /// The object's Adler-32, or why it is not known here.
  fn adler(&mut self) -> Result<u32, ReadRefused>;
}

/// Appends the bytes `from..to` of `bytes` that lie within `start..stop`, the place `bytes` take.
fn copy_part(bytes: &[u8], (start, stop): (u64, u64), (from, to): (u64, u64), out: &mut Vec<u8>) {
  let first = usize::try_from(from.max(start).saturating_sub(start)).unwrap_or(usize::MAX);
  let last = usize::try_from(to.min(stop).saturating_sub(start)).unwrap_or(usize::MAX);
  out.extend_from_slice(bytes.get(first..last).unwrap_or_default());
}

/// Appends the bytes `from..to` of the stream of an object `len` bytes long to `out` (clipped to
/// the stream): the framing made here, the object's bytes and its Adler-32 from `source`, which is
/// asked for the Adler-32 only when the bytes reach it.
pub(super) fn fill(
  len: u64,
  (from, to): (u64, u64),
  source: &mut dyn Source,
  out: &mut Vec<u8>,
) -> Result<(), ReadRefused> {
  let end = to.min(stream_len(len));
  let mut at = from;
  while at < end {
    let (piece, start, stop) = piece_at(len, at).ok_or(ReadRefused::Unreadable)?;
    let upto = stop.min(end);
    match piece {
      Piece::Zlib => copy_part(&ZLIB_HEADER, (start, stop), (at, upto), out),
      Piece::BlockHeader { block } => {
        copy_part(&block_header(len, block), (start, stop), (at, upto), out);
      }
      Piece::Data { first } => source.data(
        first.saturating_add(at.saturating_sub(start)),
        first.saturating_add(upto.saturating_sub(start)),
        out,
      )?,
      Piece::Adler => copy_part(
        &source.adler()?.to_be_bytes(),
        (start, stop),
        (at, upto),
        out,
      ),
    }
    at = upto;
  }
  Ok(())
}

/// Where the Adler-32 of a stream of an object `len` bytes long begins (the tests' reads up to
/// it).
#[cfg(test)]
pub(super) fn adler_at(len: u64) -> u64 {
  stream_len(len).saturating_sub(ADLER_LEN)
}

#[cfg(test)]
#[path = "../../tests/store/hook_view_stored.rs"]
mod tests;
