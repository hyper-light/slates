//! Identifiers: epochs (the birth clock of copy-on-write), inode numbers (monotonic, never
//! reused within a volume) and snapshot ids.

use std::fmt;

/// The birth epoch of a node or chunk: the head epoch at its creation. A snapshot freezes the
/// current epoch and moves the head to the next one, so `born < head` means "shared with a
/// snapshot; copy before mutating" (D-5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Epoch(pub u64);

impl Epoch {
  /// The next epoch.
  pub const fn next(self) -> Epoch {
    Epoch(self.0 + 1)
  }
}

/// An inode number: `(volume prefix, monotonic counter)`; never reused within a volume, and kept
/// by an entry for the volume's lifetime, through snapshots, clones and re-opens (AC-1.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InodeNo(pub u64);

impl InodeNo {
  /// Format: the volume prefix occupies the top bits, the counter the low ones.
  pub const COUNTER_BITS: u32 = 48;

  /// Composes a number from its prefix and counter.
  pub const fn compose(prefix: u16, counter: u64) -> InodeNo {
    InodeNo(((prefix as u64) << Self::COUNTER_BITS) | (counter & ((1 << Self::COUNTER_BITS) - 1)))
  }

  /// The counter part.
  pub const fn counter(self) -> u64 {
    self.0 & ((1 << Self::COUNTER_BITS) - 1)
  }

  /// The volume prefix.
  pub fn prefix(self) -> u16 {
    u16::try_from(self.0 >> Self::COUNTER_BITS).unwrap_or(u16::MAX)
  }
}

impl fmt::Display for InodeNo {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}:{}", self.prefix(), self.counter())
  }
}

/// A snapshot id within a volume (its slot in the volume's snapshot slab plus a generation, so a
/// destroyed snapshot's id is refused rather than confused with a later one).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SnapshotId {
  /// The slot.
  pub index: u32,
  /// The slot's generation when issued.
  pub generation: u32,
}
