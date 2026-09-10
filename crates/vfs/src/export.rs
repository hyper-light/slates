//! Exporting a snapshot as an archive (§4.10 "Content replication"; §4.11 "Archive"; D-17). A
//! frozen snapshot — its directory tree, each file's bytes and each entry's metadata — is walked
//! into the content-addressed [`Archive`]: a canonical Merkle-hashed manifest whose root identity
//! fingerprints the whole tree, and the distinct chunks the files' bytes are cut into, each named
//! by the BLAKE3 of its bytes. That archive is what the fleet replicates to a snapshot's candidate
//! holders (the design's "the same container is the replication transfer unit and the
//! clone-from-archive source") and what a successor restores after a takeover.
//!
//! The walk is **resumable in bounded slices** (§4.3, "every loop over user-scaled data is chunked
//! into cooperative slices under the shard's per-iteration budget"): [`SnapshotArchiver::advance`]
//! does about `byte_budget` bytes of work — bytes hashed, with a directory entry charged one chunk
//! as the upper bound on its cost — then returns [`Progress::More`] with its cursor kept, so a large
//! volume is archived across many shard steps without starving the clients the shard serves. The
//! caller derives the budget from the machine's measured hash throughput and the shard's step
//! budget (R3); a budget below one unit of work still makes progress, so the walk always terminates.
//!
//! Chunking is fixed-size at the volume's chunk size (the copy-on-write unit, §4.5), so the same
//! bytes at the same offsets cut into the same chunks and deduplicate across snapshots by identity;
//! content-defined chunking (FastCDC) and the compress-or-not cost model are the codec pass's owed
//! refinements (D-17), so chunks are stored raw here. A file's bytes are read **at the snapshot**
//! ([`Volume::read_in`]), never at the head. A symlink is carried as a file whose bytes are its
//! target and whose mode carries the link type bits — the manifest has directory and file nodes
//! only, and the mode's type bits ([`kind_of_mode`]) are what tell a restore the kind. A base-backed
//! entry (an overlay over a host directory, §4.15) is refused [`VfsError::RecoveryIncomplete`]: its
//! bytes are on disk, not in RAM, and an archive that silently covered them with zeros would place a
//! lie (§4.4: a snapshot over a live base has coverage `DeltaWithLiveBase`, and a placement never
//! silently upgrades it).
//!
//! Deterministic: the same snapshot walks to the same archive bytes and the same manifest identity
//! (the manifest sorts entries; the chunk list follows the walk, which follows the directory order
//! the snapshot froze), so two hosts archive one snapshot identically — the property the fleet's
//! holder-side verification and the design's "holders recompute the identity" rest on. Proven by
//! use in the tests below: an export restores byte-identical, twice to the same identity.

use std::collections::{BTreeSet, VecDeque};

use slates_archive::format::Chunk as ArchiveChunk;
use slates_archive::{Archive, Entry, Extent, Node, NodeMeta};
use slates_mem::Handle;

use crate::dir::{Child, DirNode};
use crate::error::VfsError;
use crate::ids::{InodeNo, SnapshotId};
use crate::inode::{Attrs, Body, Kind};
use crate::names::NameEquivalence;
use crate::volume::{Store, Volume, snapshot_handle};

/// Format: POSIX `S_IFMT`, the file-type mask of a mode.
pub const MODE_TYPE_MASK: u32 = 0o170_000;
/// Format: POSIX `S_IFDIR`, the type bits a directory's manifest mode carries.
pub const MODE_DIRECTORY: u32 = 0o040_000;
/// Format: POSIX `S_IFREG`, the type bits a regular file's manifest mode carries.
pub const MODE_FILE: u32 = 0o100_000;
/// Format: POSIX `S_IFLNK`, the type bits a symlink's manifest mode carries (its bytes are the
/// target).
pub const MODE_SYMLINK: u32 = 0o120_000;

/// Format: the archive header's name-policy id for byte-exact names.
const POLICY_EXACT: u32 = 0;
/// Format: the archive header's name-policy id for normalization- and case-insensitive names.
const POLICY_FOLD: u32 = 1;
/// Format: the archive header's Unicode version field when the exporter carries none — the volume
/// core's names module fixes its own table and does not yet surface a version number (owed).
const UNICODE_VERSION_UNSPECIFIED: u32 = 0;

