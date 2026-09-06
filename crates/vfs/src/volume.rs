//! The volume (§4.4, §4.5): the state machine, the namespace operations with POSIX semantics,
//! copy-on-write by birth epoch, snapshots, clones, cooperative destroy, exact accounting and
//! the journal, all on one shard's store.
//!
//! Every mutation follows one shape: resolve the directory node, make its path current (copy
//! nodes born before the head epoch, reporting the originals to the newest snapshot's
//! deadlist), check the POSIX rule, check the quota, apply in place, stamp times, append the
//! journal record. A refusal leaves nothing changed (T-1.1, T-1.9). The executable model in the
//! tests is the specification this file must equal on every generated history.

use std::collections::{BTreeMap, BTreeSet};

use slates_machine::{Derived, derived};
use slates_mem::arena::ChunkArena;
use slates_mem::{Handle, Slab};

use crate::clock::Clock;
use crate::content::{ChunkStore, Extent, ExtentSrc, OpenExtent, inline_bytes};
use crate::dir::{BaseDirState, Child, DirNode};
use crate::dirtree::{DirBlock, Retired};
use crate::error::VfsError;
use crate::ids::{Epoch, InodeNo, SnapshotId};
use crate::inode::{Attrs, Body, Home, Inode, Kind};
use crate::journal::{Op, OpLog};
use crate::names::{self, NameEquivalence};
use crate::quota::{Accounting, Quota};
use crate::snapshot::{Dead, Deadlist, Snapshot};
use crate::trie::{self, TrieNode};

/// Format: POSIX mode bits of a new volume root, `rwxr-xr-x` (the `mkdir` default under the
/// conventional umask 022).
const ROOT_MODE: u32 = 0o755;

/// Format: POSIX symlink mode bits are always `rwxrwxrwx`; the bits of a symlink are unused.
const SYMLINK_MODE: u32 = 0o777;

/// Format: the most snapshot slots a volume can hold, the slot field of a handle (`u32`).
const SNAPSHOT_SLOT_CAP: usize = u32::MAX as usize;

/// Derived: a destroy slice reads the clock once per this many release units, so the clock's
/// cost (measured 20 ns a read on Apple silicon, Phase 0 `timer_overhead_ns`) stays under the
/// cheapest unit's (a trie node, measured 3 ns) spread over the batch, and a slice overshoots
/// its budget by at most sixteen units of the dearest kind.
const DESTROY_CLOCK_EVERY_UNITS: usize = 16;

/// Format: `LINK_MAX` as Linux and macOS report it for ext4 and APFS (65,000 and 32,767); the
/// smaller is the volume's bound so a landing never produces a file the target refuses.
pub const LINK_MAX: u32 = 32_767;

/// The shard's store: slabs and the chunk arena every volume on the shard allocates from.
pub struct Store {
  /// Directory nodes.
  pub dirs: Slab<DirNode>,
  /// The blocks of the indexed directories (§4.5, `dirtree`).
  pub blocks: Slab<DirBlock>,
  /// Inode versions.
  pub inodes: Slab<Inode>,
  /// Inode-table nodes.
  pub tries: Slab<TrieNode>,
  /// Chunks and their bytes.
  pub content: ChunkStore,
  /// The small-directory cut-over.
  pub dir_cutover: usize,
  /// The inline-content threshold.
  pub inline_bytes: usize,
}

impl std::fmt::Debug for Store {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Store")
      .field("dirs", &self.dirs.len())
      .field("inodes", &self.inodes.len())
      .field("chunks", &self.content.chunks())
      .finish()
  }
}

/// How a store is sized.
#[derive(Clone, Debug)]
pub struct StoreConfig {
  /// The base page in bytes.
  pub page: usize,
  /// The cache line in bytes.
  pub cache_line: usize,
  /// Directory nodes the shard may hold.
  pub max_dirs: usize,
  /// Inode versions the shard may hold.
  pub max_inodes: usize,
  /// Chunks the shard may hold.
  pub max_chunks: usize,
  /// Directory blocks the shard may hold.
  pub max_dir_blocks: usize,
  /// The small-directory cut-over (entries).
  pub dir_cutover: usize,
}

impl Store {
  /// A store over `arena`.
  pub fn new(config: &StoreConfig, arena: ChunkArena) -> Self {
    let segment = (config.page / std::mem::size_of::<DirNode>()).max(1);
    Self {
      dirs: Slab::new(segment, config.max_dirs),
      blocks: Slab::new(
        block_segment_slots(config.page).get(),
        config.max_dir_blocks,
      ),
      inodes: Slab::new(
        (config.page / std::mem::size_of::<Inode>()).max(1),
        config.max_inodes,
      ),
      tries: Slab::new(
        (config.page / std::mem::size_of::<TrieNode>()).max(1),
        config.max_inodes,
      ),
      content: ChunkStore::new(arena, config.page, config.max_chunks),
      dir_cutover: config.dir_cutover.max(1),
      inline_bytes: inline_bytes(config.cache_line).get(),
    }
  }
}

/// The volume's lifecycle state (§4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeState {
  /// Serving.
  Live,
  /// Being destroyed in slices.
  Destroying,
  /// Gone.
  Destroyed,
}

/// How a volume is created.
pub struct VolumeConfig {
  /// The inode-number prefix (unique per volume on the host).
  pub prefix: u16,
  /// The name-equivalence policy.
  pub names: NameEquivalence,
  /// The quota.
  pub quota: Quota,
  /// The journal's retention budget in bytes.
  pub journal_bytes: usize,
  /// The clock.
  pub clock: Box<dyn Clock>,
}

/// The volume.
pub struct Volume {
  pub(crate) prefix: u16,
  pub(crate) policy: NameEquivalence,
  pub(crate) clock: Box<dyn Clock>,
  pub(crate) epoch: Epoch,
  pub(crate) root: Handle<DirNode>,
  pub(crate) inode_root: Handle<TrieNode>,
  pub(crate) next_counter: u64,
  pub(crate) snapshots: Slab<Snapshot>,
  pub(crate) last_snapshot: Option<SnapshotId>,
  pub(crate) origin_epoch: Option<Epoch>,
  pub(crate) quota: Quota,
  /// The base plane of an overlay volume (§4.5, D-25); `None` for a scratch volume.
  pub(crate) base: Option<crate::base::BasePlane>,
  /// Head-reachable content bytes by birth epoch; the two public counters are sums of it.
  pub(crate) bytes: ByEpoch,
  pub(crate) journal: OpLog,
  pub(crate) state: VolumeState,
  pub(crate) destroy_queue: Vec<Dead>,
  /// Open and lookup references per inode number (§4.6 lifetime; the inode-addressed-io design):
  /// an inode's content and table entry survive `unlink` while any reference is held, so a file
  /// unlinked while open keeps serving until the last reference drops. Bounded by referenced
  /// inodes; empty for a volume no one holds open.
  pub(crate) references: BTreeMap<InodeNo, u32>,
  /// Inodes that have left the namespace (`nlink == 0`) while still referenced, awaiting
  /// reclamation at their last `unreference`. Bounded by open-unlinked files.
  pub(crate) orphans: BTreeSet<InodeNo>,
}

impl std::fmt::Debug for Volume {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Volume")
      .field("prefix", &self.prefix)
      .field("epoch", &self.epoch)
      .field("state", &self.state)
      .field("accounting", &self.accounting())
      .finish()
  }
}

/// A resolved entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Located {
  /// The child as the directory holds it.
  pub child: Child,
  /// The inode number (the directory's own for a `Dir`).
  pub inode: InodeNo,
}

/// A `readdir` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirRow<'a> {
  /// The name, borrowed from the directory node.
  pub name: &'a str,
  /// The kind.
  pub kind: Kind,
  /// The inode number.
  pub inode: InodeNo,
}

/// Progress of a cooperative destroy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestroyProgress {
  /// Objects released in this slice; more remain.
  Released(usize),
  /// The volume is destroyed.
  Done,
}

impl Volume {
  /// Creates an empty scratch volume with a root directory.
  pub fn create(store: &mut Store, mut config: VolumeConfig) -> Result<Volume, VfsError> {
    let epoch = Epoch(0);
    let root_no = InodeNo::compose(config.prefix, 1);
    let now = config.clock.wall_ns();
    let mut root_inode = Inode::new(root_no, epoch, Kind::Dir, ROOT_MODE, Body::None);
    root_inode.attrs.nlink = 2;
    stamp_all(&mut root_inode.attrs, now);
    let root_handle = store.inodes.insert(root_inode)?;
    let inode_root = trie::new_root(&mut store.tries, epoch)?;
    let mut dead = Deadlist::default();
    let (inode_root, _) = trie::set(
      &mut store.tries,
      inode_root,
      root_no,
      root_handle,
      epoch,
      &mut dead,
    )?;
    let root = store.dirs.insert(DirNode::new(epoch, None, root_no, ""))?;
    store.inodes.get_mut(root_handle)?.body = Body::Directory(root);
    Ok(Volume {
      prefix: config.prefix,
      policy: config.names,
      clock: config.clock,
      epoch,
      root,
      inode_root,
      next_counter: 2,
      snapshots: Slab::new(
        crate::snapshot::initial_capacity(store.content.page()).get(),
        SNAPSHOT_SLOT_CAP,
      ),
      last_snapshot: None,
      origin_epoch: None,
      quota: config.quota,
      base: None,
      bytes: ByEpoch::default(),
      journal: OpLog::new(config.journal_bytes),
      state: VolumeState::Live,
      destroy_queue: Vec::new(),
      references: BTreeMap::new(),
      orphans: BTreeSet::new(),
    })
  }

  /// Clones `snapshot` of `origin` into a new volume sharing its nodes: O(1) (D-5). The clone's
  /// policy is the origin's; its inode counter continues from the origin's so numbers stay
  /// unique across both.
  pub fn clone_of(
    store: &Store,
    origin: &mut Volume,
    snapshot: SnapshotId,
    mut config: VolumeConfig,
  ) -> Result<Volume, VfsError> {
    if config.names != origin.policy {
      return Err(VfsError::PolicyMismatch);
    }
    let snap = origin
      .snapshots
      .get_mut(snapshot_handle(snapshot))
      .map_err(|_| VfsError::StaleHandle)?;
    snap.clone_refs += 1;
    let (root, inode_root, epoch, referenced) = (
      snap.root,
      snap.inode_root,
      snap.epoch,
      snap.referenced_bytes,
    );
    let _ = config.clock.monotonic_ns();
    Ok(Volume {
      prefix: config.prefix,
      policy: config.names,
      clock: config.clock,
      epoch: epoch.next(),
      root,
      inode_root,
      next_counter: origin.next_counter,
      snapshots: Slab::new(
        crate::snapshot::initial_capacity(store.content.page()).get(),
        SNAPSHOT_SLOT_CAP,
      ),
      last_snapshot: None,
      origin_epoch: Some(epoch),
      quota: config.quota,
      base: origin
        .base
        .as_ref()
        .map(|b| b.for_clone(InodeNo::compose(config.prefix, 1))),
      bytes: ByEpoch::inherited(epoch, referenced),
      journal: OpLog::new(config.journal_bytes),
      state: VolumeState::Live,
      destroy_queue: Vec::new(),
      references: BTreeMap::new(),
      orphans: BTreeSet::new(),
    })
  }

  // ------------------------------------------------------------------ queries

  /// The root directory node.
  pub const fn root(&self) -> Handle<DirNode> {
    self.root
  }

  /// The head epoch.
  pub const fn epoch(&self) -> Epoch {
    self.epoch
  }

  /// The lifecycle state.
  pub const fn state(&self) -> VolumeState {
    self.state
  }

