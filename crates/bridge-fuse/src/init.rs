//! `FUSE_INIT` negotiation (§4.6 "Cache posture"). The kernel sends its ABI version and the
//! flags it supports; the daemon replies with its version and the intersection of what it wants
//! and what the kernel offers, plus the sizes it will use. The negotiation is pure: it takes
//! the kernel's numbers and returns the reply's, so it is unit-tested without a mount.

use crate::abi::{FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, Opcode, flags};
use crate::error::FuseError;
use crate::wire::{Reader, Writer};

/// The maximum write the daemon accepts in one request; public because it is the anchor of the
/// virtio-fs device's readable-bytes cap (`slates-bridge-virtiofs`): the largest request a guest
/// kernel can send is a full write, so the two must be the same number.
/// Shape: one arena chunk (256 KiB at a 4 KiB page, the FUSE big-write convention and the ramp's
/// ceiling); the kernel is told this at INIT and never sends a larger write.
pub const MAX_WRITE: u32 = 256 * 1024;
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

/// The flags slates asks for (the connection keeps whatever the kernel also offers). Each is a
/// capability slates implements completely: readdirplus has its handler, explicit data invalidation
/// and expire-only entries their notifications. `INIT_EXT` is echoed so the kernel reads the reply's
/// second word at all (`process_init_reply` takes `flags2` only from a reply whose `flags` carry
/// `FUSE_INIT_EXT`); without it no second-word capability reaches the kernel however the intersection
/// came out. **Writeback cache is never asked for** (§4.6 "Cache posture": a transport that cannot
/// implement the requested coherence refuses that guarantee): under `FUSE_WRITEBACK_CACHE` the kernel
/// owns a regular file's size, mtime and ctime (`fs/fuse/dir.c` `fuse_get_cache_mask`) and neither
/// re-fetches them nor takes the daemon's, so a change made through another attachment — the SDK, a
/// second mount, an outsider beneath a base — stays invisible to `stat` even after an accepted
/// `FUSE_NOTIFY_INVAL_INODE`; measured on Linux 6.12 (2026-09-19,
/// `docs/bugs/2026-09-19-writeback-cache-made-the-kernel-the-size-authority.md`). Write-through
/// also keeps a `write`'s bytes in the daemon before the call returns (D-18).
fn wanted() -> u64 {
  flags::PARALLEL_DIROPS
    | flags::DO_READDIRPLUS
    | flags::READDIRPLUS_AUTO
    | flags::EXPLICIT_INVAL_DATA
    | flags::BIG_WRITES
    | flags::DONT_MASK
    | flags::INIT_EXT
    | flags::HAS_EXPIRE_ONLY
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
  // The kernel's second flags word, `flags2`, follows `flags` directly when the kernel set
  // INIT_EXT and the body carries it (7.36+; `struct fuse_init_in { major, minor, max_readahead,
  // flags, flags2, unused[11] }`); slates reads it only to intersect, never requires it. Until
  // 2026-09-14 the codec skipped a word it took for reserved padding and read `unused[0]` — always
  // zero — so no second-word capability could ever be negotiated; the independent header vector
  // (`tests/abi.rs`) found it.
  // Format: the high half of the flags word is `flags2`.
  const HIGH_HALF_SHIFT: u32 = 32;
  if kernel_flags & flags::INIT_EXT != 0 && r.remaining() >= size_of::<u32>() {
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