/// The kind a manifest mode's type bits name, or `None` for bits the exporter never writes.
pub fn kind_of_mode(mode: u32) -> Option<Kind> {
  match mode & MODE_TYPE_MASK {
    MODE_DIRECTORY => Some(Kind::Dir),
    MODE_FILE => Some(Kind::File),
    MODE_SYMLINK => Some(Kind::Symlink),
    _ => None,
  }
}

/// The permission bits of a manifest mode, without its type bits.
pub fn permissions_of_mode(mode: u32) -> u32 {
  mode & !MODE_TYPE_MASK
}

/// How far one [`SnapshotArchiver::advance`] got.
#[derive(Debug)]
pub enum Progress {
  /// The slice's budget is spent; more of the snapshot remains, and the cursor is kept.
  More,
  /// The whole snapshot is archived.
  Done(Archive),
}

/// One directory being walked: its node, the entry that names it in its parent, the entries still to
/// visit (fixed when the frame was entered, in the snapshot's directory order), and those built.
struct Frame {
  dir: Handle<DirNode>,
  name: String,
  meta: NodeMeta,
  pending: VecDeque<Pending>,
  built: Vec<Entry>,
}

/// A directory entry still to visit.
struct Pending {
  name: String,
  kind: Kind,
  inode: InodeNo,
}

/// A file part-way through being cut into chunks across slices.
struct FileCursor {
  name: String,
  meta: NodeMeta,
  inode: InodeNo,
  offset: u64,
  size: u64,
  extents: Vec<Extent>,
}

/// The resumable walk of one snapshot into an archive. Create with [`SnapshotArchiver::new`], then
/// call [`advance`](SnapshotArchiver::advance) with a byte budget until it returns
/// [`Progress::Done`].
pub struct SnapshotArchiver {
  snapshot: SnapshotId,
  chunk_bytes: usize,
  base_page_size: u32,
  created_unix: u64,
  volume_id: u64,
  name_policy_id: u32,
  frames: Vec<Frame>,
  file: Option<FileCursor>,
  chunks: Vec<ArchiveChunk>,
  seen: BTreeSet<[u8; 32]>,
  bytes_hashed: u64,
}

impl SnapshotArchiver {
  /// Starts the walk of `id` in `volume`, rooted at the snapshot's frozen root. `volume_id` is the
  /// informational volume identity the archive header carries (the caller's routing id) and
  /// `created_unix` its informational creation time (Unix seconds, the caller's clock — the walk
  /// itself reads no clock, so it is pure and deterministic).
  pub fn new(
    volume: &Volume,
    store: &Store,
    id: SnapshotId,
    volume_id: u64,
    created_unix: u64,
  ) -> Result<SnapshotArchiver, VfsError> {
    let snapshot = volume
      .snapshots
      .get(snapshot_handle(id))
      .map_err(|_| VfsError::StaleHandle)?;
    let root = snapshot.root;
    let pending = pending_of(volume, store, root)?;
    let name_policy_id = match volume.policy {
      NameEquivalence::Exact => POLICY_EXACT,
      NameEquivalence::Fold => POLICY_FOLD,
    };
    Ok(SnapshotArchiver {
      snapshot: id,
      chunk_bytes: store.content.chunk_bytes().max(1),
      base_page_size: u32::try_from(store.content.page()).unwrap_or(u32::MAX),
      created_unix,
      volume_id,
      name_policy_id,
      frames: vec![Frame {
        dir: root,
        name: String::new(),
        meta: NodeMeta::default(),
        pending,
        built: Vec::new(),
      }],
      file: None,
      chunks: Vec::new(),
      seen: BTreeSet::new(),
      bytes_hashed: 0,
    })
  }

  /// The bytes hashed so far — the non-vacuity counter a test reads to know the walk did real work.
  pub fn bytes_hashed(&self) -> u64 {
    self.bytes_hashed
  }

  /// Does about `byte_budget` bytes of work (at least one unit, so the walk always progresses) and
  /// reports whether the archive is complete. A refusal (a stale snapshot, a base-backed entry the
  /// archive cannot cover) ends the walk; the archiver is then spent.
  pub fn advance(
    &mut self,
    volume: &Volume,
    store: &Store,
    byte_budget: u64,
  ) -> Result<Progress, VfsError> {
    let mut spent: u64 = 0;
    loop {
      if spent >= byte_budget && spent > 0 {
        return Ok(Progress::More);
      }
      let cost = match self.step(volume, store)? {
        Step::Worked(cost) => cost,
        Step::Finished(archive) => return Ok(Progress::Done(archive)),
      };
      spent = spent.saturating_add(cost.max(1));
    }
  }

