//! The guest-memory seam (§4.6 A-9: "queue descriptors, scatter/gather ranges, arithmetic and
//! chained lengths are validated within derived caps before access"). Everything the device reads
//! from or writes into the guest goes through [`GuestMemory`]: a bounded, checked copy of one
//! guest-physical range at a time, refused typed when the range overflows the address space or lies
//! outside (or across the boundary of) the memory the VMM mapped. The device never holds a pointer
//! into guest memory and never casts a guest byte to a struct — it reads fields in order through
//! byte arrays, the discipline the FUSE codec (`slates-bridge-fuse`, no `unsafe`) set for parsers of
//! external bytes — so a hostile guest can corrupt only its own buffers, never the device.
//!
//! Two implementations: [`crate::sim::SimGuestMemory`], a `Vec<u8>` arena the tests own (the
//! `SimHost` pattern of `crates/vfs/src/host`), and — the later leg for a real VMM — a shared
//! mapping handed over in-process or by an inherited descriptor, whose `read`/`write` must go through
//! the atomic views of `slates_mem::SharedObject` (the ring indices carry acquire/release ordering,
//! virtio 1.2 §2.7.13 and §2.7.14; a plain slice over memory another process writes is a data race).
//! A [`GuestRange`] is built only through [`GuestRange::new`], which proves `start + len` does not
//! overflow, so every later computation on it is plain arithmetic.

use std::fmt;

/// A guest-physical address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GuestAddr(pub u64);

/// A half-open guest-physical byte range `[start, start + len)` whose end is proven not to
/// overflow the address space (built only through [`GuestRange::new`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GuestRange {
  start: GuestAddr,
  len: u64,
}

impl GuestRange {
  /// The range of `len` bytes at `start`; refused when `start + len` overflows.
  pub fn new(start: GuestAddr, len: u64) -> Result<GuestRange, GuestMemoryError> {
    start
      .0
      .checked_add(len)
      .ok_or(GuestMemoryError::LengthOverflow {
        start: start.0,
        len,
      })?;
    Ok(GuestRange { start, len })
  }

  /// The first address.
  pub const fn start(&self) -> GuestAddr {
    self.start
  }

  /// The length in bytes.
  pub const fn len(&self) -> u64 {
    self.len
  }

  /// Whether the range covers no byte.
  pub const fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// One past the last address (cannot overflow: proven at construction).
  pub const fn end(&self) -> u64 {
    self.start.0 + self.len
  }

  /// Whether the two ranges share a byte (empty ranges share none).
  pub const fn overlaps(&self, other: &GuestRange) -> bool {
    !self.is_empty()
      && !other.is_empty()
      && self.start.0 < other.end()
      && other.start.0 < self.end()
  }

  /// Whether `other` lies wholly inside this range.
  pub const fn contains(&self, other: &GuestRange) -> bool {
    other.start.0 >= self.start.0 && other.end() <= self.end()
  }
}

/// A typed refusal from the guest-memory seam. Never a panic, never a partial access: a refused
/// range was not read or written at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuestMemoryError {
  /// `start + len` overflows the 64-bit guest address space.
  LengthOverflow {
    /// The range's start.
    start: u64,
    /// The range's length.
    len: u64,
  },
  /// The range lies outside every region the VMM mapped, or crosses a region's boundary (a buffer
  /// must be contiguous guest memory).
  OutsideGuestMemory {
    /// The range's start.
    start: u64,
    /// The range's length.
    len: u64,
  },
  /// Two mapped regions overlap (a hostile memory table; refused before any region is used).
  RegionsOverlap {
    /// The first region's base.
    first: u64,
    /// The second region's base.
    second: u64,
  },
  /// The caller's buffer does not match the range's length (a device bug, refused rather than
  /// silently truncated).
  BufferMismatch {
    /// The range's length.
    wanted: u64,
    /// The buffer's length.
    have: usize,
  },
}

impl fmt::Display for GuestMemoryError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::LengthOverflow { start, len } => {
        write!(
          f,
          "guest range {start:#x}+{len:#x} overflows the address space"
        )
      }
      Self::OutsideGuestMemory { start, len } => {
        write!(f, "guest range {start:#x}+{len:#x} is outside guest memory")
      }
      Self::RegionsOverlap { first, second } => {
        write!(
          f,
          "guest memory regions at {first:#x} and {second:#x} overlap"
        )
      }
      Self::BufferMismatch { wanted, have } => {
        write!(
          f,
          "a {wanted}-byte guest range was given a {have}-byte buffer"
        )
      }
    }
  }
}