  /// Resizes the quota (§4.4 `resize`): refused with `ENOSPC` when the referenced bytes
  /// already exceed the new limit; nothing else changes.
  pub fn resize(&mut self, new_limit: u64) -> Result<(), VfsError> {
    self.live()?;
    self.quota.resize(self.bytes.total(), new_limit)
  }

  /// Growth requests the quota's pressure source refused (pressure events, T-1.5).
  pub fn growth_denials(&self) -> u64 {
    self.quota.denials()
  }

  /// The exact counters (D-13): `referenced_bytes` is every content byte the head reaches;
  /// `unique_bytes` is the part born after the last snapshot (or the clone's origin), which is
  /// what dropping the head would free. Both are sums over the birth-epoch histogram, so they
  /// are exact by construction and a destroyed snapshot needs no recount.
  pub fn accounting(&self) -> Accounting {
    Accounting {
      referenced_bytes: self.bytes.total(),
      unique_bytes: self.bytes.since(self.shared_epoch()),
    }
  }

  /// The newest epoch whose objects the head shares with a snapshot or its clone origin.
  fn shared_epoch(&self) -> Option<Epoch> {
    match (self.last_snapshot_epoch(), self.origin_epoch) {
      (Some(a), Some(b)) => Some(if a.0 >= b.0 { a } else { b }),
      (a, b) => a.or(b),
    }
  }

  /// The name policy.
  pub const fn policy(&self) -> NameEquivalence {
    self.policy
  }

  /// The journal.
  pub const fn op_log(&self) -> &OpLog {
    &self.journal
  }

  /// The number of live snapshots.
  pub fn snapshot_count(&self) -> usize {
    self.snapshots.len()
  }

  /// Looks a name up in a directory.
  pub fn lookup(
    &self,
    store: &Store,
    dir: Handle<DirNode>,
    name: &str,
  ) -> Result<Located, VfsError> {
    let dir = self.head_dir(store, dir)?;
    let node = store.dirs.get(dir).map_err(|_| VfsError::StaleHandle)?;
    let entry = node
      .lookup(&store.blocks, self.policy, name)
      .ok_or(VfsError::NotFound)?;
    let inode = match entry.child {
      Child::Dir(h) => store.dirs.get(h).map_err(|_| VfsError::StaleHandle)?.inode,
      Child::File(no) | Child::Symlink(no) => no,
      Child::Whiteout => return Err(VfsError::NotFound),
    };
    Ok(Located {
      child: entry.child,
      inode,
    })
  }

  /// The attributes of an inode.
  pub fn stat(&self, store: &Store, no: InodeNo) -> Result<Attrs, VfsError> {
    Ok(self.inode(store, no)?.attrs)
  }

  /// The kind of an inode.
  pub fn kind(&self, store: &Store, no: InodeNo) -> Result<Kind, VfsError> {
    Ok(self.inode(store, no)?.kind)
  }

