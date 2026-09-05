//! `FUSE_INIT` negotiation (§4.6 "Cache posture"). The kernel sends its ABI version and the
//! flags it supports; the daemon replies with its version and the intersection of what it wants
//! and what the kernel offers, plus the sizes it will use. The negotiation is pure: it takes
//! the kernel's numbers and returns the reply's, so it is unit-tested without a mount.

use crate::abi::{FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, Opcode, flags};
use crate::error::FuseError;
use crate::wire::{Reader, Writer};

/// Shape: the maximum write the daemon accepts in one request: one arena chunk (256 KiB at a
/// 4 KiB page, the FUSE big-write convention and the ramp's ceiling). The kernel is told this
/// at INIT and never sends a larger write.
const MAX_WRITE: u32 = 256 * 1024;
/// Shape: the read-ahead the daemon suggests, matched to `MAX_WRITE`.
const MAX_READAHEAD: u32 = 256 * 1024;
/// Shape: the time granularity the daemon reports: one nanosecond (times are monotonic ns).
const TIME_GRAN_NS: u32 = 1;

/// What the daemon decided at `FUSE_INIT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitNegotiation {
  /// The major version (always `FUSE_KERNEL_VERSION`; a lower kernel major is refused).
  pub major: u32,
  /// The minor version: the lesser of the kernel's and slates'.
  pub minor: u32,
  /// The flags the connection keeps (the intersection of wanted and offered).
  pub flags: u64,
  /// The maximum write per request.
  pub max_write: u32,
  /// The read-ahead suggested.
  pub max_readahead: u32,
  /// Whether the kernel's major is one slates cannot speak (the caller replies with its own
  /// version so the kernel can retry, per the ABI).
  pub version_mismatch: bool,
}

/// The flags slates asks for (the connection keeps whatever the kernel also offers).
fn wanted() -> u64 {
  flags::WRITEBACK_CACHE
    | flags::PARALLEL_DIROPS
    | flags::DO_READDIRPLUS
    | flags::READDIRPLUS_AUTO
    | flags::EXPLICIT_INVAL_DATA
    | flags::BIG_WRITES
    | flags::DONT_MASK
}

/// Negotiates from a `fuse_init_in` body (major, minor, max_readahead, flags, then flags2 and
/// padding when the kernel set `INIT_EXT`). A body shorter than the fixed part is refused.
pub fn negotiate(body: &[u8]) -> Result<InitNegotiation, FuseError> {
  let opcode = Opcode::Init.to_wire();
  let mut r = Reader::new(body);
  let major = r.u32(opcode)?;
  let minor = r.u32(opcode)?;
  let kernel_readahead = r.u32(opcode)?;
  let mut kernel_flags = u64::from(r.u32(opcode)?);
  // The kernel's second flags word rides in `flags2` when it set INIT_EXT and the body carries
  // it (7.36+); slates reads it only to intersect, never requires it.
  // Format: with FUSE_INIT_EXT the body carries two more 32-bit words (a reserved word then
  // flags2); slates reads flags2 into the high half of the flags word.
  const EXT_WORDS: usize = 2 * size_of::<u32>();
  const HIGH_HALF_SHIFT: u32 = 32;
  if kernel_flags & flags::INIT_EXT != 0 && r.remaining() >= EXT_WORDS {
    let _reserved_before_flags2 = r.u32(opcode)?;
    kernel_flags |= u64::from(r.u32(opcode)?) << HIGH_HALF_SHIFT;
  }
  if major < FUSE_KERNEL_VERSION {
    return Ok(InitNegotiation {
      major: FUSE_KERNEL_VERSION,
      minor: FUSE_KERNEL_MINOR_VERSION,
      flags: 0,
      max_write: MAX_WRITE,
      max_readahead: MAX_READAHEAD,
      version_mismatch: true,
    });
  }
  Ok(InitNegotiation {
    major: FUSE_KERNEL_VERSION,
    minor: minor.min(FUSE_KERNEL_MINOR_VERSION),
    flags: wanted() & kernel_flags,
    max_write: MAX_WRITE,
    max_readahead: kernel_readahead.min(MAX_READAHEAD),
    version_mismatch: false,
  })
}

impl InitNegotiation {
  /// Writes the `fuse_init_out` reply body in field order: major, minor, max_readahead,
  /// flags, max_background, congestion_threshold, max_write, time_gran, max_pages,
  /// map_alignment, flags2, then reserved words (left zero).
  pub fn to_bytes(&self) -> Vec<u8> {
    /// Shape: the reserved trailer of `fuse_init_out` slates leaves zero (seven 32-bit words).
    const RESERVED_WORDS: usize = 7 * 4;
    let mut w = Writer::new();
    w.u32(self.major);
    w.u32(self.minor);
    w.u32(self.max_readahead);
    // Format: the flags word splits into a low and a high 32-bit half (flags and flags2).
    const HIGH_HALF_SHIFT: u32 = 32;
    let low = u32::try_from(self.flags & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    let high = u32::try_from(self.flags >> HIGH_HALF_SHIFT).unwrap_or(u32::MAX);
    w.u32(low);
    w.u32(0); // max_background + congestion_threshold (two u16, both zero: kernel defaults)
    w.u32(self.max_write);
    w.u32(TIME_GRAN_NS);
    w.u32(0); // max_pages + map_alignment (two u16, kernel defaults)
    w.u32(high); // flags2
    w.pad(RESERVED_WORDS);
    w.into_bytes()
  }
}