  /// One unit of the walk: a piece of the file in progress, or the next directory entry, or the close
  /// of a finished directory. Returns the work's cost in bytes.
  fn step(&mut self, volume: &Volume, store: &Store) -> Result<Step, VfsError> {
    if self.file.is_some() {
      return self.step_file(volume, store).map(Step::Worked);
    }
    let Some(frame) = self.frames.last_mut() else {
      return Err(VfsError::StaleHandle); // A spent archiver: nothing left to walk.
    };
    let Some(next) = frame.pending.pop_front() else {
      return self.close_directory();
    };
    let dir = frame.dir;
    let cost = match next.kind {
      Kind::Dir => self.enter_directory(volume, store, dir, next)?,
      Kind::File => self.begin_file(volume, store, next)?,
      Kind::Symlink => self.take_symlink(volume, store, next)?,
    };
    Ok(Step::Worked(cost))
  }

  /// Pops the finished directory and attaches it to its parent, or finishes the archive at the root.
  fn close_directory(&mut self) -> Result<Step, VfsError> {
    let Some(frame) = self.frames.pop() else {
      return Err(VfsError::StaleHandle);
    };
    let node = Node::Directory(frame.built);
    let cost = u64::try_from(self.chunk_bytes).unwrap_or(u64::MAX);
    match self.frames.last_mut() {
      Some(parent) => {
        parent.built.push(Entry {
          name: frame.name,
          meta: frame.meta,
          node,
        });
        Ok(Step::Worked(cost))
      }
      None => Ok(Step::Finished(self.finish(node))),
    }
  }

  /// Enters a subdirectory: resolves its node at the snapshot, records its metadata, and pushes a frame
  /// with its entries.
  fn enter_directory(
    &mut self,
    volume: &Volume,
    store: &Store,
    parent: Handle<DirNode>,
    next: Pending,
  ) -> Result<u64, VfsError> {
    let located = volume.lookup_in(store, parent, &next.name)?;
    let Child::Dir(dir) = located.child else {
      return Err(VfsError::NotDirectory);
    };
    let meta = meta_of(volume, store, self.snapshot, next.inode, MODE_DIRECTORY)?;
    let pending = pending_of(volume, store, dir)?;
    self.frames.push(Frame {
      dir,
      name: next.name,
      meta,
      pending,
      built: Vec::new(),
    });
    Ok(u64::try_from(self.chunk_bytes).unwrap_or(u64::MAX))
  }

  /// Begins a file: refuses a base-backed body (its bytes are not in RAM), records the metadata, and
  /// either finishes an empty file at once or leaves a cursor for the slices that cut its bytes.
  fn begin_file(&mut self, volume: &Volume, store: &Store, next: Pending) -> Result<u64, VfsError> {
    let inode = volume.inode_in(store, self.snapshot, next.inode)?;
    if matches!(inode.body, Body::Base(_)) {
      return Err(VfsError::RecoveryIncomplete);
    }
    let size = inode.attrs.size;
    let meta = node_meta(next.inode, &inode.attrs, MODE_FILE);
    if size == 0 {
      self.push_entry(Entry {
        name: next.name,
        meta,
        node: Node::File(Vec::new()),
      })?;
    } else {
      self.file = Some(FileCursor {
        name: next.name,
        meta,
        inode: next.inode,
        offset: 0,
        size,
        extents: Vec::new(),
      });
    }
    Ok(u64::try_from(self.chunk_bytes).unwrap_or(u64::MAX))
  }

  /// Cuts the next chunk off the file in progress, finishing the file when its bytes are all cut.
  fn step_file(&mut self, volume: &Volume, store: &Store) -> Result<u64, VfsError> {
    let Some(cursor) = self.file.as_mut() else {
      return Ok(0);
    };
    let remaining = cursor.size.saturating_sub(cursor.offset);
    let want = usize::try_from(remaining.min(u64::try_from(self.chunk_bytes).unwrap_or(u64::MAX)))
      .unwrap_or(self.chunk_bytes);
    let mut piece = vec![0u8; want];
    let read = volume.read_in(
      store,
      self.snapshot,
      cursor.inode,
      cursor.offset,
      &mut piece,
    )?;
    // A read short of the size the inode records is a torn snapshot; refuse rather than archive a
    // truncated file under the recorded size.
    if read != want {
      return Err(VfsError::RecoveryIncomplete);
    }
    let len = u64::try_from(read).unwrap_or(u64::MAX);
    let offset = cursor.offset;
    cursor.offset = cursor.offset.saturating_add(len);
    let finished = cursor.offset >= cursor.size;
    let chunk = self.push_chunk(piece);
    if let Some(cursor) = self.file.as_mut() {
      cursor.extents.push(Extent {
        offset,
        len,
        chunk,
        chunk_offset: 0,
      });
    }
    if finished && let Some(cursor) = self.file.take() {
      self.push_entry(Entry {
        name: cursor.name,
        meta: cursor.meta,
        node: Node::File(cursor.extents),
      })?;
    }
    Ok(len)
  }

