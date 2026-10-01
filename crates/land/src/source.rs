//! What a landing lands (§4.15 step 1, D-26; A-49): the volume's head, or a snapshot the volume holds. The
//! plan, the presentation, the verdicts' witnessed bases and the bytes written all come from the one source,
//! so a landing of a snapshot presents and writes exactly that snapshot's state — never the head's — and
//! judges it against the witnesses it was based on (the snapshot's own since A-48).
//!
//! Until 2026-09-30 the engine read the head whatever snapshot the call named, and the server refused a
//! named snapshot the head had moved past rather than land the wrong state (AUD-29-02). These are the reads
//! the planner and the engine share, one per question, each answered at the source.

use slates_vfs::error::VfsError;
use slates_vfs::host::HostFs;
use slates_vfs::ids::{InodeNo, SnapshotId};
use slates_vfs::inode::{Attrs, Fingerprint, Witness};
use slates_vfs::volume::{Located, Store, Volume};

/// The state a landing lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Source {
  /// The volume's head as it is when the landing runs (one owner-shard step, so it cannot move under the
  /// landing).
  #[default]
  Head,
  /// A snapshot of the volume, exactly as it froze.
  Snapshot(SnapshotId),
}

impl Source {
  /// The snapshot landed, if any.
  pub fn snapshot(self) -> Option<SnapshotId> {
    match self {
      Source::Head => None,
      Source::Snapshot(id) => Some(id),
    }
  }

  /// The entry at `path`.
  pub(crate) fn resolve(
    self,
    vol: &Volume,
    store: &Store,
    path: &str,
  ) -> Result<Located, VfsError> {
    match self {
      Source::Head => vol.resolve(store, path),
      Source::Snapshot(id) => vol.resolve_in(store, id, path),
    }
  }

  /// Inode `no`'s attributes.
  pub(crate) fn stat(self, vol: &Volume, store: &Store, no: InodeNo) -> Result<Attrs, VfsError> {
    match self {
      Source::Head => vol.stat(store, no),
      Source::Snapshot(id) => vol.stat_in(store, id, no),
    }
  }

  /// Symlink `no`'s target.
  pub(crate) fn readlink(
    self,
    vol: &Volume,
    store: &Store,
    no: InodeNo,
  ) -> Result<Box<str>, VfsError> {
    match self {
      Source::Head => vol.readlink(store, no),
      Source::Snapshot(id) => vol.readlink_in(store, id, no),
    }
  }

  /// The base fingerprint a whiteout at `dir/name` hides.
  pub(crate) fn whiteout_witness(
    self,
    vol: &Volume,
    store: &Store,
    dir: &str,
    name: &str,
  ) -> Option<Fingerprint> {
    match self {
      Source::Head => vol.whiteout_witness(store, dir, name),
      Source::Snapshot(id) => vol.whiteout_witness_in(store, id, dir, name),
    }
  }

  /// The base fingerprint a redirect at `path` moved.
  pub(crate) fn redirect_witness(
    self,
    vol: &Volume,
    store: &Store,
    path: &str,
  ) -> Option<Fingerprint> {
    match self {
      Source::Head => vol.redirect_witness(store, path),
      Source::Snapshot(id) => vol.redirect_witness_in(store, id, path),
    }
  }

  /// Inode `no`'s witnessed base.
  pub(crate) fn witness(self, vol: &Volume, no: InodeNo) -> Option<Witness> {
    match self {
      Source::Head => vol.base_plane().and_then(|plane| plane.witness(no)),
      Source::Snapshot(id) => vol.witness_in(id, no),
    }
  }

  /// A file's bytes from its start, the base's unpinned ranges read from the disk.
  pub(crate) fn read(
    self,
    vol: &mut Volume,
    store: &mut Store,
    host: &mut dyn HostFs,
    no: InodeNo,
    buf: &mut [u8],
  ) -> Result<usize, VfsError> {
    self.read_at(vol, store, host, no, 0, buf)
  }

  /// The bytes of `no` from `off`, as far as `buf` holds: a landing copies a file a window at a time
  /// (AUD-29-25).
  pub(crate) fn read_at(
    self,
    vol: &mut Volume,
    store: &mut Store,
    host: &mut dyn HostFs,
    no: InodeNo,
    off: u64,
    buf: &mut [u8],
  ) -> Result<usize, VfsError> {
    match self {
      Source::Head => vol.with_host(host).read(store, no, off, buf),
      Source::Snapshot(id) => vol.with_host(host).read_in(store, id, no, off, buf),
    }
  }
}
