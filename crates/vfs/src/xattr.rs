//! Extended attributes (§4.5 "Extended attributes"): the volume verbs that list, read, set and remove
//! an inode's named attributes.
//!
//! Each value is the body of an *attribute inode* ([`crate::inode::Inode::attribute_of`]): a
//! file-bodied inode in no directory, named from its owner's [`XattrTable`]. The value is therefore
//! content like any file's: charged to the volume's quota, chunked and sealed by the content store,
//! frozen by a snapshot at chunk granularity, retained under the §4.2 retention rules, readable at an
//! offset (the form an NFSv4 named attribute takes, RFC 8881 §5.3) and recovered by the image walk
//! that captures every inode. Copying an owner for a new epoch copies only its table. Each attribute
//! counts as one inode against the volume's inode allowance, which bounds how many attributes a
//! volume can hold (§4.2); the value bytes count against the quota, like file bytes.
//!
//! A set is atomic: the value is written into a fresh attribute inode first, and only then does the
//! owner's table name it; a refusal on the way reclaims the fresh inode and leaves the old value in
//! place. The replaced inode (or a removed one) loses its only link and is reclaimed, or kept as an
//! orphan while a transport holds it open (POSIX unlink-while-open, as for files). An owner that is
//! reclaimed drops every attribute it names. The journal records one `SetXattr`/`RemoveXattr` on
//! the owner per verb; the attribute inode's own content writes are not journaled, because the
//! attribute, not a file, is what changed.
//!
//! Rejected: holding values inline in the owner (a `Vec` per name). Every copy-on-write of the owner
//! would copy every value, a macOS resource fork (a value megabytes long) could not be read or
//! written at an offset without a whole-value copy, and values would bypass the content store's
//! sealing and deduplication.

use crate::error::VfsError;
use crate::ids::{InodeNo, SnapshotId};
use crate::inode::{Body, Inode, Kind, XattrTable};
use crate::journal::Op;
use crate::volume::{Store, Volume, stamp_all};

/// Format: the longest attribute name the volume core accepts, in bytes: Linux's `XATTR_NAME_MAX`
/// (255, `include/uapi/linux/limits.h`), the largest limit of any host slates serves (macOS's
/// `XATTR_MAXNAMELEN` is 127, an NFSv4 component is bounded by the server's advertised name limit).
/// A bridge narrows it to its host's own limit.
pub const XATTR_NAME_MAX_BYTES: usize = 255;

/// How a set treats an existing attribute: the `XATTR_CREATE`/`XATTR_REPLACE` flags of
/// `setxattr(2)` (and the NFSv4 `OPEN` of a named attribute with or without `EXCLUSIVE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XattrSet {
  /// Create the attribute or replace its value.
  Either,
  /// Create only: refuse `AlreadyExists` when the name is set.
  Create,
  /// Replace only: refuse `NoAttribute` when the name is not set.
  Replace,
}

/// Checks an attribute name: non-empty, at most [`XATTR_NAME_MAX_BYTES`] bytes, no NUL (every host's
/// names are C strings or UTF-8 components). An empty name is `Invalid` (`EINVAL`, as Linux refuses
/// it); a long name or one with a NUL is `InvalidName` (`ENAMETOOLONG`/`EINVAL` at the bridge).
pub fn check_name(name: &[u8]) -> Result<(), VfsError> {
  if name.is_empty() {
    return Err(VfsError::Invalid);
  }
  if name.len() > XATTR_NAME_MAX_BYTES || name.contains(&0) {
    return Err(VfsError::InvalidName);
  }
  Ok(())
}

impl Volume {
  /// The names of inode `no`'s extended attributes, ascending by bytes.
  pub fn xattr_names(&self, store: &Store, no: InodeNo) -> Result<Vec<Box<[u8]>>, VfsError> {
    Ok(names_of(owner_of(self.inode(store, no)?)?))
  }

  /// The length in bytes of attribute `name`'s value on inode `no` (`NoAttribute` when unset).
  pub fn xattr_len(&self, store: &Store, no: InodeNo, name: &[u8]) -> Result<u64, VfsError> {
    let attribute = self.xattr_inode(store, no, name)?;
    Ok(self.stat(store, attribute)?.size)
  }