  /// Carries a symlink as a file of its target bytes under the link type bits.
  fn take_symlink(
    &mut self,
    volume: &Volume,
    store: &Store,
    next: Pending,
  ) -> Result<u64, VfsError> {
    let target = volume.readlink_in(store, self.snapshot, next.inode)?;
    let meta = meta_of(volume, store, self.snapshot, next.inode, MODE_SYMLINK)?;
    let bytes = target.as_bytes().to_vec();
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let extents = if bytes.is_empty() {
      Vec::new()
    } else {
      let chunk = self.push_chunk(bytes);
      vec![Extent {
        offset: 0,
        len,
        chunk,
        chunk_offset: 0,
      }]
    };
    self.push_entry(Entry {
      name: next.name,
      meta,
      node: Node::File(extents),
    })?;
    Ok(
      u64::try_from(self.chunk_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(len),
    )
  }

  /// Adds a built entry to the directory being walked.
  fn push_entry(&mut self, entry: Entry) -> Result<(), VfsError> {
    match self.frames.last_mut() {
      Some(frame) => {
        frame.built.push(entry);
        Ok(())
      }
      None => Err(VfsError::StaleHandle),
    }
  }

  /// Records a raw chunk once by identity and returns that identity.
  fn push_chunk(&mut self, bytes: Vec<u8>) -> [u8; 32] {
    self.bytes_hashed = self
      .bytes_hashed
      .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    let chunk = Archive::raw_chunk(bytes);
    let identity = chunk.identity;
    if self.seen.insert(identity) {
      self.chunks.push(chunk);
    }
    identity
  }

  /// The finished archive: the header fields, the manifest and the chunks in walk order.
  fn finish(&mut self, root: Node) -> Archive {
    let chunk_size = u32::try_from(self.chunk_bytes).unwrap_or(u32::MAX);
    Archive {
      base_page_size: self.base_page_size,
      chunk_min: chunk_size,
      chunk_max: chunk_size,
      created_unix: self.created_unix,
      volume_id: self.volume_id,
      snapshot_id: (u64::from(self.snapshot.index) << u32::BITS)
        | u64::from(self.snapshot.generation),
      name_policy_id: self.name_policy_id,
      unicode_version: UNICODE_VERSION_UNSPECIFIED,
      manifest: root,
      chunks: std::mem::take(&mut self.chunks),
    }
  }
}

/// The outcome of one walk step.
enum Step {
  Worked(u64),
  Finished(Archive),
}

/// The entries of `dir` at the snapshot, in the directory's order, whiteouts skipped (a whiteout
/// hides a base name and has no content of its own).
fn pending_of(
  volume: &Volume,
  store: &Store,
  dir: Handle<DirNode>,
) -> Result<VecDeque<Pending>, VfsError> {
  Ok(
    volume
      .readdir_in(store, dir)?
      .into_iter()
      .map(|row| Pending {
        name: row.name.to_owned(),
        kind: row.kind,
        inode: row.inode,
      })
      .collect(),
  )
}

/// The manifest metadata of `no` at the snapshot, its mode carrying `type_bits`.
fn meta_of(
  volume: &Volume,
  store: &Store,
  snapshot: SnapshotId,
  no: InodeNo,
  type_bits: u32,
) -> Result<NodeMeta, VfsError> {
  let attrs = volume.stat_in(store, snapshot, no)?;
  Ok(node_meta(no, &attrs, type_bits))
}

/// Manifest metadata from a snapshot's attributes: the inode number (identity across renames), the
/// permission bits under `type_bits`, the times as unsigned nanoseconds (a pre-epoch time clamps to
/// zero), the size and link count. Extended attributes are not yet carried (owed with the xattr pass).
fn node_meta(no: InodeNo, attrs: &Attrs, type_bits: u32) -> NodeMeta {
  NodeMeta {
    ino: no.0,
    mode: permissions_of_mode(attrs.mode) | type_bits,
    mtime_ns: u64::try_from(attrs.mtime).unwrap_or(0),
    ctime_ns: u64::try_from(attrs.ctime).unwrap_or(0),
    size: attrs.size,
    nlink: attrs.nlink,
    xattr_flags: 0,
  }
}

#[cfg(test)]
mod tests {
  use slates_archive::restore;
  use slates_mem::arena::ChunkArena;
  use slates_mem::region::Region;