  /// The entries of a directory in canonical order.
  pub fn readdir<'s>(
    &self,
    store: &'s Store,
    dir: Handle<DirNode>,
  ) -> Result<Vec<DirRow<'s>>, VfsError> {
    let dir = self.head_dir(store, dir)?;
    let node = store.dirs.get(dir).map_err(|_| VfsError::StaleHandle)?;
    let mut rows = Vec::with_capacity(node.len());
    for entry in node.iter(&store.blocks) {
      let (kind, inode) = match entry.child {
        Child::Dir(h) => (
          Kind::Dir,
          store.dirs.get(h).map_err(|_| VfsError::StaleHandle)?.inode,
        ),
        Child::File(no) => (Kind::File, no),
        Child::Symlink(no) => (Kind::Symlink, no),
        Child::Whiteout => continue,
      };
      rows.push(DirRow {
        name: entry.name,
        kind,
        inode,
      });
    }
    Ok(rows)
  }

  /// The root directory's inode number, the bridge's node id 1 (§4.6).
  pub fn root_inode(&self, store: &Store) -> Result<InodeNo, VfsError> {
    Ok(
      store
        .dirs
        .get(self.root)
        .map_err(|_| VfsError::StaleHandle)?
        .inode,
    )
  }

  /// Looks `name` up in the directory named by inode number `dir_no` (the bridge speaks inode
  /// numbers, not handles).
  pub fn lookup_no(&self, store: &Store, dir_no: InodeNo, name: &str) -> Result<Located, VfsError> {
    let dir = self.current_dir(store, dir_no)?;
    self.lookup(store, dir, name)
  }

  /// The entries of the directory named by inode number `dir_no`.
  pub fn readdir_no<'s>(
    &self,
    store: &'s Store,
    dir_no: InodeNo,
  ) -> Result<Vec<DirRow<'s>>, VfsError> {
    let dir = self.current_dir(store, dir_no)?;
    self.readdir(store, dir)
  }

  /// Creates a file named `name` in the directory named by inode number `dir_no`.
  pub fn create_file_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
    mode: u32,
  ) -> Result<InodeNo, VfsError> {
    let dir = self.current_dir(store, dir_no)?;
    self.create_file(store, dir, name, mode)
  }

  /// Creates a directory named `name` in the directory named by inode number `dir_no`; the new
  /// directory's inode number.
  pub fn mkdir_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
    mode: u32,
  ) -> Result<InodeNo, VfsError> {
    let dir = self.current_dir(store, dir_no)?;
    let handle = self.mkdir(store, dir, name, mode)?;
    Ok(
      store
        .dirs
        .get(handle)
        .map_err(|_| VfsError::StaleHandle)?
        .inode,
    )
  }

  /// Creates a symlink named `name` (to `target`) in the directory named by `dir_no`.
  pub fn symlink_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
    target: &str,
  ) -> Result<InodeNo, VfsError> {
    let dir = self.current_dir(store, dir_no)?;
    self.symlink(store, dir, name, target)
  }

  /// Unlinks `name` from the directory named by inode number `dir_no`.
  pub fn unlink_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
  ) -> Result<(), VfsError> {
    let dir = self.current_dir(store, dir_no)?;
    self.unlink(store, dir, name)
  }

  /// Removes the directory `name` from the directory named by inode number `dir_no`.
  pub fn rmdir_no(
    &mut self,
    store: &mut Store,
    dir_no: InodeNo,
    name: &str,
  ) -> Result<(), VfsError> {
    let dir = self.current_dir(store, dir_no)?;
    self.rmdir(store, dir, name)
  }

  /// Renames `from_name` under `from_dir_no` to `to_name` under `to_dir_no` (both inode
  /// numbers).
  pub fn rename_no(
    &mut self,
    store: &mut Store,
    from_dir_no: InodeNo,
    from_name: &str,
    to_dir_no: InodeNo,
    to_name: &str,
  ) -> Result<(), VfsError> {
    let from = self.current_dir(store, from_dir_no)?;
    let to = self.current_dir(store, to_dir_no)?;
    self.rename(store, from, from_name, to, to_name)
  }

  /// The target of a symlink.
  pub fn readlink(&self, store: &Store, no: InodeNo) -> Result<Box<str>, VfsError> {
    match &self.inode(store, no)?.body {
      Body::Symlink(target) => Ok(target.clone()),
      _ => Err(VfsError::Invalid),
    }
  }

  /// Reads up to `buf.len()` bytes at `off`; holes read as zeros; returns the bytes read.
  pub fn read(
    &self,
    store: &Store,
    no: InodeNo,
    off: u64,
    buf: &mut [u8],
  ) -> Result<usize, VfsError> {
    let inode = self.inode(store, no)?;
    if inode.kind == Kind::Dir {
      return Err(VfsError::IsDirectory);
    }
    let size = inode.attrs.size;
    if off >= size {
      return Ok(0);
    }
    let want =
      usize::try_from((size - off).min(u64::try_from(buf.len()).unwrap_or(u64::MAX))).unwrap_or(0);
    let out = &mut buf[..want];
    out.fill(0);
    match &inode.body {
      Body::Inline(bytes) => copy_range(bytes, 0, off, out),
      Body::Sealed(extents) => {
        for e in extents {
          if let Some(bytes) = store.content.extent_bytes(e) {
            copy_range(bytes, e.off, off, out);
          }
        }
      }
      Body::Open { open, sealed } => {
        for e in sealed {
          if let Some(bytes) = store.content.extent_bytes(e) {
            copy_range(bytes, e.off, off, out);
          }
        }
        copy_range(store.content.open_bytes(open), open.off, off, out);
      }
      Body::Base(b) => {
        if b.lost {
          return Err(VfsError::BaseDrift);
        }
        // Unpinned disk bytes need the host: `Overlay::read` serves them.
        let unpinned = off < b.base_len
          && !b
            .pinned
            .iter()
            .any(|e| e.off <= off && off + u64::try_from(want).unwrap_or(0) <= e.off + e.len);
        if unpinned {
          return Err(VfsError::BaseUnavailable(0));
        }
        for e in &b.pinned {
          if let Some(bytes) = store.content.extent_bytes(e) {
            copy_range(bytes, e.off, off, out);
          }
        }
      }
      Body::None | Body::Directory(_) | Body::Symlink(_) => {}
    }
    Ok(want)
  }

  // ------------------------------------------------------------------ namespace mutations

  /// Creates an empty file.
  pub fn create_file(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
    mode: u32,
  ) -> Result<InodeNo, VfsError> {
    self.live()?;
    names::check(name)?;
    let dir = self.make_current_dir(store, dir)?;
    if store
      .dirs
      .get(dir)?
      .lookup(&store.blocks, self.policy, name)
      .is_some_and(|e| e.child != Child::Whiteout)
    {
      return Err(VfsError::AlreadyExists);
    }
    let no = self.next_no();
    let now = self.clock.wall_ns();
    let mut inode = Inode::new(no, self.epoch, Kind::File, mode, Body::Inline(Vec::new()));
    inode.home = Some(Home {
      parent: store.dirs.get(dir)?.inode,
      hash: self.policy.hash(name),
    });
    stamp_all(&mut inode.attrs, now);
    let handle = store.inodes.insert(inode)?;
    self.table_set(store, no, handle)?;
    self.dir_insert(store, dir, name, Child::File(no))?;
    self.touch_dir(store, dir, now)?;
    let path = self.path_of(store, dir, name);
    self.record(Op::Create, &path, Some(no), 0);
    Ok(no)
  }

  /// Creates a directory.
  pub fn mkdir(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
    mode: u32,
  ) -> Result<Handle<DirNode>, VfsError> {
    self.live()?;
    names::check(name)?;
    let dir = self.make_current_dir(store, dir)?;
    if store
      .dirs
      .get(dir)?
      .lookup(&store.blocks, self.policy, name)
      .is_some_and(|e| e.child != Child::Whiteout)
    {
      return Err(VfsError::AlreadyExists);
    }
    let no = self.next_no();
    let now = self.clock.wall_ns();
    let parent_no = store.dirs.get(dir)?.inode;
    let child = store
      .dirs
      .insert(DirNode::new(self.epoch, Some(parent_no), no, name))?;
    let mut inode = Inode::new(no, self.epoch, Kind::Dir, mode, Body::Directory(child));
    inode.attrs.nlink = 2;
    stamp_all(&mut inode.attrs, now);
    let handle = store.inodes.insert(inode)?;
    self.table_set(store, no, handle)?;
    self.dir_insert(store, dir, name, Child::Dir(child))?;
    self.touch_dir(store, dir, now)?;
    self.adjust_nlink(store, store.dirs.get(dir)?.inode, 1)?;
    let path = self.path_of(store, dir, name);
    self.record(Op::Mkdir, &path, Some(no), 0);
    Ok(child)
  }

  /// Creates a symlink.
  pub fn symlink(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
    target: &str,
  ) -> Result<InodeNo, VfsError> {
    self.live()?;
    names::check(name)?;
    let dir = self.make_current_dir(store, dir)?;
    if store
      .dirs
      .get(dir)?
      .lookup(&store.blocks, self.policy, name)
      .is_some_and(|e| e.child != Child::Whiteout)
    {
      return Err(VfsError::AlreadyExists);
    }
    let no = self.next_no();
    let now = self.clock.wall_ns();
    let mut inode = Inode::new(
      no,
      self.epoch,
      Kind::Symlink,
      SYMLINK_MODE,
      Body::Symlink(target.into()),
    );
    inode.attrs.size = u64::try_from(target.len()).unwrap_or(0);
    inode.home = Some(Home {
      parent: store.dirs.get(dir)?.inode,
      hash: self.policy.hash(name),
    });
    stamp_all(&mut inode.attrs, now);
    let handle = store.inodes.insert(inode)?;
    self.table_set(store, no, handle)?;
    self.dir_insert(store, dir, name, Child::Symlink(no))?;
    self.touch_dir(store, dir, now)?;
    let path = self.path_of(store, dir, name);
    self.record(Op::Symlink, &path, Some(no), 0);
    Ok(no)
  }

  /// Creates a hard link to a file or symlink (`EPERM` for a directory).
  pub fn link(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
    target: InodeNo,
  ) -> Result<(), VfsError> {
    self.live()?;
    names::check(name)?;
    let kind = self.kind(store, target)?;
    if kind == Kind::Dir {
      return Err(VfsError::NotPermitted);
    }
    if self.inode(store, target)?.attrs.nlink >= LINK_MAX {
      return Err(VfsError::TooManyLinks);
    }
    let dir = self.make_current_dir(store, dir)?;
    if store
      .dirs
      .get(dir)?
      .lookup(&store.blocks, self.policy, name)
      .is_some_and(|e| e.child != Child::Whiteout)
    {
      return Err(VfsError::AlreadyExists);
    }
    let now = self.clock.wall_ns();
    let child = if kind == Kind::Symlink {
      Child::Symlink(target)
    } else {
      Child::File(target)
    };
    self.dir_insert(store, dir, name, child)?;
    self.adjust_nlink(store, target, 1)?;
    // From now on the inode has had more than one path; the deriver walks for them.
    let handle = self.make_current_inode(store, target)?;
    store.inodes.get_mut(handle)?.multi = true;
    self.touch_dir(store, dir, now)?;
    let path = self.path_of(store, dir, name);
    let prev = self.inode(store, target)?.version;
    self.record(Op::Link, &path, Some(target), prev);
    Ok(())
  }

  /// Unlinks a file or symlink (`EISDIR` for a directory).
  pub fn unlink(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
  ) -> Result<(), VfsError> {
    self.live()?;
    let located = self.lookup(store, dir, name)?;
    if matches!(located.child, Child::Dir(_)) {
      return Err(VfsError::IsDirectory);
    }
    let dir = self.make_current_dir(store, dir)?;
    let now = self.clock.wall_ns();
    let path = self.path_of(store, dir, name);
    self.dir_remove(store, dir, name)?;
    self.drop_link(store, located.inode)?;
    self.touch_dir(store, dir, now)?;
    self.record(Op::Unlink, &path, Some(located.inode), 0);
    self.record_whiteout(store, dir, name, &path)?;
    Ok(())
  }

  /// Removes an empty directory.
  pub fn rmdir(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
  ) -> Result<(), VfsError> {
    self.live()?;
    let located = self.lookup(store, dir, name)?;
    let Child::Dir(child) = located.child else {
      return Err(VfsError::NotDirectory);
    };
    if !self.empty_for_rmdir(store, child)? {
      return Err(VfsError::NotEmpty);
    }
    let dir = self.make_current_dir(store, dir)?;
    let now = self.clock.wall_ns();
    let path = self.path_of(store, dir, name);
    self.dir_remove(store, dir, name)?;
    self.release_dir_node(store, child)?;
    self.drop_link(store, located.inode)?;
    self.drop_link(store, located.inode)?;
    self.adjust_nlink(store, store.dirs.get(dir)?.inode, -1)?;
    self.touch_dir(store, dir, now)?;
    self.record(Op::Rmdir, &path, Some(located.inode), 0);
    self.record_whiteout(store, dir, name, &path)?;
    Ok(())
  }

  /// Renames with POSIX semantics: a file may replace a file, a directory an empty directory;
  /// a directory may not move into its own subtree (`EINVAL`); nothing changes on refusal.
  pub fn rename(
    &mut self,
    store: &mut Store,
    from_dir: Handle<DirNode>,
    from_name: &str,
    to_dir: Handle<DirNode>,
    to_name: &str,
  ) -> Result<(), VfsError> {
    self.live()?;
    names::check(to_name)?;
    let source = self.lookup(store, from_dir, from_name)?;
    let target = self.lookup(store, to_dir, to_name).ok();
    if from_dir == to_dir && self.policy.same(from_name, to_name) {
      return Ok(());
    }
    if let Some(t) = target
      && t.inode == source.inode
    {
      // Two names of one inode: POSIX says do nothing.
      return Ok(());
    }
    if let Child::Dir(moving) = source.child {
      if self.is_ancestor(store, moving, to_dir)? {
        return Err(VfsError::Invalid);
      }
      match target.map(|t| t.child) {
        None => {}
        Some(Child::Dir(existing)) => {
          if !store.dirs.get(existing)?.is_empty() {
            return Err(VfsError::NotEmpty);
          }
        }
        Some(_) => return Err(VfsError::NotDirectory),
      }
    } else if matches!(target.map(|t| t.child), Some(Child::Dir(_))) {
      return Err(VfsError::IsDirectory);
    }
    let from_path = self.path_of(store, from_dir, from_name);
    let from_dir = self.make_current_dir(store, from_dir)?;
    let to_dir = self.make_current_dir(store, to_dir)?;
    let now = self.clock.wall_ns();
    // Remove the target first, then move the entry.
    if let Some(t) = target {
      self.dir_remove(store, to_dir, to_name)?;
      match t.child {
        Child::Dir(existing) => {
          self.release_dir_node(store, existing)?;
          self.drop_link(store, t.inode)?;
          self.drop_link(store, t.inode)?;
          self.adjust_nlink(store, store.dirs.get(to_dir)?.inode, -1)?;
        }
        Child::File(_) | Child::Symlink(_) => self.drop_link(store, t.inode)?,
        Child::Whiteout => {}
      }
    }
    let moved = self.dir_remove(store, from_dir, from_name)?;
    let moved_child = match moved {
      Child::Dir(moving) => {
        let moving = self.make_current_dir_node(store, moving)?;
        let to_no = store.dirs.get(to_dir)?.inode;
        let node = store.dirs.get_mut(moving)?;
        node.parent = Some(to_no);
        node.name = to_name.into();
        if from_dir != to_dir {
          self.adjust_nlink(store, store.dirs.get(from_dir)?.inode, -1)?;
          self.adjust_nlink(store, store.dirs.get(to_dir)?.inode, 1)?;
        }
        Child::Dir(moving)
      }
      Child::File(no) | Child::Symlink(no) => {
        // The file's home follows it.
        let to_no = store.dirs.get(to_dir)?.inode;
        let handle = self.make_current_inode(store, no)?;
        store.inodes.get_mut(handle)?.home = Some(Home {
          parent: to_no,
          hash: self.policy.hash(to_name),
        });
        moved
      }
      Child::Whiteout => moved,
    };
    self.dir_insert(store, to_dir, to_name, moved_child)?;
    self.touch_dir(store, from_dir, now)?;
    self.touch_dir(store, to_dir, now)?;
    let to_path = self.path_of(store, to_dir, to_name);
    self.record(
      Op::Rename {
        from: from_path.into(),
      },
      &to_path,
      Some(source.inode),
      0,
    );
    Ok(())
  }

  // ------------------------------------------------------------------ content mutations

  /// Writes `bytes` at `off`; refuses with `ENOSPC` before touching anything when the quota
  /// would be exceeded; returns the bytes written (all of them).
  pub fn write(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    off: u64,
    bytes: &[u8],
  ) -> Result<usize, VfsError> {
    self.live()?;
    if bytes.is_empty() {
      return Ok(0);
    }
    let end = off
      .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
      .ok_or(VfsError::FileTooLarge)?;
    let kind = self.kind(store, no)?;
    if kind == Kind::Dir {
      return Err(VfsError::IsDirectory);
    }
    let old_size = self.inode(store, no)?.attrs.size;
    let charge = self.write_charge(store, no, off, end)?;
    if !self.quota.admit(self.bytes.total(), charge) {
      return Err(VfsError::NoSpace);
    }
    let handle = self.make_current_inode(store, no)?;
    let prev_version = store.inodes.get(handle)?.version;
    let path_ino = no;
    self.apply_write(store, handle, off, bytes)?;
    let now = self.clock.wall_ns();
    let inode = store.inodes.get_mut(handle)?;
    inode.attrs.size = inode.attrs.size.max(end);
    inode.attrs.mtime = now;
    inode.attrs.ctime = now;
    inode.version += 1;
    let op = if off >= old_size {
      Op::Extend {
        at: old_size,
        len: end - old_size,
      }
    } else {
      Op::Overwrite {
        at: off,
        len: u64::try_from(bytes.len()).unwrap_or(0),
      }
    };
    self.record(op, "", Some(path_ino), prev_version);
    Ok(bytes.len())
  }

  /// Truncates (or extends with a hole) to `len`.
  pub fn truncate(&mut self, store: &mut Store, no: InodeNo, len: u64) -> Result<(), VfsError> {
    self.live()?;
    if self.kind(store, no)? == Kind::Dir {
      return Err(VfsError::IsDirectory);
    }
    let handle = self.make_current_inode(store, no)?;
    let prev_version = store.inodes.get(handle)?.version;
    self.apply_truncate(store, handle, len)?;
    let now = self.clock.wall_ns();
    let inode = store.inodes.get_mut(handle)?;
    inode.attrs.size = len;
    inode.attrs.mtime = now;
    inode.attrs.ctime = now;
    inode.version += 1;
    self.record(Op::Truncate { len }, "", Some(no), prev_version);
    Ok(())
  }

  /// Sets the mode (the ownership fields follow the same path).
  pub fn chmod(&mut self, store: &mut Store, no: InodeNo, mode: u32) -> Result<(), VfsError> {
    self.live()?;
    let handle = self.make_current_inode(store, no)?;
    let now = self.clock.wall_ns();
    let inode = store.inodes.get_mut(handle)?;
    let prev = inode.version;
    inode.attrs.mode = mode;
    inode.attrs.ctime = now;
    inode.version += 1;
    self.record(Op::Setattr, "", Some(no), prev);
    Ok(())
  }

  /// Sets the owner uid and gid (a `chown`), so a bridge honors a `setattr` of ownership instead
  /// of ignoring it (§4.6 "never acknowledge an ignored setattr field"). Copy-on-write; the
  /// change time advances, as POSIX requires for an attribute change.
  pub fn chown(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    uid: u32,
    gid: u32,
  ) -> Result<(), VfsError> {
    self.live()?;
    let handle = self.make_current_inode(store, no)?;
    let now = self.clock.wall_ns();
    let inode = store.inodes.get_mut(handle)?;
    let prev = inode.version;
    inode.attrs.uid = uid;
    inode.attrs.gid = gid;
    inode.attrs.ctime = now;
    inode.version += 1;
    self.record(Op::Setattr, "", Some(no), prev);
    Ok(())
  }

  /// The volume's current wall-clock time, in nanoseconds since the Unix epoch. A transport uses it
  /// to resolve a "set to now" request (NFS `SET_TO_SERVER_TIME`, FUSE `UTIME_NOW`) into the
  /// explicit value [`Volume::set_times`] takes — the NOW resolution the design places at the
  /// transport (AC-3.10).
  pub fn wall_ns(&mut self) -> i64 {
    self.clock.wall_ns()
  }

  /// Sets the access and modification times (a `utimens`), so a bridge honors a `setattr` of times
  /// instead of ignoring it (§4.6). Times are nanoseconds since the Unix epoch. Copy-on-write; the
  /// change time advances. `UTIME_NOW`/`UTIME_OMIT` resolution belongs to the transport that
  /// carries those flags (owed, AC-3.10); this takes explicit values.
  pub fn set_times(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    atime: i64,
    mtime: i64,
  ) -> Result<(), VfsError> {
    self.live()?;
    let handle = self.make_current_inode(store, no)?;
    let now = self.clock.wall_ns();
    let inode = store.inodes.get_mut(handle)?;
    let prev = inode.version;
    inode.attrs.atime = atime;
    inode.attrs.mtime = mtime;
    inode.attrs.ctime = now;
    inode.version += 1;
    self.record(Op::Setattr, "", Some(no), prev);
    Ok(())
  }

  // ------------------------------------------------------------------ snapshots and clones

  /// Takes a snapshot: one record; O(1).
  pub fn snapshot(&mut self, store: &mut Store) -> Result<SnapshotId, VfsError> {
    self.live()?;
    let _ = store;
    let record = Snapshot {
      epoch: self.epoch,
      root: self.root,
      inode_root: self.inode_root,
      deadlist: Deadlist::default(),
      clone_refs: 0,
      previous: self.last_snapshot,
      next: None,
      referenced_bytes: self.bytes.total(),
      seq: self.journal.head_seq(),
      identity: None,
    };
    let handle = self.snapshots.insert(record)?;
    let id = SnapshotId {
      index: handle.index(),
      generation: handle.generation(),
    };
    if let Some(previous) = self.last_snapshot
      && let Ok(p) = self.snapshots.get_mut(snapshot_handle(previous))
    {
      p.next = Some(id);
    }
    self.last_snapshot = Some(id);
    self.epoch = self.epoch.next();
    self.record(Op::Snapshot, "", None, 0);
    Ok(id)
  }

  /// A snapshot's frozen epoch and roots.
  pub fn snapshot_info(&self, id: SnapshotId) -> Result<(Epoch, Handle<DirNode>), VfsError> {
    let s = self
      .snapshots
      .get(snapshot_handle(id))
      .map_err(|_| VfsError::StaleHandle)?;
    Ok((s.epoch, s.root))
  }

  /// Looks a name up in a directory of a snapshot's tree (the same walk over frozen nodes).
  pub fn lookup_in(
    &self,
    store: &Store,
    dir: Handle<DirNode>,
    name: &str,
  ) -> Result<Located, VfsError> {
    // The given node, as it was: never resolved to the head's current node.
    let node = store.dirs.get(dir).map_err(|_| VfsError::StaleHandle)?;
    let entry = node
      .lookup(&store.blocks, self.policy, name)
      .ok_or(VfsError::NotFound)?;
    let inode = match entry.child {
      Child::Dir(h) => store.dirs.get(h).map_err(|_| VfsError::StaleHandle)?.inode,
      Child::File(no) | Child::Symlink(no) => no,
      Child::Whiteout => return Err(VfsError::NotFound),
    };
    Ok(Located {
      child: entry.child,
      inode,
    })
  }

  /// Reads from an inode as a snapshot's inode table has it.
  pub fn read_in(
    &self,
    store: &Store,
    id: SnapshotId,
    no: InodeNo,
    off: u64,
    buf: &mut [u8],
  ) -> Result<usize, VfsError> {
    let s = self
      .snapshots
      .get(snapshot_handle(id))
      .map_err(|_| VfsError::StaleHandle)?;
    let handle = trie::get(&store.tries, s.inode_root, no).ok_or(VfsError::NotFound)?;
    let inode = store.inodes.get(handle)?;
    let size = inode.attrs.size;
    if off >= size {
      return Ok(0);
    }
    let want =
      usize::try_from((size - off).min(u64::try_from(buf.len()).unwrap_or(u64::MAX))).unwrap_or(0);
    let out = &mut buf[..want];
    out.fill(0);
    match &inode.body {
      Body::Inline(bytes) => copy_range(bytes, 0, off, out),
      Body::Sealed(extents) => {
        for e in extents {
          if let Some(bytes) = store.content.extent_bytes(e) {
            copy_range(bytes, e.off, off, out);
          }
        }
      }
      Body::Open { open, sealed } => {
        for e in sealed {
          if let Some(bytes) = store.content.extent_bytes(e) {
            copy_range(bytes, e.off, off, out);
          }
        }
        copy_range(store.content.open_bytes(open), open.off, off, out);
      }
      _ => {}
    }
    Ok(want)
  }

  /// A symlink's target as a snapshot holds it.
  pub fn readlink_in(
    &self,
    store: &Store,
    id: SnapshotId,
    no: InodeNo,
  ) -> Result<Box<str>, VfsError> {
    match &self.inode_in(store, id, no)?.body {
      Body::Symlink(target) => Ok(target.clone()),
      _ => Err(VfsError::Invalid),
    }
  }

  /// The attributes of an inode as a snapshot holds them.
  pub fn stat_in(&self, store: &Store, id: SnapshotId, no: InodeNo) -> Result<Attrs, VfsError> {
    Ok(self.inode_in(store, id, no)?.attrs)
  }

  /// The inode record a snapshot holds for `no`.
  pub(crate) fn inode_in<'s>(
    &self,
    store: &'s Store,
    id: SnapshotId,
    no: InodeNo,
  ) -> Result<&'s Inode, VfsError> {
    let snap = self
      .snapshots
      .get(snapshot_handle(id))
      .map_err(|_| VfsError::StaleHandle)?;
    let handle = trie::get(&store.tries, snap.inode_root, no).ok_or(VfsError::NotFound)?;
    store.inodes.get(handle).map_err(VfsError::from)
  }

  /// Resolves an absolute path in a snapshot.
  pub fn resolve_in(&self, store: &Store, id: SnapshotId, path: &str) -> Result<Located, VfsError> {
    let (_, root) = self.snapshot_info(id)?;
    let mut last = Located {
      child: Child::Dir(root),
      inode: store.dirs.get(root)?.inode,
    };
    for part in path.split('/').filter(|p| !p.is_empty()) {
      let Child::Dir(d) = last.child else {
        return Err(VfsError::NotDirectory);
      };
      last = self.lookup_in(store, d, part)?;
    }
    Ok(last)
  }

  /// The path of a file or symlink inode in the head, through its home; `None` when the inode
  /// has no home (a directory, or a link the home does not name) or the home's entry is gone.
  pub fn path_of_inode(&self, store: &Store, no: InodeNo) -> Option<String> {
    let inode = self.inode(store, no).ok()?;
    let home = inode.home?;
    let dir = self.current_dir(store, home.parent).ok()?;
    let node = store.dirs.get(dir).ok()?;
    let name = node.name_of(&store.blocks, home.hash, no)?;
    Some(self.path_of(store, dir, name))
  }

  /// The path of a file or symlink inode in a snapshot, through the home it had then.
  pub fn path_of_inode_in(&self, store: &Store, id: SnapshotId, no: InodeNo) -> Option<String> {
    let inode = self.inode_in(store, id, no).ok()?;
    let home = inode.home?;
    let parent = self.inode_in(store, id, home.parent).ok()?;
    let Body::Directory(dir) = parent.body else {
      return None;
    };
    let node = store.dirs.get(dir).ok()?;
    let name = node.name_of(&store.blocks, home.hash, no)?;
    let mut parts: Vec<Box<str>> = vec![name.into()];
    let mut current = dir;
    let mut guard = 0usize;
    while let Ok(node) = store.dirs.get(current) {
      let Some(parent_no) = node.parent else { break };
      parts.push(node.name.clone());
      let Ok(p) = self.inode_in(store, id, parent_no) else {
        break;
      };
      let Body::Directory(d) = p.body else { break };
      current = d;
      guard += 1;
      if guard > usize::from(u16::MAX) {
        break;
      }
    }
    parts.reverse();
    Some(format!("/{}", parts.join("/")))
  }

  /// The path of a directory inode as a snapshot held it, through the node's own name and
  /// parent chain; `None` when the snapshot did not hold it.
  pub fn path_of_dir_in(&self, store: &Store, id: SnapshotId, no: InodeNo) -> Option<String> {
    let inode = self.inode_in(store, id, no).ok()?;
    let Body::Directory(dir) = inode.body else {
      return None;
    };
    let mut parts: Vec<Box<str>> = Vec::new();
    let mut current = dir;
    let mut guard = 0usize;
    while let Ok(node) = store.dirs.get(current) {
      let Some(parent_no) = node.parent else { break };
      parts.push(node.name.clone());
      let Ok(p) = self.inode_in(store, id, parent_no) else {
        break;
      };
      let Body::Directory(d) = p.body else { break };
      current = d;
      guard += 1;
      if guard > usize::from(u16::MAX) {
        break;
      }
    }
    parts.reverse();
    Some(format!("/{}", parts.join("/")))
  }

  /// Every path of an inode under `root` (a full walk: the fallback for inodes that have had
  /// more than one link, or whose home no longer names them).
  pub fn paths_of_inode_walk(
    &self,
    store: &Store,
    root: Handle<DirNode>,
    no: InodeNo,
  ) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(String::new(), root)];
    while let Some((prefix, dir)) = stack.pop() {
      let Ok(node) = store.dirs.get(dir) else {
        continue;
      };
      for e in node.iter(&store.blocks) {
        match e.child {
          Child::Dir(h) => stack.push((format!("{prefix}/{}", e.name), h)),
          Child::File(n) | Child::Symlink(n) if n == no => out.push(format!("{prefix}/{}", e.name)),
          _ => {}
        }
      }
    }
    out.sort();
    out
  }

  /// The SDK edit (§4.16): removes `delete_len` bytes at `at` and inserts `bytes` there, with
  /// the rest of the file shifting; declared as a `Delete` and an `Insert` with true positions.
  /// `at` past the end is refused (`EINVAL`). The tail after the edit is rewritten, so an edit
  /// costs the tail's bytes; the merge's splice by extent surgery (Phase 6) is the zero-copy
  /// path for versions, not for a work volume's edits.
  pub fn edit(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    at: u64,
    delete_len: u64,
    bytes: &[u8],
  ) -> Result<(), VfsError> {
    self.live()?;
    if self.kind(store, no)? == Kind::Dir {
      return Err(VfsError::IsDirectory);
    }
    let size = self.inode(store, no)?.attrs.size;
    if at > size {
      return Err(VfsError::Invalid);
    }
    let delete_len = delete_len.min(size - at);
    let tail_len = usize::try_from(size - at - delete_len).map_err(|_| VfsError::FileTooLarge)?;
    let mut tail = vec![0u8; tail_len];
    let read = self.read(store, no, at + delete_len, &mut tail)?;
    tail.truncate(read);
    let inserted = u64::try_from(bytes.len()).map_err(|_| VfsError::FileTooLarge)?;
    let end = at
      .checked_add(inserted)
      .and_then(|e| e.checked_add(u64::try_from(tail.len()).unwrap_or(u64::MAX)))
      .ok_or(VfsError::FileTooLarge)?;
    let charge = self.write_charge(store, no, at, end)?;
    if !self.quota.admit(self.bytes.total(), charge) {
      return Err(VfsError::NoSpace);
    }
    let handle = self.make_current_inode(store, no)?;
    let prev_version = store.inodes.get(handle)?.version;
    self.apply_truncate(store, handle, at)?;
    store.inodes.get_mut(handle)?.attrs.size = at;
    if !bytes.is_empty() {
      self.apply_write(store, handle, at, bytes)?;
    }
    if !tail.is_empty() {
      self.apply_write(store, handle, at + inserted, &tail)?;
    }
    let now = self.clock.wall_ns();
    let inode = store.inodes.get_mut(handle)?;
    inode.attrs.size = end;
    inode.attrs.mtime = now;
    inode.attrs.ctime = now;
    inode.version += 1;
    if delete_len > 0 {
      self.record(
        Op::Delete {
          at,
          len: delete_len,
        },
        "",
        Some(no),
        prev_version,
      );
    }
    if inserted > 0 {
      self.record(Op::Insert { at, len: inserted }, "", Some(no), prev_version);
    }
    Ok(())
  }

  /// Whether an inode has ever had more than one link (its home then names one path).
  pub fn inode_multi(&self, store: &Store, no: InodeNo) -> Result<bool, VfsError> {
    Ok(self.inode(store, no)?.multi)
  }

  /// [`Volume::inode_multi`] as a snapshot holds it.
  pub fn inode_multi_in(
    &self,
    store: &Store,
    id: SnapshotId,
    no: InodeNo,
  ) -> Result<bool, VfsError> {
    Ok(self.inode_in(store, id, no)?.multi)
  }

  /// The entries of a directory node as a snapshot holds it (never the head's current node).
  pub fn readdir_in<'s>(
    &self,
    store: &'s Store,
    dir: Handle<DirNode>,
  ) -> Result<Vec<DirRow<'s>>, VfsError> {
    let node = store.dirs.get(dir).map_err(|_| VfsError::StaleHandle)?;
    let mut rows = Vec::with_capacity(node.len());
    for entry in node.iter(&store.blocks) {
      let (kind, inode) = match entry.child {
        Child::Dir(h) => (
          Kind::Dir,
          store.dirs.get(h).map_err(|_| VfsError::StaleHandle)?.inode,
        ),
        Child::File(no) => (Kind::File, no),
        Child::Symlink(no) => (Kind::Symlink, no),
        Child::Whiteout => continue,
      };
      rows.push(DirRow {
        name: entry.name,
        kind,
        inode,
      });
    }
    Ok(rows)
  }

  /// The ops document of the work since `base` (§4.16; [`crate::derive`]).
  pub fn derive(
    &self,
    store: &Store,
    base: SnapshotId,
  ) -> Result<crate::derive::OpsDocument, VfsError> {
    crate::derive::derive(self, store, base)
  }

  /// The op log records after a snapshot (the deriver's input).
  pub fn records_since(&self, id: SnapshotId) -> Result<Vec<crate::journal::OpRecord>, VfsError> {
    let snap = self
      .snapshots
      .get(snapshot_handle(id))
      .map_err(|_| VfsError::StaleHandle)?;
    let seq = snap.seq;
    Ok(self.journal.since(seq).cloned().collect())
  }

  /// Releases one clone's pin on a snapshot. The owner of both volumes (the shard, §4.5) calls
  /// this when a clone's destroy has completed; the volume core keeps the count only.
  pub fn unpin(&mut self, id: SnapshotId) -> Result<(), VfsError> {
    let snap = self
      .snapshots
      .get_mut(snapshot_handle(id))
      .map_err(|_| VfsError::StaleHandle)?;
    if snap.clone_refs == 0 {
      return Err(VfsError::Invalid);
    }
    snap.clone_refs -= 1;
    Ok(())
  }

  /// Destroys a snapshot: its dead objects go to the previous snapshot if that one still
  /// shares them, else they are released; a snapshot pinned by a clone is refused.
  pub fn destroy_snapshot(&mut self, store: &mut Store, id: SnapshotId) -> Result<(), VfsError> {
    let handle = snapshot_handle(id);
    let snap = self
      .snapshots
      .get(handle)
      .map_err(|_| VfsError::StaleHandle)?;
    if snap.clone_refs > 0 {
      return Err(VfsError::Pinned);
    }
    let previous = snap.previous;
    let next = snap.next;
    let prev_epoch =
      previous.and_then(|p| self.snapshots.get(snapshot_handle(p)).ok().map(|s| s.epoch));
    let removed = self
      .snapshots
      .remove(handle)
      .map_err(|_| VfsError::StaleHandle)?;
    // Relink the neighbours around the removed record.
    if let Some(n) = next
      && let Ok(s) = self.snapshots.get_mut(snapshot_handle(n))
    {
      s.previous = previous;
    }
    if let Some(p) = previous
      && let Ok(s) = self.snapshots.get_mut(snapshot_handle(p))
    {
      s.next = next;
    }
    if self.last_snapshot == Some(id) {
      self.last_snapshot = previous;
    }
    for dead in removed.deadlist.items() {
      match prev_epoch {
        Some(pe) if dead.born() <= pe => {
          if let Some(p) = previous
            && let Ok(ps) = self.snapshots.get_mut(snapshot_handle(p))
          {
            ps.deadlist.push(*dead);
          }
        }
        _ => match self.origin_epoch {
          Some(origin) if dead.born() <= origin => {}
          _ => {
            release_dead(store, *dead)?;
          }
        },
      }
    }
    Ok(())
  }

  /// Begins destroying the volume; the deadlists and the head's own objects are released in
  /// cooperative slices by [`Volume::destroy_step`].
  pub fn destroy(&mut self, store: &mut Store) -> Result<(), VfsError> {
    if self.state == VolumeState::Destroyed {
      return Err(VfsError::Destroying);
    }
    self.state = VolumeState::Destroying;
    let mut queue = Vec::new();
    let snapshots: Vec<SnapshotId> = self
      .snapshots
      .iter()
      .map(|(h, _)| SnapshotId {
        index: h.index(),
        generation: h.generation(),
      })
      .collect();
    for id in snapshots {
      if let Ok(s) = self.snapshots.remove(snapshot_handle(id)) {
        queue.extend(s.deadlist.items().iter().copied());
      }
    }
    self.last_snapshot = None;
    // The head's own objects: every node, inode version and chunk it reaches.
    // A clone walks only what it made: nodes born after its origin's epoch.
    let since = self.origin_epoch;
    let mut dirs = Vec::new();
    let mut blocks = Vec::new();
    collect_dirs(store, self.root, since, &mut dirs, &mut blocks);
    for d in &dirs {
      if let Ok(node) = store.dirs.get(*d) {
        queue.push(Dead::Dir(*d, node.born));
      }
    }
    for (block, born) in blocks {
      queue.push(Dead::DirBlock(block, born));
    }
    let mut inodes = Vec::new();
    trie::walk_since(&store.tries, self.inode_root, since, &mut inodes);
    for h in inodes {
      if let Ok(inode) = store.inodes.get(h) {
        queue.push(Dead::Inode(h, inode.born));
      }
    }
    let mut tries = Vec::new();
    trie::nodes_under_since(&store.tries, self.inode_root, since, &mut tries);
    for t in tries {
      if let Ok(n) = store.tries.get(t) {
        queue.push(Dead::Trie(t, n.born));
      }
    }
    self.destroy_queue = queue;
    Ok(())
  }

  /// Releases objects of a destroy in progress for `budget_ns` of the volume's clock (the
  /// shard's per-iteration budget, AC-1.8), checking the clock every few release units; objects
  /// shared with a clone origin are skipped; call until `Done`.
  pub fn destroy_step(
    &mut self,
    store: &mut Store,
    budget_ns: u64,
  ) -> Result<DestroyProgress, VfsError> {
    if self.state != VolumeState::Destroying {
      return Err(VfsError::Destroying);
    }
    let started = self.clock.monotonic_ns();
    let mut released = 0;
    let mut since_check = 0;
    loop {
      let Some(dead) = self.destroy_queue.pop() else {
        // The queue emptied in this slice: report its units; the next call says `Done`.
        if released > 0 {
          return Ok(DestroyProgress::Released(released));
        }
        self.state = VolumeState::Destroyed;
        self.bytes = ByEpoch::default();
        return Ok(DestroyProgress::Done);
      };
      let shared = self
        .origin_epoch
        .is_some_and(|origin| dead.born() <= origin);
      let weight = if shared {
        1
      } else {
        release_dead(store, dead)?
      };
      released += weight;
      since_check += weight;
      if since_check >= DESTROY_CLOCK_EVERY_UNITS {
        since_check = 0;
        if self.clock.monotonic_ns().saturating_sub(started) >= budget_ns {
          break;
        }
      }
    }
    Ok(DestroyProgress::Released(released))
  }

  /// Objects still queued for a destroy in progress.
  pub fn destroy_pending(&self) -> usize {
    self.destroy_queue.len()
  }

  // ------------------------------------------------------------------ paths (tests, journal)

  /// Resolves a `/`-separated path from the root.
  pub fn resolve(&self, store: &Store, path: &str) -> Result<Located, VfsError> {
    let mut dir = self.root;
    let mut last = Located {
      child: Child::Dir(dir),
      inode: store
        .dirs
        .get(dir)
        .map_err(|_| VfsError::StaleHandle)?
        .inode,
    };
    for part in path.split('/').filter(|p| !p.is_empty()) {
      let Child::Dir(d) = last.child else {
        return Err(VfsError::NotDirectory);
      };
      dir = d;
      last = self.lookup(store, dir, part)?;
    }
    Ok(last)
  }

  /// The path of `name` under `dir`, for the journal.
  pub fn path_of(&self, store: &Store, dir: Handle<DirNode>, name: &str) -> String {
    let mut parts: Vec<Box<str>> = vec![name.into()];
    let mut current = dir;
    let mut guard = 0usize;
    while let Ok(node) = store.dirs.get(current) {
      let Some(parent_no) = node.parent else { break };
      parts.push(node.name.clone());
      let Ok(parent) = self.current_dir(store, parent_no) else {
        break;
      };
      current = parent;
      guard += 1;
      if guard > usize::from(u16::MAX) {
        break;
      }
    }
    parts.reverse();
    format!("/{}", parts.join("/"))
  }

  // ------------------------------------------------------------------ internals

  pub(crate) fn live(&self) -> Result<(), VfsError> {
    match self.state {
      VolumeState::Live => Ok(()),
      VolumeState::Destroying | VolumeState::Destroyed => Err(VfsError::Destroying),
    }
  }

  pub(crate) fn next_no(&mut self) -> InodeNo {
    let no = InodeNo::compose(self.prefix, self.next_counter);
    self.next_counter += 1;
    no
  }

  pub(crate) fn inode<'s>(&self, store: &'s Store, no: InodeNo) -> Result<&'s Inode, VfsError> {
    let handle = trie::get(&store.tries, self.inode_root, no).ok_or(VfsError::NotFound)?;
    store.inodes.get(handle).map_err(|_| VfsError::StaleHandle)
  }

  pub(crate) fn last_snapshot_epoch(&self) -> Option<Epoch> {
    self
      .last_snapshot
      .and_then(|id| {
        self
          .snapshots
          .get(snapshot_handle(id))
          .ok()
          .map(|s| s.epoch)
      })
      .or(self.origin_epoch)
  }

  pub(crate) fn deadlist_mut(&mut self) -> Option<&mut Deadlist> {
    let id = self.last_snapshot?;
    self
      .snapshots
      .get_mut(snapshot_handle(id))
      .ok()
      .map(|s| &mut s.deadlist)
  }

  /// Reports an object the head no longer reaches: onto the newest snapshot's deadlist when a
  /// snapshot (or the clone origin) still reaches it, else released now.
  pub(crate) fn retire(&mut self, store: &mut Store, dead: Dead) -> Result<(), VfsError> {
    let born = dead.born();
    if let Some(origin) = self.origin_epoch
      && born <= origin
      && self.last_snapshot.is_none()
    {
      return Ok(());
    }
    match self.last_snapshot_epoch() {
      Some(snap) if born <= snap => {
        if let Some(list) = self.deadlist_mut() {
          list.push(dead);
        }
        Ok(())
      }
      _ => release_dead(store, dead).map(drop),
    }
  }

  pub(crate) fn table_set(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    handle: Handle<Inode>,
  ) -> Result<Option<Handle<Inode>>, VfsError> {
    let mut scratch = Deadlist::default();
    let (root, previous) = trie::set(
      &mut store.tries,
      self.inode_root,
      no,
      handle,
      self.epoch,
      &mut scratch,
    )?;
    self.inode_root = root;
    for d in scratch.take() {
      self.retire(store, d)?;
    }
    Ok(previous)
  }

  fn table_remove(
    &mut self,
    store: &mut Store,
    no: InodeNo,
  ) -> Result<Option<Handle<Inode>>, VfsError> {
    let mut scratch = Deadlist::default();
    let (root, removed) = trie::remove(
      &mut store.tries,
      self.inode_root,
      no,
      self.epoch,
      &mut scratch,
    )?;
    self.inode_root = root;
    for d in scratch.take() {
      self.retire(store, d)?;
    }
    Ok(removed)
  }

  /// The current-epoch version of an inode, copying an older version and re-pointing the table.
  pub(crate) fn make_current_inode(
    &mut self,
    store: &mut Store,
    no: InodeNo,
  ) -> Result<Handle<Inode>, VfsError> {
    let handle = trie::get(&store.tries, self.inode_root, no).ok_or(VfsError::NotFound)?;
    let (born, kind) = {
      let inode = store.inodes.get(handle)?;
      (inode.born, inode.kind)
    };
    if born == self.epoch {
      return Ok(handle);
    }
    let _ = kind;
    let mut copy = store.inodes.get(handle)?.clone();
    copy.born = self.epoch;
    // An open extent born earlier is sealed first: the snapshot keeps the sealed bytes and the
    // head continues in a fresh extent on its next write.
    if let Body::Open { open, sealed } = copy.body {
      let mut extents = sealed;
      if let Some(e) = store.content.seal(open)? {
        insert_extent(&mut extents, e);
      }
      copy.body = Body::Sealed(extents);
      if let Ok(old) = store.inodes.get_mut(handle) {
        old.body = copy.body.clone();
      }
    }
    if let Body::Inline(v) = &copy.body {
      // The head's copy of inline content is born now; the old record keeps the snapshot's.
      let len = u64::try_from(v.len()).unwrap_or(0);
      self.bytes.sub(born, len);
      self.bytes.add(self.epoch, len);
    }
    let fresh = store.inodes.insert(copy)?;
    self.table_set(store, no, fresh)?;
    self.retire(store, Dead::Inode(handle, born))?;
    Ok(fresh)
  }

  /// Makes a directory and its ancestors current-epoch, re-pointing entries and returning the
  /// current handle for `dir`.
  pub(crate) fn make_current_dir(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
  ) -> Result<Handle<DirNode>, VfsError> {
    self.make_current_dir_node(store, dir)
  }

  pub(crate) fn make_current_dir_node(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
  ) -> Result<Handle<DirNode>, VfsError> {
    // Resolve by identity first: a handle from before a copy names a node the head has left.
    let no = store
      .dirs
      .get(dir)
      .map_err(|_| VfsError::StaleHandle)?
      .inode;
    let current = self.current_dir(store, no)?;
    let node = store.dirs.get(current)?;
    if node.born == self.epoch {
      return Ok(current);
    }
    let born = node.born;
    let parent_no = node.parent;
    let own_name = node.name.clone();
    let mut copy = node.clone();
    copy.born = self.epoch;
    let fresh = store.dirs.insert(copy)?;
    // The inode record follows the node, so the table names the fresh node from now on.
    let inode_handle = self.make_current_inode(store, no)?;
    store.inodes.get_mut(inode_handle)?.body = Body::Directory(fresh);
    match parent_no {
      None => self.root = fresh,
      Some(parent_no) => {
        let parent = self.current_dir(store, parent_no)?;
        let parent = self.make_current_dir_node(store, parent)?;
        let mut retired = Retired::new();
        store.dirs.get_mut(parent)?.set_child(
          &mut store.blocks,
          self.epoch,
          &mut retired,
          self.policy,
          &own_name,
          Child::Dir(fresh),
        )?;
        self.retire_blocks(store, retired)?;
      }
    }
    self.retire(store, Dead::Dir(current, born))?;
    Ok(fresh)
  }

  /// The head's current node of directory inode `no`, through the inode table.
  pub(crate) fn current_dir(
    &self,
    store: &Store,
    no: InodeNo,
  ) -> Result<Handle<DirNode>, VfsError> {
    let handle = trie::get(&store.tries, self.inode_root, no).ok_or(VfsError::NotFound)?;
    match store.inodes.get(handle)?.body {
      Body::Directory(dir) => Ok(dir),
      _ => Err(VfsError::NotDirectory),
    }
  }

  /// The head's current node for a directory handle a caller holds (which may predate a copy).
  pub(crate) fn head_dir(
    &self,
    store: &Store,
    dir: Handle<DirNode>,
  ) -> Result<Handle<DirNode>, VfsError> {
    let no = store
      .dirs
      .get(dir)
      .map_err(|_| VfsError::StaleHandle)?
      .inode;
    self.current_dir(store, no)
  }

  pub(crate) fn dir_insert(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
    child: Child,
  ) -> Result<(), VfsError> {
    let cutover = store.dir_cutover;
    let epoch = self.epoch;
    let policy = self.policy;
    let mut retired = Retired::new();
    let node = store.dirs.get_mut(dir)?;
    if node
      .lookup(&store.blocks, policy, name)
      .is_some_and(|e| e.child == Child::Whiteout)
    {
      node.set_child(&mut store.blocks, epoch, &mut retired, policy, name, child)?;
    } else {
      node.insert(
        &mut store.blocks,
        epoch,
        &mut retired,
        policy,
        name,
        child,
        cutover,
      )?;
    }
    self.retire_blocks(store, retired)
  }

  /// Journals a whiteout when the removal of `name` left one (a base name, §4.5).
  fn record_whiteout(
    &mut self,
    store: &Store,
    dir: Handle<DirNode>,
    name: &str,
    path: &str,
  ) -> Result<(), VfsError> {
    let dir = self.head_dir(store, dir)?;
    let left = store
      .dirs
      .get(dir)?
      .lookup(&store.blocks, self.policy, name)
      .is_some_and(|e| e.child == Child::Whiteout);
    if left {
      self.record(Op::Whiteout, path, None, 0);
    }
    Ok(())
  }

  /// Retires blocks a directory mutation replaced or dropped, by the epoch rule.
  pub(crate) fn retire_blocks(
    &mut self,
    store: &mut Store,
    retired: Retired,
  ) -> Result<(), VfsError> {
    for (block, born) in retired {
      self.retire(store, Dead::DirBlock(block, born))?;
    }
    Ok(())
  }

  pub(crate) fn dir_remove(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    name: &str,
  ) -> Result<Child, VfsError> {
    let cutover = store.dir_cutover;
    let epoch = self.epoch;
    let policy = self.policy;
    let mut retired = Retired::new();
    let node = store.dirs.get_mut(dir)?;
    let dir_no = node.inode;
    let in_base = node.base == BaseDirState::Merged && self.base_listing_has(dir_no, name);
    let node = store.dirs.get_mut(dir)?;
    let removed = if !in_base {
      node.remove(
        &mut store.blocks,
        epoch,
        &mut retired,
        policy,
        name,
        cutover,
      )?
    } else {
      // A base-backed name keeps a whiteout so the base entry stays hidden (§4.5).
      let child = node.lookup(&store.blocks, policy, name).map(|e| e.child);
      if child.is_some() {
        node.set_child(
          &mut store.blocks,
          epoch,
          &mut retired,
          policy,
          name,
          Child::Whiteout,
        )?;
      }
      child
    };
    self.retire_blocks(store, retired)?;
    removed.ok_or(VfsError::NotFound)
  }

  pub(crate) fn touch_dir(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
    now: i64,
  ) -> Result<(), VfsError> {
    let no = store.dirs.get(dir)?.inode;
    let handle = self.make_current_inode(store, no)?;
    let inode = store.inodes.get_mut(handle)?;
    inode.attrs.mtime = now;
    inode.attrs.ctime = now;
    inode.version += 1;
    Ok(())
  }

  pub(crate) fn adjust_nlink(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    delta: i32,
  ) -> Result<(), VfsError> {
    let handle = self.make_current_inode(store, no)?;
    let inode = store.inodes.get_mut(handle)?;
    inode.attrs.nlink = u32::try_from(i64::from(inode.attrs.nlink) + i64::from(delta)).unwrap_or(0);
    inode.attrs.ctime = self.clock.wall_ns();
    Ok(())
  }

  /// Drops one link; at zero the inode's content leaves the head's accounting and the version
  /// is retired.
  pub(crate) fn drop_link(&mut self, store: &mut Store, no: InodeNo) -> Result<(), VfsError> {
    let handle = self.make_current_inode(store, no)?;
    let nlink = {
      let inode = store.inodes.get_mut(handle)?;
      inode.attrs.nlink = inode.attrs.nlink.saturating_sub(1);
      inode.attrs.nlink
    };
    if nlink > 0 {
      return Ok(());
    }
    // At zero links the inode has left the namespace. If a transport still holds it open (a
    // reference), keep its content and table entry alive as an orphan and reclaim at the last
    // `unreference` (POSIX unlink-while-open); otherwise reclaim now.
    if self.references.get(&no).is_some_and(|count| *count > 0) {
      self.orphans.insert(no);
      Ok(())
    } else {
      self.reclaim_inode(store, no)
    }
  }

  /// Reclaims an inode that has no links and no references: its content leaves the head's
  /// accounting, its number is freed and its version retired.
  fn reclaim_inode(&mut self, store: &mut Store, no: InodeNo) -> Result<(), VfsError> {
    let handle = self.make_current_inode(store, no)?;
    let born = store.inodes.get(handle)?.born;
    self.release_body(store, handle)?;
    self.table_remove(store, no)?;
    self.retire(store, Dead::Inode(handle, born))?;
    // The base plane's descriptor, if one was held, is closed by the owner of the host at its
    // next `process_hints`; the tables forget the inode now.
    let _ = self.base_forget(no);
    Ok(())
  }

  /// Takes a reference on inode `no` — an open handle, or a transport lookup the kernel holds
  /// until it forgets the inode. The inode must exist (a reference to an absent number is refused,
  /// so the reference map cannot grow past the inode table's own cap), and the count is checked (a
  /// reference count that would overflow is refused, not wrapped). The inode's content survives a
  /// later `unlink` until every reference is dropped (POSIX unlink-while-open; §4.6, the
  /// inode-addressed-io design). Charging the reference against the §4.2 admission budget, and
  /// recording which attachment owns it (for disconnect and restart cleanup), are owed with the
  /// interface change.
  pub fn reference(&mut self, store: &Store, no: InodeNo) -> Result<(), VfsError> {
    self.inode(store, no)?;
    let count = self.references.entry(no).or_insert(0);
    *count = count.checked_add(1).ok_or(VfsError::TooManyLinks)?;
    Ok(())
  }

  /// Drops one reference on inode `no`. If it was the last reference and the inode has already left
  /// the namespace (an orphan), its content is reclaimed now, at this terminal step.
  pub fn unreference(&mut self, store: &mut Store, no: InodeNo) -> Result<(), VfsError> {
    self.unreference_n(store, no, 1)
  }

  /// Drops up to `n` references on inode `no` in one bounded step — a transport's bulk forget, or
  /// an attachment teardown that discards its references without a message per inode (FUSE's
  /// unmount). Never drops below zero. Reclaims the inode if this brings the count to zero and it
  /// is an orphan, at this terminal step.
  pub fn unreference_n(&mut self, store: &mut Store, no: InodeNo, n: u64) -> Result<(), VfsError> {
    let remaining = match self.references.get_mut(&no) {
      Some(count) => {
        let drop = u32::try_from(n).unwrap_or(u32::MAX).min(*count);
        *count -= drop;
        let remaining = *count;
        if remaining == 0 {
          self.references.remove(&no);
        }
        remaining
      }
      None => 0,
    };
    if remaining == 0 && self.orphans.contains(&no) {
      // Reclaim only if the inode is still unlinked. A re-link — a future `LINK` /
      // `linkat(AT_EMPTY_PATH)` on the still-open inode — revives it with a name, and it must not
      // be reclaimed then. No operation can re-link a nameless orphan today, so this guards that
      // owed operation rather than fixing a reachable bug.
      let nlink = self.inode(store, no).map(|i| i.attrs.nlink).unwrap_or(0);
      if nlink == 0 {
        // Reclaim before dropping the orphan record, so a failed reclamation retains the cleanup
        // obligation (the orphan is retried) rather than leaking the inode.
        self.reclaim_inode(store, no)?;
      }
      self.orphans.remove(&no);
    }
    Ok(())
  }

  pub(crate) fn release_dir_node(
    &mut self,
    store: &mut Store,
    dir: Handle<DirNode>,
  ) -> Result<(), VfsError> {
    let born = store.dirs.get(dir)?.born;
    let mut blocks = Retired::new();
    store.dirs.get(dir)?.blocks(&store.blocks, &mut blocks);
    self.retire_blocks(store, blocks)?;
    self.retire(store, Dead::Dir(dir, born))
  }

  /// Releases an inode's content by the epoch rule (the head no longer references it).
  pub(crate) fn release_body(
    &mut self,
    store: &mut Store,
    handle: Handle<Inode>,
  ) -> Result<(), VfsError> {
    for (epoch, len) in content_by_epoch(store, handle) {
      self.bytes.sub(epoch, len);
    }
    let body = std::mem::replace(&mut store.inodes.get_mut(handle)?.body, Body::None);
    let last = self.last_snapshot_epoch();
    let mut dead = Deadlist::default();
    match body {
      Body::Sealed(extents) => release_extents(store, &extents, last, &mut dead)?,
      Body::Open { open, sealed } => {
        store.content.release_open(open)?;
        release_extents(store, &sealed, last, &mut dead)?;
      }
      Body::Base(b) => release_extents(store, &b.pinned, last, &mut dead)?,
      _ => {}
    }
    for d in dead.take() {
      if let Some(list) = self.deadlist_mut() {
        list.push(d);
      }
    }
    Ok(())
  }

  /// The bytes a write will add to `referenced_bytes`: the materialized delta under the chunk
  /// rule (§4.5, D-6). Content is materialized per chunk window: a write into a window
  /// materializes the window from its start to the write's end, so `materialized(window) =
  /// max(existing, end within window)`; inline content is materialized byte for byte until a
  /// write ends past the inline threshold, when it spills into the first window.
  pub(crate) fn write_charge(
    &self,
    store: &Store,
    no: InodeNo,
    off: u64,
    end: u64,
  ) -> Result<u64, VfsError> {
    let inode = self.inode(store, no)?;
    let chunk = u64::try_from(store.content.chunk_bytes()).unwrap_or(u64::MAX);
    let inline = u64::try_from(store.inline_bytes).unwrap_or(0);
    let before = materialized_windows(&inode.body, chunk);
    let mut after = before.clone();
    if let Body::Inline(v) = &inode.body {
      if end <= inline {
        let len = u64::try_from(v.len()).unwrap_or(0);
        return Ok(end.saturating_sub(len));
      }
      // The inline bytes spill into the first window before the write applies.
      after.insert(0, u64::try_from(v.len()).unwrap_or(0));
    }
    let mut cursor = off;
    while cursor < end {
      let window = cursor / chunk;
      let window_end = (window + 1) * chunk;
      let write_end = end.min(window_end);
      let materialized = write_end - window * chunk;
      let entry = after.entry(window).or_insert(0);
      *entry = (*entry).max(materialized);
      cursor = write_end;
    }
    let total_before: u64 = before.values().sum();
    let total_after: u64 = after.values().sum();
    Ok(total_after.saturating_sub(total_before))
  }

  pub(crate) fn apply_write(
    &mut self,
    store: &mut Store,
    handle: Handle<Inode>,
    off: u64,
    bytes: &[u8],
  ) -> Result<(), VfsError> {
    let end = off + u64::try_from(bytes.len()).unwrap_or(0);
    let inline_limit = u64::try_from(store.inline_bytes).unwrap_or(0);
    let epoch = self.epoch;
    let chunk = u64::try_from(store.content.chunk_bytes()).unwrap_or(u64::MAX);
    let before = content_by_epoch(store, handle);
    let body = std::mem::replace(&mut store.inodes.get_mut(handle)?.body, Body::None);
    let new_body = match body {
      Body::Inline(mut v) if end <= inline_limit => {
        let at = usize::try_from(off).unwrap_or(0);
        let e = usize::try_from(end).unwrap_or(0);
        if v.len() < e {
          v.resize(e, 0);
        }
        v[at..e].copy_from_slice(bytes);
        Body::Inline(v)
      }
      Body::Inline(v) => {
        // Spill the inline bytes into an open extent, then write.
        let mut open =
          store
            .content
            .open(0, usize::try_from(end.min(chunk)).unwrap_or(0), epoch)?;
        store.content.write_open(&mut open, 0, &v)?;
        let mut sealed = Vec::new();
        self.write_into(store, &mut open, &mut sealed, off, bytes)?
      }
      Body::Sealed(mut sealed) => {
        let mut open = self.open_window(store, &mut sealed, off, bytes.len())?;
        self.write_into(store, &mut open, &mut sealed, off, bytes)?
      }
      Body::Open {
        mut open,
        mut sealed,
      } => self.write_into(store, &mut open, &mut sealed, off, bytes)?,
      Body::Base(mut b) => {
        // The touched windows were pinned by `Overlay::write`; the write goes into them, and
        // beyond the base's bytes into fresh windows, then everything seals back into `pinned`.
        let mut open = self.open_window(store, &mut b.pinned, off, bytes.len())?;
        let written = self.write_into(store, &mut open, &mut b.pinned, off, bytes)?;
        if let Body::Open { open, sealed } = written {
          b.pinned = sealed;
          if let Some(e) = store.content.seal(open)? {
            insert_extent(&mut b.pinned, e);
          }
        }
        Body::Base(b)
      }
      other => other,
    };
    store.inodes.get_mut(handle)?.body = new_body;
    self.reconcile(before, content_by_epoch(store, handle));
    Ok(())
  }

  /// Moves the histogram from one by-epoch view of an inode's content to the next.
  pub(crate) fn reconcile(&mut self, before: Vec<(Epoch, u64)>, after: Vec<(Epoch, u64)>) {
    for (epoch, len) in before {
      self.bytes.sub(epoch, len);
    }
    for (epoch, len) in after {
      self.bytes.add(epoch, len);
    }
  }

  /// Writes into an open extent, sealing and reopening as the write crosses chunk boundaries
  /// or leaves the open extent's range.
  /// Opens the chunk window holding `cursor` for writing: the window's sealed extent, if it
  /// has one, is reopened (copied, its chunk retired by the epoch rule), else a fresh extent
  /// starts at the window's start. Every extent thus begins at a window boundary and a window
  /// has at most one extent, which is what the chunk rule of §4.5 charges.
  pub(crate) fn open_window(
    &mut self,
    store: &mut Store,
    sealed: &mut Vec<Extent>,
    cursor: u64,
    want: usize,
  ) -> Result<OpenExtent, VfsError> {
    let chunk = u64::try_from(store.content.chunk_bytes()).unwrap_or(u64::MAX);
    let epoch = self.epoch;
    let window_start = cursor - (cursor % chunk);
    let Some(pos) = sealed.iter().position(|e| e.off == window_start) else {
      return store.content.open(window_start, want, epoch);
    };
    let e = sealed.remove(pos);
    let open = store.content.reopen(&e, epoch)?;
    if let ExtentSrc::Chunk { chunk: c, .. } = e.src {
      let last = self.last_snapshot_epoch();
      let mut dead = Deadlist::default();
      store.content.release_chunk(c, last, &mut dead)?;
      for d in dead.take() {
        if let Some(list) = self.deadlist_mut() {
          list.push(d);
        }
      }
    }
    Ok(open)
  }

  pub(crate) fn write_into(
    &mut self,
    store: &mut Store,
    open: &mut OpenExtent,
    sealed: &mut Vec<Extent>,
    off: u64,
    bytes: &[u8],
  ) -> Result<Body, VfsError> {
    let chunk = u64::try_from(store.content.chunk_bytes()).unwrap_or(u64::MAX);
    let mut cursor = off;
    let mut remaining = bytes;
    let mut current = *open;
    while !remaining.is_empty() {
      // The open extent covers one chunk window; a write outside it seals the extent and
      // opens the cursor's window (reopening its sealed extent if it has one).
      let same_window = cursor >= current.off && cursor - current.off < chunk;
      if !same_window {
        if let Some(e) = store.content.seal(current)? {
          insert_extent(sealed, e);
        }
        current = self.open_window(store, sealed, cursor, remaining.len())?;
      }
      let at = usize::try_from(cursor - current.off).unwrap_or(0);
      let room = usize::try_from(chunk)
        .unwrap_or(usize::MAX)
        .saturating_sub(at);
      let take = remaining.len().min(room);
      store
        .content
        .write_open(&mut current, at, &remaining[..take])?;
      cursor += u64::try_from(take).unwrap_or(0);
      remaining = &remaining[take..];
    }
    Ok(Body::Open {
      open: current,
      sealed: std::mem::take(sealed),
    })
  }

  pub(crate) fn apply_truncate(
    &mut self,
    store: &mut Store,
    handle: Handle<Inode>,
    len: u64,
  ) -> Result<(), VfsError> {
    let old_size = store.inodes.get(handle)?.attrs.size;
    if len >= old_size {
      return Ok(());
    }
    let last = self.last_snapshot_epoch();
    let before = content_by_epoch(store, handle);
    let body = std::mem::replace(&mut store.inodes.get_mut(handle)?.body, Body::None);
    let mut dead = Deadlist::default();
    let new_body = match body {
      Body::Inline(mut v) => {
        v.truncate(usize::try_from(len).unwrap_or(0));
        Body::Inline(v)
      }
      Body::Sealed(mut extents) => {
        clip_extents(store, &mut extents, len, last, &mut dead)?;
        Body::Sealed(extents)
      }
      Body::Open {
        mut open,
        mut sealed,
      } => {
        clip_extents(store, &mut sealed, len, last, &mut dead)?;
        if open.off >= len {
          store.content.release_open(open)?;
          Body::Sealed(sealed)
        } else {
          let keep = len - open.off;
          ChunkStore::truncate_open(&mut open, keep);
          Body::Open { open, sealed }
        }
      }
      Body::Base(mut b) => {
        clip_extents(store, &mut b.pinned, len, last, &mut dead)?;
        // Disk bytes past the cut are no longer the file's; a later extension is a hole.
        b.base_len = b.base_len.min(len);
        Body::Base(b)
      }
      other => other,
    };
    for d in dead.take() {
      if let Some(list) = self.deadlist_mut() {
        list.push(d);
      }
    }
    store.inodes.get_mut(handle)?.body = new_body;
    self.reconcile(before, content_by_epoch(store, handle));
    Ok(())
  }

  fn is_ancestor(
    &self,
    store: &Store,
    ancestor: Handle<DirNode>,
    of: Handle<DirNode>,
  ) -> Result<bool, VfsError> {
    let ancestor_no = store
      .dirs
      .get(ancestor)
      .map_err(|_| VfsError::StaleHandle)?
      .inode;
    let mut current = Some(of);
    let mut guard = 0usize;
    while let Some(c) = current {
      let node = store.dirs.get(c).map_err(|_| VfsError::StaleHandle)?;
      if node.inode == ancestor_no {
        return Ok(true);
      }
      current = match node.parent {
        Some(no) => Some(self.current_dir(store, no)?),
        None => None,
      };
      guard += 1;
      if guard > usize::from(u16::MAX) {
        return Err(VfsError::Invalid);
      }
    }
    Ok(false)
  }

  pub(crate) fn record(&mut self, op: Op, path: &str, inode: Option<InodeNo>, prev_version: u64) {
    let at = self.clock.monotonic_ns();
    self
      .journal
      .append(op, path, inode, self.epoch, at, prev_version);
  }
}

