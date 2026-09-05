//! The read-only host seam of the base plane (§4.5, §4.15, D-25): everything the volume core
//! asks of the disk beneath an overlay volume, as one trait with opaque handles, so the core
//! links no host call at all. `slates-base` implements it over the operating system with the
//! bulk listing and `O_NOFOLLOW` opens the design names and with the watchers behind the same
//! seam; [`sim::SimHost`] implements it in memory with outsider edits, clock control and
//! watcher overflow on demand, which is what the oracle tests drive.
//!
//! Nothing behind this trait can write: it has no verb for it, and the implementing crate's
//! lint wall refuses write-capable syscalls (R1).

pub mod sim;

use crate::inode::Fingerprint;

/// An open directory on the host, owned by the host implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostDir(pub u64);

/// An open file on the host, owned by the host implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostFile(pub u64);

/// What a listed entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HostKind {
  /// A regular file.
  File,
  /// A directory.
  Dir,
  /// A symlink.
  Symlink,
  /// Anything else (a device, a socket): listed, never served.
  Other,
}

/// One entry of a listing, with the fingerprint the bulk call returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseEntry {
  /// The name as the host holds it.
  pub name: Box<str>,
  /// The kind.
  pub kind: HostKind,
  /// The stat fingerprint at listing time.
  pub fingerprint: Fingerprint,
}

/// Facts about the filesystem beneath a directory that the drift rules need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostFacts {
  /// The filesystem's timestamp granularity in nanoseconds, from the cited table by type
  /// (`research/disk-source-of-truth.md` §4), plus the measured clock resolution.
  pub timestamp_granularity_ns: u64,
}

/// A watcher's word, never the truth (§4.5): a directory changed, or events were lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
  /// Something under this directory changed.
  Changed(HostDir),
  /// The watcher overflowed: every listing is suspect and every witness is re-checked.
  Overflow,
}

/// The watcher's state, as `status` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchState {
  /// Delivering hints.
  Live,
  /// Lost events; a full re-check is scheduled or done.
  Overflowed,
  /// No watcher on this host or directory; fingerprints alone.
  Unavailable,
}

/// A host refusal, as the seam reports it; the volume maps it to its own taxonomy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostError {
  /// The entry does not exist (any more).
  NotFound,
  /// Not a directory where one was needed.
  NotDirectory,
  /// Not a file where one was needed (a directory, a device).
  NotFile,
  /// A handle the host no longer holds.
  StaleHandle,
  /// The host cannot serve it: the errno.
  Unavailable(i32),
}

/// The seam. Every call is relative to a handle the host handed out, never to a path string,
/// so a rename of the base directory by the user changes nothing (§4.4).
pub trait HostFs {
  /// Facts about the filesystem under a directory.
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError>;
  /// The directory's own fingerprint, which changes when its entries do.
  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError>;
  /// The directory's entries with their fingerprints, one bulk call.
  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError>;
  /// Opens a subdirectory (never through a symlink).
  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError>;
  /// Opens a file read-only (never through a symlink); the descriptor keeps the inode's data
  /// alive if the file is renamed over or unlinked.
  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError>;
  /// The fingerprint of an open file, now.
  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError>;
  /// Reads at an offset; returns the bytes read (fewer at the end).
  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError>;
  /// A symlink's target.
  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError>;
  /// Closes a file handle.
  fn close_file(&mut self, file: HostFile);
  /// Closes a directory handle.
  fn close_dir(&mut self, dir: HostDir);
  /// Asks for hints about a directory; `Unavailable` when the host has no watcher.
  fn watch(&mut self, dir: HostDir) -> WatchState;
  /// Drains the hints that arrived since the last call (never blocks).
  fn hints(&mut self) -> Vec<Hint>;
}