  use super::*;
  use crate::clock::StepClock;
  use crate::quota::Quota;
  use crate::volume::{StoreConfig, VolumeConfig};

  /// Shape: the test store's page — the machine's usual page, so the chunk is sixteen of them.
  const PAGE: usize = 4096;
  /// Shape: table caps ample for the trees below.
  const CAP: usize = 4096;
  /// Shape: the arena's pages — room for a few multi-chunk files.
  const ARENA_PAGES: usize = 256;
  /// Shape: the test volume's quota — well above the bytes written.
  const QUOTA: u64 = 1 << 20;
  /// Shape: the informational volume id and creation time the archive header carries.
  const VOLUME_ID: u64 = 7;
  const CREATED_UNIX: u64 = 1_700_000_000;

  fn store() -> Store {
    let mut arena = ChunkArena::new(PAGE);
    arena
      .add_region(Region::map(PAGE * ARENA_PAGES, PAGE, false).unwrap())
      .unwrap();
    Store::new(
      &StoreConfig {
        page: PAGE,
        cache_line: 64,
        max_dirs: CAP,
        max_inodes: CAP,
        max_chunks: CAP,
        max_dir_blocks: CAP,
        dir_cutover: 4,
      },
      arena,
      0,
    )
  }

  fn volume(store: &mut Store) -> Volume {
    Volume::create(
      store,
      VolumeConfig {
        prefix: 1,
        names: NameEquivalence::Exact,
        quota: Quota::Bounded { limit: QUOTA },
        journal_bytes: 1 << 16,
        clock: Box::new(StepClock::new(0, 1)),
      },
    )
    .unwrap()
  }

  /// Archives `id` to completion in `budget`-byte slices, counting the slices.
  fn archive_in_slices(
    volume: &Volume,
    store: &Store,
    id: SnapshotId,
    budget: u64,
  ) -> (Archive, u32) {
    let mut archiver = SnapshotArchiver::new(volume, store, id, VOLUME_ID, CREATED_UNIX).unwrap();
    let mut slices = 0;
    loop {
      slices += 1;
      match archiver.advance(volume, store, budget).unwrap() {
        Progress::More => continue,
        Progress::Done(archive) => return (archive, slices),
      }
    }
  }

  /// A tree with a nested directory, a multi-chunk file, an empty file and a symlink, so every
  /// node kind and the chunk cut are exercised.
  fn populate(volume: &mut Volume, store: &mut Store) -> Vec<u8> {
    let root = volume.root();
    let src = volume.mkdir(store, root, "src", 0o755).unwrap();
    let main = volume.create_file(store, src, "main.rs", 0o644).unwrap();
    let body: Vec<u8> = (0..(PAGE * 16 * 3 + 5))
      .map(|i| u8::try_from(i % 251).unwrap_or(0))
      .collect();
    volume.write(store, main, 0, &body).unwrap();
    volume.create_file(store, root, "empty", 0o600).unwrap();
    volume.symlink(store, root, "link", "src/main.rs").unwrap();
    body
  }

  /// AC-7.3 / §4.10: an export restores byte-identical — every file's bytes, the directories, the
  /// symlink (as a file under the link type bits) and each entry's mode round-trip through the
  /// archive — and the multi-chunk file cut into more than one distinct chunk (non-vacuous).
  #[test]
  fn an_export_restores_byte_identical_with_modes_and_kinds() {
    let mut store = store();
    let mut volume = volume(&mut store);
    let body = populate(&mut volume, &mut store);
    let id = volume.snapshot(&mut store).unwrap();

    let (archive, _) = archive_in_slices(&volume, &store, id, u64::MAX);
    assert!(
      archive.chunks.len() >= 3,
      "a file of three chunks and more cuts into several distinct chunks: {}",
      archive.chunks.len()
    );
    let restored = restore(&archive).unwrap();
    assert_restored_tree(&restored, &body);
  }