/// Head-reachable content bytes by birth epoch. `total` is `referenced_bytes`; `since(e)` is
/// the bytes born after `e`, which is `unique_bytes` for the newest shared epoch. A clone
/// starts with everything it inherits in one bucket at its origin epoch (`floor`): every
/// object born at or before the origin is shared, so their exact epochs do not matter and
/// releases of them land in the same bucket.
#[derive(Clone, Debug, Default)]
pub(crate) struct ByEpoch {
  /// Bytes born at each epoch, indexed by epoch number minus the floor.
  buckets: Vec<u64>,
  /// The epoch the first bucket stands for.
  floor: u64,
}

impl ByEpoch {
  fn inherited(origin: Epoch, bytes: u64) -> Self {
    Self {
      buckets: vec![bytes],
      floor: origin.0,
    }
  }

  fn slot(&mut self, epoch: Epoch) -> &mut u64 {
    let index = usize::try_from(epoch.0.saturating_sub(self.floor)).unwrap_or(0);
    if self.buckets.len() <= index {
      self.buckets.resize(index + 1, 0);
    }
    &mut self.buckets[index]
  }

  fn add(&mut self, epoch: Epoch, len: u64) {
    if len > 0 {
      *self.slot(epoch) += len;
    }
  }

  fn sub(&mut self, epoch: Epoch, len: u64) {
    if len > 0 {
      let slot = self.slot(epoch);
      *slot = slot.saturating_sub(len);
    }
  }

