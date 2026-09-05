//! The closed refusal taxonomy of the memory crate (§4.2, failure matrix): every variant names
//! what was asked and what was available, so the caller can decide; none is a catch-all.

use std::fmt;

/// A typed refusal from the memory crate; never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemError {
  /// The handle's slot has been freed (its generation moved on) or never existed.
  StaleHandle {
    /// The slot index the handle named.
    index: u32,
    /// The generation the handle carried.
    generation: u32,
  },
  /// The slab has no free slot and no reserve segment to take one from.
  SlabFull {
    /// The slab's capacity in slots.
    capacity: usize,
  },
  /// No region has a free extent of the requested class; retryable after growth.
  ArenaExhausted {
    /// The bytes requested.
    requested: usize,
    /// The largest free extent any region can offer right now, in bytes.
    largest_free: usize,
  },
  /// A reservation exceeds what the shard's reserve can cover; nothing was allocated.
  BudgetExceeded {
    /// The bytes requested.
    requested: u64,
    /// The bytes available.
    available: u64,
  },
  /// The OS refused to lock some of the requested bytes; the rest stays usable, unlocked.
  LockRefused {
    /// The bytes asked to be locked.
    requested: usize,
    /// The bytes the OS locked before refusing.
    locked: usize,
    /// The OS error code, when one exists.
    code: Option<i32>,
  },
  /// The OS refused to map a region.
  RegionRefused {
    /// The bytes requested.
    len: usize,
    /// The OS error code, when one exists.
    code: Option<i32>,
  },
  /// A request exceeds the largest class the arena serves.
  TooLarge {
    /// The bytes requested.
    len: usize,
    /// The largest bytes any single extent can hold.
    max: usize,
  },
  /// A ring slot count that is not a power of two, or zero.
  BadCapacity {
    /// The capacity given.
    capacity: usize,
  },
  /// The OS refused a call on a shared memory object; the call is named.
  OsRefused {
    /// The call.
    call: &'static str,
    /// The OS error code, when one exists.
    code: Option<i32>,
  },
  /// An offset into a shared object that is misaligned or past its end.
  OutOfRange {
    /// The offset asked for.
    offset: usize,
    /// The object's length.
    len: usize,
  },
}

impl fmt::Display for MemError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::StaleHandle { index, generation } => {
        write!(f, "stale handle: slot {index} generation {generation}")
      }
      Self::SlabFull { capacity } => write!(f, "slab full at {capacity} slots"),
      Self::ArenaExhausted {
        requested,
        largest_free,
      } => {
        write!(
          f,
          "arena exhausted: {requested} bytes requested, largest free extent {largest_free}"
        )
      }
      Self::BudgetExceeded {
        requested,
        available,
      } => {
        write!(
          f,
          "budget exceeded: {requested} bytes requested, {available} available"
        )
      }
      Self::LockRefused {
        requested,
        locked,
        code,
      } => {
        write!(
          f,
          "lock refused after {locked} of {requested} bytes (code {code:?})"
        )
      }
      Self::RegionRefused { len, code } => {
        write!(f, "region of {len} bytes refused (code {code:?})")
      }
      Self::TooLarge { len, max } => write!(f, "{len} bytes exceeds the largest extent of {max}"),
      Self::BadCapacity { capacity } => write!(f, "ring capacity {capacity} is not a power of two"),
      Self::OsRefused { call, code } => write!(f, "{call} refused (code {code:?})"),
      Self::OutOfRange { offset, len } => {
        write!(
          f,
          "offset {offset} is misaligned or past the {len}-byte object"
        )
      }
    }
  }
}

impl std::error::Error for MemError {}
