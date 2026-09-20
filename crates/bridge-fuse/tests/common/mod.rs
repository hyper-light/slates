//! Fixtures shared by the FUSE bridge's integration tests: a scratch store and volume (the shape of
//! `tests/volume_bridge.rs`), and a seam wrapper that refuses to gather invalidations on demand — the
//! injected notification-collection failure AUD-02's regression requires. Each integration test is
//! its own crate and compiles this module afresh, so a helper one test does not use is dead code
//! there — allowed here.
#![allow(dead_code)]
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{
  Bridge, CacheLifetime, DirEntry, FsStat, Invalidation, InvalidationCursor, NodeAttr, ObjectId,
  OpContext, RenameFlags, SetAttr,
};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::error::VfsError;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the page and a small arena for a test volume.
pub(crate) const PAGE: usize = 4096;
pub(crate) const REGION_PAGES: usize = 4096;

/// A scratch store: one small RAM region, as the bridge tests build it.
pub(crate) fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

/// A scratch volume in `store`, journalled so the seam can report what changed (§4.6).
pub(crate) fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

/// A mounted fixture follows the daemon's provisioning ownership rule (§4.6, R10).
pub(crate) fn volume_for_owner(store: &mut Store, uid: u32, gid: u32) -> Volume {
  let mut volume = volume(store);
  let root = volume.root_inode(store).unwrap();
  volume.chown(store, root, uid, gid).unwrap();
  volume
}

/// A seam that serves every operation through `inner` and refuses the next `refuse_gathers`
/// invalidation gathers (AUD-02's injected collection failure): what a transport sees when the
/// seam cannot answer what changed — the round must be retried, never skipped.
pub(crate) struct FailingGather<B> {
  pub(crate) inner: B,
  pub(crate) refuse_gathers: u32,
}

impl<B: Bridge> Bridge for FailingGather<B> {
  fn root(&mut self, cx: &OpContext) -> Result<u64, VfsError> {
    self.inner.root(cx)
  }

  fn lookup(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<NodeAttr, VfsError> {
    self.inner.lookup(parent, cx, name)
  }

  fn getattr(&mut self, object: ObjectId, cx: &OpContext) -> Result<NodeAttr, VfsError> {
    self.inner.getattr(object, cx)
  }

  fn open(&mut self, object: ObjectId, cx: &OpContext, flags: u32) -> Result<u64, VfsError> {
    self.inner.open(object, cx, flags)
  }

  fn read(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    size: u32,
    out: &mut Vec<u8>,
  ) -> Result<(), VfsError> {
    self.inner.read(object, cx, offset, size, out)
  }

  fn write(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    data: &[u8],
  ) -> Result<u32, VfsError> {
    self.inner.write(object, cx, offset, data)
  }

  fn opendir(&mut self, object: ObjectId, cx: &OpContext) -> Result<u64, VfsError> {
    self.inner.opendir(object, cx)
  }

  fn readdir(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    fh: u64,
    offset: u64,
  ) -> Result<Vec<DirEntry>, VfsError> {
    self.inner.readdir(object, cx, fh, offset)
  }

  fn create(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
    flags: u32,
  ) -> Result<(NodeAttr, u64), VfsError> {
    self.inner.create(parent, cx, name, mode, flags)
  }

  fn release(&mut self, object: ObjectId, cx: &OpContext, fh: u64) -> Result<(), VfsError> {
    self.inner.release(object, cx, fh)
  }

  fn reference(&mut self, object: ObjectId, cx: &OpContext) -> Result<(), VfsError> {
    self.inner.reference(object, cx)
  }

  fn forget(&mut self, object: ObjectId, cx: &OpContext, nlookup: u64) {
    self.inner.forget(object, cx, nlookup);
  }

  fn flush(&mut self, object: ObjectId, cx: &OpContext, fh: u64) -> Result<(), VfsError> {
    self.inner.flush(object, cx, fh)
  }

  fn mknod(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
    kind: slates_vfs::inode::Kind,
  ) -> Result<NodeAttr, VfsError> {
    self.inner.mknod(parent, cx, name, mode, kind)
  }

  fn mkdir(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
  ) -> Result<NodeAttr, VfsError> {
    self.inner.mkdir(parent, cx, name, mode)
  }

  fn unlink(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<(), VfsError> {
    self.inner.unlink(parent, cx, name)
  }

  fn rmdir(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<(), VfsError> {
    self.inner.rmdir(parent, cx, name)
  }

  fn symlink(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    target: &str,
  ) -> Result<NodeAttr, VfsError> {
    self.inner.symlink(parent, cx, name, target)
  }

  fn link(
    &mut self,
    target: ObjectId,
    new_parent: ObjectId,
    cx: &OpContext,
    new_name: &str,
  ) -> Result<NodeAttr, VfsError> {
    self.inner.link(target, new_parent, cx, new_name)
  }

  fn readlink(&mut self, object: ObjectId, cx: &OpContext) -> Result<String, VfsError> {
    self.inner.readlink(object, cx)
  }

  fn rename(
    &mut self,
    old_parent: ObjectId,
    new_parent: ObjectId,
    cx: &OpContext,
    old_name: &str,
    new_name: &str,
    flags: RenameFlags,
  ) -> Result<(), VfsError> {
    self
      .inner
      .rename(old_parent, new_parent, cx, old_name, new_name, flags)
  }

  fn setattr(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    changes: SetAttr,
  ) -> Result<NodeAttr, VfsError> {
    self.inner.setattr(object, cx, changes)
  }

  fn statfs(&mut self, object: ObjectId, cx: &OpContext) -> Result<FsStat, VfsError> {
    self.inner.statfs(object, cx)
  }

  fn now(&mut self) -> i64 {
    self.inner.now()
  }

  fn change_token(&mut self, object: ObjectId, cx: &OpContext) -> Result<u64, VfsError> {
    self.inner.change_token(object, cx)
  }

  fn sweep_attachment(&mut self, cx: &OpContext) -> Result<(), VfsError> {
    self.inner.sweep_attachment(cx)
  }

  fn cache_lifetime(&mut self, object: ObjectId, cx: &OpContext) -> CacheLifetime {
    self.inner.cache_lifetime(object, cx)
  }

  fn invalidations(
    &mut self,
    cx: &OpContext,
    cursor: InvalidationCursor,
    out: &mut Vec<Invalidation>,
  ) -> Result<InvalidationCursor, VfsError> {
    if self.refuse_gathers > 0 {
      self.refuse_gathers -= 1;
      return Err(VfsError::Invalid);
    }
    self.inner.invalidations(cx, cursor, out)
  }

  fn seen(&mut self, cx: &OpContext) -> InvalidationCursor {
    self.inner.seen(cx)
  }
}