  pub(crate) fn total(&self) -> u64 {
    self.buckets.iter().sum()
  }

  /// Bytes born after `epoch` (all of them when there is no shared epoch).
  fn since(&self, epoch: Option<Epoch>) -> u64 {
    let Some(epoch) = epoch else {
      return self.total();
    };
    let first = usize::try_from(epoch.0.saturating_sub(self.floor) + 1).unwrap_or(usize::MAX);
    self.buckets.iter().skip(first).sum()
  }
}

/// An inode's content bytes by birth epoch: inline bytes are born with the record, an extent's
/// bytes with its chunk, an open extent's with the extent.
pub(crate) fn content_by_epoch(store: &Store, handle: Handle<Inode>) -> Vec<(Epoch, u64)> {
  let Ok(inode) = store.inodes.get(handle) else {
    return Vec::new();
  };
  let sealed_born = |e: &Extent| match e.src {
    ExtentSrc::Chunk { chunk, .. } => store.content.chunk(chunk).map(|c| c.born),
    ExtentSrc::Zero => None,
  };
  match &inode.body {
    Body::Inline(b) => vec![(inode.born, u64::try_from(b.len()).unwrap_or(0))],
    Body::Sealed(extents) => extents
      .iter()
      .filter_map(|e| sealed_born(e).map(|born| (born, chunk_len(e))))
      .collect(),
    Body::Open { open, sealed } => {
      let mut out: Vec<(Epoch, u64)> = sealed
        .iter()
        .filter_map(|e| sealed_born(e).map(|born| (born, chunk_len(e))))
        .collect();
      out.push((open.born, open.len));
      out
    }
    Body::Base(b) => b
      .pinned
      .iter()
      .filter_map(|e| sealed_born(e).map(|born| (born, chunk_len(e))))
      .collect(),
    _ => Vec::new(),
  }
}

