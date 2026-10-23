//! A bridge scoped to one directory of its volume (§4.6 scoped exports; AUD-29-76: "grant a tenant only one
//! directory"). Every transport serves through the [`Bridge`] seam, so the scope is enforced here, once, for all
//! of them — never by a path check before a bind, which a component swapped for a symbolic link after the
//! check would defeat (the runtime would bind whatever the path then named).
//!
//! The rules:
//! - The scope directory is the root: [`Bridge::root`] answers it, and a listing of it names it as its own `..`
//!   (POSIX's rule for a root), so no walk climbs out.
//! - Every object a request names is checked before anything is done with it ([`Bridge::within`]): the scope
//!   itself, a directory whose parents reach it, or a node homed beneath it. A handle naming anything else —
//!   forged, kept from before a rename moved the object out, or reached by an alias whose home is outside — is
//!   answered `NotFound`: in this view it does not exist.
//! - A lookup's result and a listing's entries are held to the same rule, so a name the view shows can always
//!   be opened, and nothing outside is ever shown.
//!
//! The check climbs the object's parents, at most the volume's live inodes; a scoped request pays it per object
//! it names. A lookup's result and a listing's entries are checked against the directory the request already
//! admitted first (the relation is transitive), so a page of a listing climbs its depth once, not once per entry.
//! A rename or link within the scope moves nothing out of it: both of its directories are checked.

use crate::{
  Bridge, CacheLifetime, DirEntry, FsStat, Invalidation, InvalidationCursor, NodeAttr, ObjectId,
  OpContext, RenameFlags, SetAttr,
};
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

/// `inner`, scoped to directory `scope`.
pub struct ScopedBridge<'b> {
  inner: &'b mut dyn Bridge,
  scope: u64,
}

impl<'b> ScopedBridge<'b> {
  /// `inner` presenting only the subtree of directory inode `scope`.
  pub fn new(inner: &'b mut dyn Bridge, scope: u64) -> ScopedBridge<'b> {
    ScopedBridge { inner, scope }
  }

  /// `Ok` when `object` lies in the scope, `NotFound` when it does not.
  fn admit(&mut self, object: ObjectId, cx: &OpContext) -> Result<(), VfsError> {
    if self.inner.within(object, self.scope, cx)? {
      Ok(())
    } else {
      Err(VfsError::NotFound)
    }
  }

  /// Whether `object`, named by directory `parent` that this request already admitted, lies in the scope. The
  /// subtree relation is transitive, so an object beneath `parent` is in the scope, and that answer climbs from
  /// the object to `parent` only — one step for an entry homed there. Only an object homed elsewhere (a hard
  /// link's alias) climbs to the scope. The answer equals [`Self::admit`]'s; only its cost differs, and a page
  /// of a listing costs its depth once rather than once per entry (measured: `docs/wip/BENCHMARKS.md`, the
  /// scoped listing, 2026-10-03).
  fn within_admitted(
    &mut self,
    object: ObjectId,
    parent: ObjectId,
    cx: &OpContext,
  ) -> Result<bool, VfsError> {
    Ok(self.inner.within(object, parent.inode, cx)? || self.inner.within(object, self.scope, cx)?)
  }

  /// `node`, found in directory `parent` that this request already admitted, when it lies in the scope;
  /// `NotFound` when it does not.
  fn shown(
    &mut self,
    node: NodeAttr,
    parent: ObjectId,
    cx: &OpContext,
  ) -> Result<NodeAttr, VfsError> {
    if self.within_admitted(ObjectId::new(node.ino, node.generation), parent, cx)? {
      Ok(node)
    } else {
      Err(VfsError::NotFound)
    }
  }
}

impl Bridge for ScopedBridge<'_> {
  fn numbers_unreserved(&self) -> bool {
    self.inner.numbers_unreserved()
  }

  fn root(&mut self, _cx: &OpContext) -> Result<u64, VfsError> {
    Ok(self.scope)
  }

  fn within(&mut self, object: ObjectId, scope: u64, cx: &OpContext) -> Result<bool, VfsError> {
    Ok(self.inner.within(object, self.scope, cx)? && self.inner.within(object, scope, cx)?)
  }