  /// Reads attribute `name`'s value on inode `no` from byte `off` into `buf`; returns the bytes read
  /// (fewer than `buf` at the value's end, zero past it).
  pub fn xattr_read(
    &self,
    store: &Store,
    no: InodeNo,
    name: &[u8],
    off: u64,
    buf: &mut [u8],
  ) -> Result<usize, VfsError> {
    let attribute = self.xattr_inode(store, no, name)?;
    self.read(store, attribute, off, buf)
  }

  /// The attribute inode that holds `name`'s value on inode `no`: the handle a transport reads a
  /// named attribute through (`NoAttribute` when unset).
  pub fn xattr_inode(&self, store: &Store, no: InodeNo, name: &[u8]) -> Result<InodeNo, VfsError> {
    check_name(name)?;
    let owner = owner_of(self.inode(store, no)?)?;
    owner
      .xattrs
      .as_deref()
      .and_then(|table| table.get(name))
      .ok_or(VfsError::NoAttribute)
  }

  /// Whether inode `no` is an attribute inode (the value of another inode's extended attribute). A
  /// transport refuses to serve one as a namespace object: it is reached only through its owner.
  pub fn is_attribute(&self, store: &Store, no: InodeNo) -> Result<bool, VfsError> {
    Ok(self.inode(store, no)?.attribute_of.is_some())
  }

  /// Sets attribute `name` on inode `no` to `value`, as `mode` allows. Atomic (the module doc): the
  /// value is written into a fresh attribute inode, which the owner's table then names; a refusal on
  /// the way (the inode allowance, the quota, the retention allowance) leaves the old value in place.
  /// The owner's change time advances and one `SetXattr` is journaled on it.
  pub fn xattr_set(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    name: &[u8],
    value: &[u8],
    mode: XattrSet,
  ) -> Result<(), VfsError> {
    self.live()?;
    check_name(name)?;
    let (existing, uid, gid) = {
      let owner = owner_of(self.inode(store, no)?)?;
      (
        owner.xattrs.as_deref().and_then(|table| table.get(name)),
        owner.attrs.uid,
        owner.attrs.gid,
      )
    };
    match (mode, existing) {
      (XattrSet::Create, Some(_)) => return Err(VfsError::AlreadyExists),
      (XattrSet::Replace, None) => return Err(VfsError::NoAttribute),
      _ => {}
    }
    let fresh = self.new_attribute_inode(store, no, uid, gid)?;
    if let Err(refusal) = self.write_unrecorded(store, fresh, 0, value) {
      return Err(self.abandon_attribute(store, fresh, refusal));
    }
    let replaced = match self.name_attribute(store, no, name, fresh) {
      Ok(replaced) => replaced,
      Err(refusal) => return Err(self.abandon_attribute(store, fresh, refusal)),
    };
    if let Some(old) = replaced {
      self.drop_link(store, old)?;
    }
    Ok(())
  }

  /// Removes attribute `name` from inode `no` (`NoAttribute` when unset). The owner's change time
  /// advances and one `RemoveXattr` is journaled on it; the attribute inode loses its only link.
  pub fn xattr_remove(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    name: &[u8],
  ) -> Result<(), VfsError> {
    self.live()?;
    self.xattr_inode(store, no, name)?;
    let handle = self.make_current_inode(store, no)?;
    let now = self.clock.wall_ns();
    let inode = store.inodes.get_mut(handle)?;
    let prev = inode.version;
    let removed = inode
      .xattrs
      .as_deref_mut()
      .and_then(|table| table.remove(name))
      .ok_or(VfsError::NoAttribute)?;
    if inode.xattrs.as_deref().is_some_and(XattrTable::is_empty) {
      inode.xattrs = None;
    }
    inode.attrs.ctime = now;
    inode.version += 1;
    self.record(Op::RemoveXattr { name: name.into() }, "", Some(no), prev);
    self.drop_link(store, removed)
  }

  /// The names of inode `no`'s extended attributes as snapshot `id` holds them.
  pub fn xattr_names_in(
    &self,
    store: &Store,
    id: SnapshotId,
    no: InodeNo,
  ) -> Result<Vec<Box<[u8]>>, VfsError> {
    Ok(names_of(owner_of(self.inode_in(store, id, no)?)?))
  }

  /// The length of attribute `name`'s value on inode `no` as snapshot `id` holds it.
  pub fn xattr_len_in(
    &self,
    store: &Store,
    id: SnapshotId,
    no: InodeNo,
    name: &[u8],
  ) -> Result<u64, VfsError> {
    let attribute = self.xattr_inode_in(store, id, no, name)?;
    Ok(self.stat_in(store, id, attribute)?.size)
  }