/// The materialized bytes per chunk window of a body: window index → bytes from the window's
/// start (the chunk rule of §4.5: an extent covers its window from the window's start to the
/// extent's end, at most one chunk).
fn materialized_windows(body: &Body, chunk: u64) -> std::collections::BTreeMap<u64, u64> {
  let mut map = std::collections::BTreeMap::new();
  let chunk = chunk.max(1);
  let mut add = |off: u64, len: u64| {
    if len > 0 {
      let window = off / chunk;
      let entry = map.entry(window).or_insert(0u64);
      *entry = (*entry).max(off - window * chunk + len);
    }
  };
  match body {
    Body::Sealed(extents) => extents.iter().for_each(|e| add(e.off, e.len)),
    Body::Open { open, sealed } => {
      sealed.iter().for_each(|e| add(e.off, e.len));
      add(open.off, open.len);
    }
    Body::Base(b) => b.pinned.iter().for_each(|e| add(e.off, e.len)),
    _ => {}
  }
  map
}

fn snapshot_handle(id: SnapshotId) -> Handle<Snapshot> {
  Handle::from_raw(id.index, id.generation)
}

pub(crate) fn stamp_all(attrs: &mut Attrs, now: i64) {
  attrs.atime = now;
  attrs.mtime = now;
  attrs.ctime = now;
  attrs.btime = now;
}