impl std::error::Error for GuestMemoryError {}

/// The memory a guest can see, as the device may touch it: bounded, checked copies of one range at
/// a time. `check` is the question every access asks first; an implementation answers it without
/// touching a byte, so a refused range is never accessed.
pub trait GuestMemory {
  /// Whether `range` lies wholly inside one mapped region; refused typed otherwise. Touches nothing.
  fn check(&self, range: GuestRange) -> Result<(), GuestMemoryError>;
  /// Copies `range` into `out`, which must be exactly `range.len()` bytes long; refused, with no
  /// byte read, when the range is outside guest memory or the buffer does not match.
  fn read(&self, range: GuestRange, out: &mut [u8]) -> Result<(), GuestMemoryError>;
  /// Copies `bytes`, which must be exactly `range.len()` bytes, into `range`; refused, with no byte
  /// written, when the range is outside guest memory or the buffer does not match.
  fn write(&mut self, range: GuestRange, bytes: &[u8]) -> Result<(), GuestMemoryError>;
}

/// The guest range of a fixed-width field at `at`.
fn field<const WIDTH: usize>(at: GuestAddr) -> Result<GuestRange, GuestMemoryError> {
  GuestRange::new(at, u64::try_from(WIDTH).unwrap_or(u64::MAX))
}

/// Reads a little-endian `u16` at `at` (virtio fields are little-endian, §2.7).
pub fn read_u16(memory: &dyn GuestMemory, at: GuestAddr) -> Result<u16, GuestMemoryError> {
  let mut bytes = [0u8; size_of::<u16>()];
  memory.read(field::<{ size_of::<u16>() }>(at)?, &mut bytes)?;
  Ok(u16::from_le_bytes(bytes))
}

/// Reads a little-endian `u32` at `at`.
pub fn read_u32(memory: &dyn GuestMemory, at: GuestAddr) -> Result<u32, GuestMemoryError> {
  let mut bytes = [0u8; size_of::<u32>()];
  memory.read(field::<{ size_of::<u32>() }>(at)?, &mut bytes)?;
  Ok(u32::from_le_bytes(bytes))
}

/// Reads a little-endian `u64` at `at`.
pub fn read_u64(memory: &dyn GuestMemory, at: GuestAddr) -> Result<u64, GuestMemoryError> {
  let mut bytes = [0u8; size_of::<u64>()];
  memory.read(field::<{ size_of::<u64>() }>(at)?, &mut bytes)?;
  Ok(u64::from_le_bytes(bytes))
}

/// Writes a little-endian `u16` at `at`.
pub fn write_u16(
  memory: &mut dyn GuestMemory,
  at: GuestAddr,
  value: u16,
) -> Result<(), GuestMemoryError> {
  memory.write(field::<{ size_of::<u16>() }>(at)?, &value.to_le_bytes())
}

/// Writes `bytes` at `at` as one access (a used element is published whole, §2.7.8.2).
pub fn write_bytes(
  memory: &mut dyn GuestMemory,
  at: GuestAddr,
  bytes: &[u8],
) -> Result<(), GuestMemoryError> {
  let range = GuestRange::new(at, u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
  memory.write(range, bytes)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_range_proves_its_end_at_construction_and_overlap_is_symmetric() {
    assert!(GuestRange::new(GuestAddr(u64::MAX), 1).is_err());
    let a = GuestRange::new(GuestAddr(100), 50).unwrap();
    let b = GuestRange::new(GuestAddr(149), 10).unwrap();
    let c = GuestRange::new(GuestAddr(150), 10).unwrap();
    let empty = GuestRange::new(GuestAddr(120), 0).unwrap();
    assert!(a.overlaps(&b) && b.overlaps(&a));
    assert!(
      !a.overlaps(&c) && !c.overlaps(&a),
      "half-open: touching ranges do not overlap"
    );
    assert!(!a.overlaps(&empty), "an empty range shares no byte");
    assert!(a.contains(&empty));
    assert_eq!(a.end(), 150);
  }
}