  /// The attribute inode that holds `name`'s value on inode `no` as snapshot `id` holds it.
  pub fn xattr_inode_in(
    &self,
    store: &Store,
    id: SnapshotId,
    no: InodeNo,
    name: &[u8],
  ) -> Result<InodeNo, VfsError> {
    check_name(name)?;
    owner_of(self.inode_in(store, id, no)?)?
      .xattrs
      .as_deref()
      .and_then(|table| table.get(name))
      .ok_or(VfsError::NoAttribute)
  }

  /// Reads attribute `name`'s value on inode `no` as snapshot `id` holds it.
  pub fn xattr_read_in(
    &self,
    store: &Store,
    id: SnapshotId,
    no: InodeNo,
    name: &[u8],
    off: u64,
    buf: &mut [u8],
  ) -> Result<usize, VfsError> {
    let attribute = self.xattr_inode_in(store, id, no, name)?;
    self.read_in(store, id, attribute, off, buf)
  }

  /// A fresh, empty attribute inode for `owner`, owned as the owner is (its uid and gid) and
  /// readable and writable by that owner only; charged against the inode allowance.
  fn new_attribute_inode(
    &mut self,
    store: &mut Store,
    owner: InodeNo,
    uid: u32,
    gid: u32,
  ) -> Result<InodeNo, VfsError> {
    /// Format: the attribute inode's permission bits — owner read and write. Access to an
    /// attribute is decided on its owner (the bridge checks the owner's mode); these bits only
    /// keep the attribute inode from ever reading as world-accessible.
    const ATTRIBUTE_MODE: u32 = 0o600;
    let fresh = self.next_no()?;
    let now = self.clock.wall_ns();
    let mut inode = Inode::new(
      fresh,
      self.epoch,
      Kind::File,
      ATTRIBUTE_MODE,
      Body::Inline(Vec::new()),
    );
    inode.attrs.uid = uid;
    inode.attrs.gid = gid;
    inode.attribute_of = Some(owner);
    stamp_all(&mut inode.attrs, now);
    let handle = match store.inodes.insert(inode) {
      Ok(handle) => handle,
      Err(refusal) => {
        self.unissue_no();
        return Err(refusal.into());
      }
    };
    if let Err(refusal) = self.table_set(store, fresh, handle) {
      let _ = store.inodes.remove(handle);
      self.unissue_no();
      return Err(refusal);
    }
    Ok(fresh)
  }

  /// Names the attribute inode `fresh` as `name` in `owner`'s table, returning the inode it replaced.
  /// The owner's change time advances and one `SetXattr` is journaled on it.
  fn name_attribute(
    &mut self,
    store: &mut Store,
    owner: InodeNo,
    name: &[u8],
    fresh: InodeNo,
  ) -> Result<Option<InodeNo>, VfsError> {
    let handle = self.make_current_inode(store, owner)?;
    let now = self.clock.wall_ns();
    let inode = store.inodes.get_mut(handle)?;
    let prev = inode.version;
    let replaced = inode
      .xattrs
      .get_or_insert_with(Box::default)
      .insert(name, fresh);
    inode.attrs.ctime = now;
    inode.version += 1;
    self.record(Op::SetXattr { name: name.into() }, "", Some(owner), prev);
    Ok(replaced)
  }

  /// Reclaims an attribute inode a set could not install, returning the set's refusal (or the
  /// reclaim's own, which then names the worse state: the fresh inode could not be released).
  fn abandon_attribute(
    &mut self,
    store: &mut Store,
    fresh: InodeNo,
    refusal: VfsError,
  ) -> VfsError {
    match self.reclaim_inode(store, fresh) {
      Ok(()) => refusal,
      Err(reclaim) => reclaim,
    }
  }
}

/// The inode as an attribute owner: a namespace inode. An attribute inode has no attributes of its
/// own (NFSv4 refuses a named attribute of a named attribute, RFC 8881 §5.3); `NotPermitted`.
fn owner_of(inode: &Inode) -> Result<&Inode, VfsError> {
  if inode.attribute_of.is_some() {
    return Err(VfsError::NotPermitted);
  }
  Ok(inode)
}

/// The attribute names an owner's table holds, ascending.
fn names_of(owner: &Inode) -> Vec<Box<[u8]>> {
  owner
    .xattrs
    .as_deref()
    .map(|table| table.iter().map(|(name, _)| name.into()).collect())
    .unwrap_or_default()
}