pub(crate) fn chunk_len(e: &Extent) -> u64 {
  match e.src {
    ExtentSrc::Chunk { .. } => e.len,
    ExtentSrc::Zero => 0,
  }
}

/// Copies the overlap of `bytes` (at file offset `base`) into `out` (at file offset `off`).
pub(crate) fn copy_range(bytes: &[u8], base: u64, off: u64, out: &mut [u8]) {
  let src_end = base + u64::try_from(bytes.len()).unwrap_or(0);
  let dst_end = off + u64::try_from(out.len()).unwrap_or(0);
  let start = base.max(off);
  let end = src_end.min(dst_end);
  if start >= end {
    return;
  }
  let (s, e) = (
    usize::try_from(start - base).unwrap_or(0),
    usize::try_from(end - base).unwrap_or(0),
  );
  let (d0, d1) = (
    usize::try_from(start - off).unwrap_or(0),
    usize::try_from(end - off).unwrap_or(0),
  );
  out[d0..d1].copy_from_slice(&bytes[s..e]);
}

/// Releases the chunks of sealed extents by the epoch rule.
fn release_extents(
  store: &mut Store,
  extents: &[Extent],
  last: Option<Epoch>,
  dead: &mut Deadlist,
) -> Result<(), VfsError> {
  for e in extents {
    if let ExtentSrc::Chunk { chunk, .. } = e.src {
      store.content.release_chunk(chunk, last, dead)?;
    }
  }
  Ok(())
}

