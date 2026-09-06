//! The closed refusal taxonomy of the volume core: the POSIX errnos a bridge maps one to one, and
//! the volume-level refusals of §4.4. An uncategorized refusal is a bug.

use std::fmt;

/// A typed refusal from the volume core; never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VfsError {
  /// `ENOENT`.
  NotFound,
  /// `EEXIST` (including a name that folds equal under the volume's policy).
  AlreadyExists,
  /// `ENOTDIR`.
  NotDirectory,
  /// `EISDIR`.
  IsDirectory,
  /// `ENOTEMPTY`.
  NotEmpty,
  /// `EINVAL` (a rename into its own subtree, an invalid argument).
  Invalid,
  /// `EPERM` (a hard link to a directory).
  NotPermitted,
  /// `ENAMETOOLONG` or a name with a separator or NUL.
  InvalidName,
  /// `EMLINK`.
  TooManyLinks,
  /// `ENOSPC`: the quota, or the pressure source, refused the bytes; nothing changed.
  NoSpace,
  /// `EFBIG`: an offset or length beyond what the volume addresses.
  FileTooLarge,
  /// `EXDEV`: a move across volumes.
  CrossVolumeMove,
  /// A handle or number that no longer names anything (a freed inode, a destroyed snapshot).
  StaleHandle,
  /// The volume is being destroyed.
  Destroying,
  /// The snapshot is pinned by a live clone; destroy the clone (and unpin) first.
  Pinned,
  /// The base beneath an overlay entry cannot be served: the host's errno.
  BaseUnavailable(i32),
  /// A witnessed base entry changed on disk beneath an unpinned range: the read refuses rather
  /// than return torn bytes; `status` lists the drift.
  BaseDrift,
  /// A base-plane verb on a scratch volume, or a path the base does not hold.
  NotOverlay,
  /// A resource a recovery needs is missing or unreadable: a truncated or corrupt volume image,
  /// or a body this recovery slice does not yet capture (a base-backed entry). §4.8 (A-9)
  /// requires this over an empty success — a partial recovery must refuse, never silently
  /// present a smaller volume than was acknowledged.
  RecoveryIncomplete,
  /// The volume is archived.
  Archived,
  /// The name-equivalence policy of the two volumes differs (clone into a policy is refused).
  PolicyMismatch,
  /// A memory refusal beneath the volume (arena exhausted, slab full).
  Memory(slates_mem::MemError),
}

impl VfsError {
  /// The POSIX errno name, for bridges and the differential harness.
  pub const fn errno_name(&self) -> &'static str {
    match self {
      Self::NotFound => "ENOENT",
      Self::AlreadyExists => "EEXIST",
      Self::NotDirectory => "ENOTDIR",
      Self::IsDirectory => "EISDIR",
      Self::NotEmpty => "ENOTEMPTY",
      Self::Invalid => "EINVAL",
      Self::NotPermitted => "EPERM",
      Self::InvalidName => "ENAMETOOLONG",
      Self::TooManyLinks => "EMLINK",
      Self::NoSpace => "ENOSPC",
      Self::FileTooLarge => "EFBIG",
      Self::CrossVolumeMove => "EXDEV",
      Self::StaleHandle => "ESTALE",
      Self::Destroying | Self::Archived | Self::Pinned => "EBUSY",
      Self::BaseUnavailable(_) | Self::BaseDrift | Self::RecoveryIncomplete => "EIO",
      Self::NotOverlay => "ENODEV",
      Self::PolicyMismatch => "EINVAL",
      Self::Memory(_) => "ENOMEM",
    }
  }
}

impl fmt::Display for VfsError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Memory(e) => write!(f, "{} ({e})", self.errno_name()),
      other => f.write_str(other.errno_name()),
    }
  }
}

impl std::error::Error for VfsError {}

impl From<slates_mem::MemError> for VfsError {
  fn from(e: slates_mem::MemError) -> Self {
    Self::Memory(e)
  }
}
