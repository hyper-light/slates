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

/// Who holds a share of an inode's references and opens (§4.8 attachments; A-61). The volume attributes every
/// transport reference to an owner, so a teardown releases exactly that owner's share.
///
/// An owner is either a recorded attachment, by its durable §4.8 id — its references are carried in the
/// recovery image and given back to it after a daemon restart, since its kernel (a FUSE mount the anchor held)
/// still holds them — or an owner this process alone knows (a test harness, a host without a record), whose
/// references die with the process. Two kinds, not one number, so a process-local key can never be taken for
/// a durable id it happens to equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RefOwner {
  /// An owner known to this process only.
  Process(u64),
  /// A recorded attachment, by its durable id.
  Attachment(u64),
}

/// An inode number: `(volume prefix, monotonic counter)`; never reused within a volume, and kept
/// by an entry for the volume's lifetime, through snapshots, clones and re-opens (AC-1.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InodeNo(pub u64);

impl InodeNo {
  /// Format: the volume prefix occupies the top bits, the counter the low ones.
  pub const COUNTER_BITS: u32 = 48;

  /// Format: the counter bit that marks a *derived* number: a number no inode holds, which a
  /// transport uses to name an object it derives from an inode (the AppleDouble view of an inode's
  /// extended attributes, §4.6). The volume issues counters strictly below it and refuses at it
  /// (`NoSpace`), so a derived number never names a real inode. Half the 48-bit counter space,
  /// 2^47 inodes per volume, is far beyond any slab a machine can hold.
  pub const DERIVED_BIT: u64 = 1 << (Self::COUNTER_BITS - 1);

  /// The derived number of this inode (see [`Self::DERIVED_BIT`]).
  pub const fn derived(self) -> InodeNo {
    InodeNo(self.0 | Self::DERIVED_BIT)
  }

  /// The inode a derived number was derived from; `None` for a real inode's number.
  pub const fn derived_from(self) -> Option<InodeNo> {
    if self.0 & Self::DERIVED_BIT == 0 {
      None
    } else {
      Some(InodeNo(self.0 & !Self::DERIVED_BIT))
    }
  }

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