/// Inserts a sealed extent, keeping the list ascending by offset.
pub(crate) fn insert_extent(list: &mut Vec<Extent>, e: Extent) {
  let at = list.partition_point(|x| x.off < e.off);
  list.insert(at, e);
}

/// Clips extents to `len`, releasing the chunks fully beyond it; returns bytes freed.
pub(crate) fn clip_extents(
  store: &mut Store,
  extents: &mut Vec<Extent>,
  len: u64,
  last: Option<Epoch>,
  dead: &mut Deadlist,
) -> Result<u64, VfsError> {
  let mut freed = 0u64;
  let mut keep = Vec::with_capacity(extents.len());
  for e in extents.drain(..) {
    if e.off >= len {
      if let ExtentSrc::Chunk { chunk, .. } = e.src {
        freed += store.content.release_chunk(chunk, last, dead)?;
      }
    } else if e.off + e.len > len {
      let clipped = Extent {
        off: e.off,
        len: len - e.off,
        src: e.src,
      };
      freed += e.len - clipped.len;
      keep.push(clipped);
    } else {
      keep.push(e);
    }
  }
  *extents = keep;
  Ok(freed)
}

/// Every directory node reachable from `root` and born after `since`; a node born at or before
/// `since` is shared with the clone's origin and its subtree with it (a child's copy forces the
/// parent's), so the walk visits only the volume's own nodes (§4.5, the clone destroy example).
fn collect_dirs(
  store: &Store,
  root: Handle<DirNode>,
  since: Option<Epoch>,
  out: &mut Vec<Handle<DirNode>>,
  blocks: &mut Vec<(Handle<DirBlock>, Epoch)>,
) {
  let mut stack = vec![root];
  while let Some(d) = stack.pop() {
    let Ok(node) = store.dirs.get(d) else {
      continue;
    };
    if since.is_some_and(|s| node.born.0 <= s.0) {
      continue;
    }
    out.push(d);
    node.blocks_since(&store.blocks, since, blocks);
    for e in node.iter(&store.blocks) {
      if let Child::Dir(c) = e.child {
        stack.push(c);
      }
    }
  }
}

/// Derived: block slab segments hold the blocks of one arena-sized region of directory data,
/// sixty-four pages of blocks per segment, so a segment is one large allocation and per-block
/// frees never reach the allocator.
fn block_segment_slots(page: usize) -> Derived<usize> {
  derived!(
    (page * BLOCK_SEGMENT_PAGES / size_of::<DirBlock>()).max(1),
    "page × 64 / size_of::<DirBlock>()",
    ["machine page", "size_of::<DirBlock>()"]
  )
}

/// Shape: pages of block slots per slab segment.
const BLOCK_SEGMENT_PAGES: usize = 64;

/// Releases a dead object and returns the work units it cost, so a destroy slice's budget
/// counts what is freed rather than how many handles it touched: an inode one unit per extent
/// plus one, a directory node, a block, a trie node or a chunk one unit (their slots are
/// vacated in place, never copied out).
fn release_dead(store: &mut Store, dead: Dead) -> Result<usize, VfsError> {
  match dead {
    Dead::Dir(h, _) => {
      let _ = store.dirs.discard(h);
      Ok(1)
    }
    Dead::DirBlock(h, _) => {
      let _ = store.blocks.discard(h);
      Ok(1)
    }
    Dead::Inode(h, _) => {
      let Ok(inode) = store.inodes.remove(h) else {
        return Ok(1);
      };
      let extents = match inode.body {
        Body::Sealed(extents) => free_extents(store, &extents),
        Body::Open { open, sealed } => {
          let _ = store.content.release_open(open);
          free_extents(store, &sealed) + 1
        }
        Body::Base(b) => free_extents(store, &b.pinned),
        _ => 0,
      };
      Ok(1 + extents)
    }
    Dead::Trie(h, _) => {
      let _ = store.tries.discard(h);
      Ok(1)
    }
    Dead::Chunk(h, _) => {
      let _ = store.content.free_chunk(h);
      Ok(1)
    }
  }
}

/// Frees the chunks of sealed extents; returns how many there were.
fn free_extents(store: &mut Store, extents: &[Extent]) -> usize {
  for e in extents {
    if let ExtentSrc::Chunk { chunk, .. } = e.src {
      let _ = store.content.free_chunk(chunk);
    }
  }
  extents.len()
}