  fn lookup(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<NodeAttr, VfsError> {
    self.admit(parent, cx)?;
    // The scope's root is this view's root: its `..` is itself, as a volume root's is, never the directory above.
    if name == ".." && parent.inode == self.scope {
      return self.inner.getattr(parent, cx);
    }
    let node = self.inner.lookup(parent, cx, name)?;
    self.shown(node, parent, cx)
  }

  fn getattr(&mut self, object: ObjectId, cx: &OpContext) -> Result<NodeAttr, VfsError> {
    self.admit(object, cx)?;
    self.inner.getattr(object, cx)
  }

  fn open(&mut self, object: ObjectId, cx: &OpContext, flags: u32) -> Result<u64, VfsError> {
    self.admit(object, cx)?;
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
    self.admit(object, cx)?;
    self.inner.read(object, cx, offset, size, out)
  }

  fn xattr_get(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    name: &[u8],
  ) -> Result<Vec<u8>, VfsError> {
    self.admit(object, cx)?;
    self.inner.xattr_get(object, cx, name)
  }

  fn xattr_set(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    name: &[u8],
    value: &[u8],
    how: slates_vfs::xattr::XattrSet,
  ) -> Result<(), VfsError> {
    self.admit(object, cx)?;
    self.inner.xattr_set(object, cx, name, value, how)
  }

  fn xattr_list(&mut self, object: ObjectId, cx: &OpContext) -> Result<Vec<Box<[u8]>>, VfsError> {
    self.admit(object, cx)?;
    self.inner.xattr_list(object, cx)
  }

  fn xattr_remove(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    name: &[u8],
  ) -> Result<(), VfsError> {
    self.admit(object, cx)?;
    self.inner.xattr_remove(object, cx, name)
  }

  fn seek(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    data: bool,
  ) -> Result<Option<u64>, VfsError> {
    self.admit(object, cx)?;
    self.inner.seek(object, cx, offset, data)
  }

  fn write(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    data: &[u8],
  ) -> Result<u32, VfsError> {
    self.admit(object, cx)?;
    self.inner.write(object, cx, offset, data)
  }

  fn admit_allocation(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    len: u64,
  ) -> Result<(), VfsError> {
    self.admit(object, cx)?;
    self.inner.admit_allocation(object, cx, offset, len)
  }

  fn allocate(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    offset: u64,
    len: u64,
  ) -> Result<(), VfsError> {
    self.admit(object, cx)?;
    self.inner.allocate(object, cx, offset, len)
  }

  fn opendir(&mut self, object: ObjectId, cx: &OpContext) -> Result<u64, VfsError> {
    self.admit(object, cx)?;
    self.inner.opendir(object, cx)
  }

  fn readdir(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    fh: u64,
    cookie: u64,
    limit: usize,
  ) -> Result<Vec<DirEntry>, VfsError> {
    self.admit(object, cx)?;
    // A page of the underlying directory may be entirely entries the scope drops (hard-link aliases of files
    // homed outside it). Filtering one inner page in isolation would then return an empty page *before* the
    // directory's end, which a transport reads as end-of-directory (NFS `eof = sent == rows.len() && rows.len() <
    // limit`): the in-scope entries past the dropped page would be lost. So keep fetching inner pages, advancing
    // the cursor, until a full page of in-scope entries has been gathered or the inner directory is exhausted —
    // a short or empty result then means a true end, never a page the scope happened to empty. §4.6.
    let mut shown = Vec::new();
    let mut cursor = cookie;
    while shown.len() < limit {
      let page = self.inner.readdir(object, cx, fh, cursor, limit)?;
      let Some(next) = page.last().map(|entry| entry.cookie) else {
        break; // the inner directory is exhausted.
      };
      for mut entry in page {
        if entry.name == ".." {
          // The scope is this view's root: its parent is itself, so no walk climbs out.
          if object.inode == self.scope {
            entry.ino = self.scope;
          }
          shown.push(entry);
          continue;
        }
        if entry.name == "." || self.within_admitted(ObjectId::new(entry.ino, 0), object, cx)? {
          shown.push(entry);
        }
      }
      if next == cursor {
        break; // no progress (a single cookie group fills the page); the inner bound governs it.
      }
      cursor = next;
    }
    Ok(shown)
  }

  fn create(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
    flags: u32,
  ) -> Result<(NodeAttr, u64), VfsError> {
    self.admit(parent, cx)?;
    self.inner.create(parent, cx, name, mode, flags)
  }

  fn mknod(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
    kind: Kind,
  ) -> Result<NodeAttr, VfsError> {
    self.admit(parent, cx)?;
    self.inner.mknod(parent, cx, name, mode, kind)
  }

  fn release(&mut self, object: ObjectId, cx: &OpContext, fh: u64) -> Result<(), VfsError> {
    // A handle this view opened is always released, wherever its object has since moved: a release gives
    // back, it reaches nothing.
    self.inner.release(object, cx, fh)
  }

  fn reference(&mut self, object: ObjectId, cx: &OpContext) -> Result<(), VfsError> {
    self.admit(object, cx)?;
    self.inner.reference(object, cx)
  }

  fn forget(&mut self, object: ObjectId, cx: &OpContext, nlookup: u64) {
    // A forget gives back references this view took, wherever the object has since moved.
    self.inner.forget(object, cx, nlookup);
  }

  fn flush(&mut self, object: ObjectId, cx: &OpContext, fh: u64) -> Result<(), VfsError> {
    self.inner.flush(object, cx, fh)
  }

  fn fsync(&mut self, object: ObjectId, cx: &OpContext, fh: u64) -> Result<(), VfsError> {
    self.inner.fsync(object, cx, fh)
  }

  fn mkdir(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    mode: u32,
  ) -> Result<NodeAttr, VfsError> {
    self.admit(parent, cx)?;
    self.inner.mkdir(parent, cx, name, mode)
  }

  fn unlink(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<(), VfsError> {
    self.admit(parent, cx)?;
    self.inner.unlink(parent, cx, name)
  }

  fn rmdir(&mut self, parent: ObjectId, cx: &OpContext, name: &str) -> Result<(), VfsError> {
    self.admit(parent, cx)?;
    self.inner.rmdir(parent, cx, name)
  }

  fn symlink(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    name: &str,
    target: &str,
  ) -> Result<NodeAttr, VfsError> {
    self.admit(parent, cx)?;
    self.inner.symlink(parent, cx, name, target)
  }

  fn link(
    &mut self,
    target: ObjectId,
    new_parent: ObjectId,
    cx: &OpContext,
    new_name: &str,
  ) -> Result<NodeAttr, VfsError> {
    self.admit(target, cx)?;
    self.admit(new_parent, cx)?;
    self.inner.link(target, new_parent, cx, new_name)
  }

  fn readlink(&mut self, object: ObjectId, cx: &OpContext) -> Result<String, VfsError> {
    self.admit(object, cx)?;
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
    self.admit(old_parent, cx)?;
    self.admit(new_parent, cx)?;
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
    self.admit(object, cx)?;
    self.inner.setattr(object, cx, changes)
  }

  fn statfs(&mut self, object: ObjectId, cx: &OpContext) -> Result<FsStat, VfsError> {
    self.admit(object, cx)?;
    self.inner.statfs(object, cx)
  }

  fn now(&mut self) -> i64 {
    self.inner.now()
  }

  fn change_token(&mut self, object: ObjectId, cx: &OpContext) -> Result<u64, VfsError> {
    self.admit(object, cx)?;
    self.inner.change_token(object, cx)
  }

  fn sweep_attachment(&mut self, cx: &OpContext) -> Result<(), VfsError> {
    self.inner.sweep_attachment(cx)
  }

  fn appledouble_lookup(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    owner_name: &str,
  ) -> Result<NodeAttr, VfsError> {
    self.admit(parent, cx)?;
    self.inner.appledouble_lookup(parent, cx, owner_name)
  }

  fn appledouble_create(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    owner_name: &str,
    exclusive: bool,
  ) -> Result<NodeAttr, VfsError> {
    self.admit(parent, cx)?;
    self
      .inner
      .appledouble_create(parent, cx, owner_name, exclusive)
  }

  fn appledouble_rename(
    &mut self,
    from: ObjectId,
    to: ObjectId,
    cx: &OpContext,
    from_owner: &str,
    to_owner: &str,
  ) -> Result<bool, VfsError> {
    self.admit(from, cx)?;
    self.admit(to, cx)?;
    self
      .inner
      .appledouble_rename(from, to, cx, from_owner, to_owner)
  }

  fn appledouble_remove(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    owner_name: &str,
  ) -> Result<(), VfsError> {
    self.admit(parent, cx)?;
    self.inner.appledouble_remove(parent, cx, owner_name)
  }

  fn cache_lifetime(&mut self, object: ObjectId, cx: &OpContext) -> CacheLifetime {
    self.inner.cache_lifetime(object, cx)
  }

  fn data_changes(
    &mut self,
    cx: &OpContext,
    since: u64,
    out: &mut Vec<crate::DataChange>,
  ) -> Result<bool, VfsError> {
    // The inner bridge reports the whole volume's changes; the scope invariant holds here as for invalidations — keep
    // only the files the scope admits, by the same `within` climb every other verb uses.
    let start = out.len();
    let complete = self.inner.data_changes(cx, since, out)?;
    for change in out.split_off(start) {
      if self
        .inner
        .within(ObjectId::new(change.ino, 0), self.scope, cx)?
      {
        out.push(change);
      }
    }
    Ok(complete)
  }

  fn journal_capacity(&mut self, cx: &OpContext) -> usize {
    self.inner.journal_capacity(cx)
  }

  fn invalidations(
    &mut self,
    cx: &OpContext,
    cursor: InvalidationCursor,
    out: &mut Vec<Invalidation>,
  ) -> Result<InvalidationCursor, VfsError> {
    // The inner bridge draws its invalidations from the whole volume's journal, so the stream names objects
    // across every scope. The scope invariant holds here too — an `Inode` invalidation names an inode and an
    // `Entry` names a directory and a file name, and a scoped view must reveal neither an out-of-scope inode nor
    // a name in an out-of-scope directory (§4.6; "nothing outside is ever shown"). Keep only those the scope
    // admits, by the same `within` climb every other verb uses; the parent's membership is the test for an
    // entry, exactly as a listing checks its entries.
    let start = out.len();
    let next = self.inner.invalidations(cx, cursor, out)?;
    for invalidation in out.split_off(start) {
      let object = match &invalidation {
        Invalidation::Inode { ino, .. } => *ino,
        Invalidation::Entry { parent, .. } => *parent,
        // It names no object: it stands for whatever this transport's kernel holds, which is all within the scope.
        Invalidation::Everything => {
          out.push(invalidation);
          continue;
        }
      };
      if self
        .inner
        .within(ObjectId::new(object, 0), self.scope, cx)?
      {
        out.push(invalidation);
      }
    }
    Ok(next)
  }

  fn seen(&mut self, cx: &OpContext) -> InvalidationCursor {
    self.inner.seen(cx)
  }

  fn dax_map(
    &mut self,
    object: ObjectId,
    cx: &OpContext,
    writable: bool,
    file_offset: usize,
  ) -> Result<slates_vfs::dax::Mapping, VfsError> {
    // A scoped guest may map only an inode in its subtree: admit before the gate mints the window mapping, so a
    // DAX window never opens onto content outside the scope (the "one missing predicate per method" discipline).
    self.admit(object, cx)?;
    self.inner.dax_map(object, cx, writable, file_offset)
  }

  fn dax_unmap(
    &mut self,
    mapping: &slates_vfs::dax::Mapping,
    cx: &OpContext,
  ) -> Result<bool, VfsError> {
    // The mapping's inode must lie in the scope — a scoped guest cannot revoke a mapping outside its subtree
    // (unreachable through the per-guest window table, but checked here for the same confinement discipline).
    self.admit(ObjectId::new(mapping.inode.0, 0), cx)?;
    self.inner.dax_unmap(mapping, cx)
  }

  #[cfg(unix)]
  fn dax_backing(
    &self,
    mapping: &slates_vfs::dax::Mapping,
  ) -> Option<(std::os::fd::BorrowedFd<'_>, usize, usize)> {
    // The mapping was admitted when it was minted; backing a mapping the guest already holds needs no re-admit, and
    // the inner bridge confines it to this volume.
    self.inner.dax_backing(mapping)
  }

  fn dax_revoke_pending(
    &mut self,
    mapping: &slates_vfs::dax::Mapping,
    cx: &OpContext,
  ) -> Result<bool, VfsError> {
    self.admit(ObjectId::new(mapping.inode.0, 0), cx)?;
    self.inner.dax_revoke_pending(mapping, cx)
  }

  fn dax_release_after_unmap(
    &mut self,
    mapping: &slates_vfs::dax::Mapping,
    cx: &OpContext,
  ) -> Result<bool, VfsError> {
    self.admit(ObjectId::new(mapping.inode.0, 0), cx)?;
    self.inner.dax_release_after_unmap(mapping, cx)
  }

  fn take_pending_dax_revokes(&mut self) -> Vec<(u64, bool)> {
    // The pending-revoke queue is the shard registry's, not this scope's; a scope narrows which objects a caller may
    // name, not which the daemon must unmap to stay coherent. Forward unchanged — the device half still munmaps only
    // the windows it actually holds.
    self.inner.take_pending_dax_revokes()
  }

  fn dax_window_current(&mut self, mapping: &slates_vfs::dax::Mapping, cx: &OpContext) -> bool {
    // A window outside this scope is not this scoped guest's to validate (unreachable through its per-guest window
    // table); treat it as not current so the device tears it.
    if self.admit(ObjectId::new(mapping.inode.0, 0), cx).is_err() {
      return false;
    }
    self.inner.dax_window_current(mapping, cx)
  }
}