  /// The tree [`populate`] built, as a restore must reproduce it: every file's bytes, the directory, the
  /// symlink as a file under the link type bits, and each entry's kind, permissions and size.
  fn assert_restored_tree(restored: &slates_archive::Restored, body: &[u8]) {
    assert_eq!(
      restored.files.get("src/main.rs").map(Vec::as_slice),
      Some(body)
    );
    assert_eq!(restored.files.get("empty"), Some(&Vec::new()));
    assert_eq!(
      restored.files.get("link").map(|b| b.as_slice()),
      Some(b"src/main.rs".as_slice())
    );
    assert!(restored.directories.contains("src"));
    let meta = |path: &str| restored.metadata.get(path).copied().unwrap();
    let expected = [
      ("src", Some(Kind::Dir), 0o755),
      ("src/main.rs", Some(Kind::File), 0o644),
      ("link", Some(Kind::Symlink), 0o777),
      ("empty", Some(Kind::File), 0o600),
    ];
    for (path, kind, permissions) in expected {
      assert_eq!(kind_of_mode(meta(path).mode), kind, "{path}");
      if kind != Some(Kind::Symlink) {
        assert_eq!(permissions_of_mode(meta(path).mode), permissions, "{path}");
      }
    }
    assert_eq!(meta("src/main.rs").size, u64::try_from(body.len()).unwrap());
  }

  /// D-17 determinism gate: the same snapshot exports to the same archive bytes and manifest identity
  /// every time, whatever the slice budget; a later snapshot with a changed file has another identity.
  #[test]
  fn the_same_snapshot_exports_to_the_same_identity_whatever_the_slicing() {
    let mut store = store();
    let mut volume = volume(&mut store);
    populate(&mut volume, &mut store);
    let id = volume.snapshot(&mut store).unwrap();

    let (whole, one_slice) = archive_in_slices(&volume, &store, id, u64::MAX);
    let (sliced, many_slices) = archive_in_slices(&volume, &store, id, 1);
    assert_eq!(one_slice, 1, "an unbounded budget archives in one slice");
    assert!(
      many_slices > one_slice,
      "a one-byte budget takes many slices: {many_slices}"
    );
    assert_eq!(
      whole.encode(),
      sliced.encode(),
      "the archive bytes are the same"
    );
    assert_eq!(whole.manifest.identity(), sliced.manifest.identity());

    let root = volume.root();
    let src = volume.lookup(&store, root, "src").unwrap();
    let Child::Dir(src) = src.child else {
      panic!("src is a directory");
    };
    let main = volume.lookup(&store, src, "main.rs").unwrap().inode;
    volume.write(&mut store, main, 0, b"changed").unwrap();
    let later = volume.snapshot(&mut store).unwrap();
    let (changed, _) = archive_in_slices(&volume, &store, later, u64::MAX);
    assert_ne!(
      changed.manifest.identity(),
      whole.manifest.identity(),
      "a changed file changes the manifest identity"
    );
    let (again, _) = archive_in_slices(&volume, &store, id, u64::MAX);
    assert_eq!(
      again.manifest.identity(),
      whole.manifest.identity(),
      "the earlier snapshot still exports to its own identity after the head moved"
    );
  }

  /// §4.3 bounded slices: a budget bounds each slice's work — a slice of `budget` bytes hashes at most
  /// about that many bytes more than the slice before — and the walk still completes.
  #[test]
  fn a_slice_hashes_about_its_budget_and_the_walk_completes() {
    let mut store = store();
    let mut volume = volume(&mut store);
    populate(&mut volume, &mut store);
    let id = volume.snapshot(&mut store).unwrap();
    let budget = u64::try_from(PAGE * 16).unwrap();
    let mut archiver = SnapshotArchiver::new(&volume, &store, id, VOLUME_ID, CREATED_UNIX).unwrap();
    let mut before = 0;
    let mut slices = 0;
    let done = loop {
      slices += 1;
      let progress = archiver.advance(&volume, &store, budget).unwrap();
      let hashed = archiver.bytes_hashed();
      assert!(
        hashed - before <= budget * 2,
        "one slice hashes at most its budget plus one unit of work: {}",
        hashed - before
      );
      before = hashed;
      if let Progress::Done(archive) = progress {
        break archive;
      }
    };
    assert!(slices > 1, "the tree took several slices: {slices}");
    assert!(
      done.chunks.len() >= 3 && archiver.bytes_hashed() > 0,
      "the walk did real work"
    );
  }
}
