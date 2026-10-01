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
use crate::inode::{Attrs, Body, Inode, Kind, XattrTable};
use crate::journal::Op;
use crate::volume::{Store, Volume, stamp_all};

/// Format: the longest attribute name the volume core accepts, in bytes: Linux's `XATTR_NAME_MAX`
/// (255, `include/uapi/linux/limits.h`), the largest limit of any host slates serves (macOS's
/// `XATTR_MAXNAMELEN` is 127, an NFSv4 component is bounded by the server's advertised name limit).
/// A bridge narrows it to its host's own limit. It is the archive format's own limit, so a volume never
/// holds a name its placed archive would refuse (AUD-29-56).
pub const XATTR_NAME_MAX_BYTES: usize = slates_archive::manifest::XATTR_NAME_MAX_BYTES;

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

/// What an attribute change does to the owner's working copy (`XattrTable::sidecar`): a change
/// through any path but the working copy itself drops it, so a transport's next read encodes the
/// attributes afresh; the transport that is reconciling its own working copy keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sidecar {
  /// Drop the working copy (every attribute verb but a transport's own reconcile).
  Drop,
  /// Keep it: the change *is* the working copy's content.
  Keep,
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
    Ok(self.inode(store, attribute)?.attrs.size)
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
    self.read_body(store, attribute, off, buf)
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
    self.xattr_set_with(store, no, name, value, mode, Sidecar::Drop)
  }

  /// [`Volume::xattr_set`] with the working copy's fate chosen ([`Sidecar`]).
  pub fn xattr_set_with(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    name: &[u8],
    value: &[u8],
    mode: XattrSet,
    sidecar: Sidecar,
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
    let (replaced, dropped) = match self.name_attribute(store, no, name, fresh, sidecar) {
      Ok(outcome) => outcome,
      Err(refusal) => return Err(self.abandon_attribute(store, fresh, refusal)),
    };
    for old in replaced.into_iter().chain(dropped) {
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
    self.xattr_remove_with(store, no, name, Sidecar::Drop)
  }

  /// [`Volume::xattr_remove`] with the working copy's fate chosen ([`Sidecar`]).
  pub fn xattr_remove_with(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    name: &[u8],
    sidecar: Sidecar,
  ) -> Result<(), VfsError> {
    self.live()?;
    self.xattr_inode(store, no, name)?;
    let folded = self.dropped_copy_counter(store, no)?;
    let handle = self.make_current_inode(store, no)?;
    let now = self.clock.wall_ns();
    let inode = store.inodes.get_mut(handle)?;
    let prev = inode.version;
    let table = inode.xattrs.as_deref_mut().ok_or(VfsError::NoAttribute)?;
    let removed = table.remove(name).ok_or(VfsError::NoAttribute)?;
    let dropped = match sidecar {
      Sidecar::Drop => table.sidecar.take(),
      Sidecar::Keep => None,
    };
    if inode.xattrs.as_deref().is_some_and(XattrTable::is_vacant) {
      inode.xattrs = None;
    }
    if dropped.is_some() {
      inode.fold_counter(folded);
    }
    inode.stamp_change(now);
    self.record(Op::RemoveXattr { name: name.into() }, "", Some(no), prev);
    for old in std::iter::once(removed).chain(dropped) {
      self.drop_link(store, old)?;
    }
    Ok(())
  }

  /// Writes `bytes` into attribute `name`'s existing value on inode `no` at `off`, in place, keeping
  /// the working copy: a transport's reconcile when its working copy's write landed wholly inside one
  /// attribute's value (a resource fork written in pieces), so each piece costs its own length rather
  /// than a whole-value rewrite. The value may grow; one `SetXattr` is journaled on the owner and its
  /// change time advances. `NoAttribute` when unset.
  pub fn xattr_write_at(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    name: &[u8],
    off: u64,
    bytes: &[u8],
  ) -> Result<(), VfsError> {
    self.live()?;
    let attribute = self.xattr_inode(store, no, name)?;
    self.write_unrecorded(store, attribute, off, bytes)?;
    let handle = self.make_current_inode(store, no)?;
    let now = self.clock.wall_ns();
    let inode = store.inodes.get_mut(handle)?;
    let prev = inode.version;
    inode.stamp_change(now);
    self.record(Op::SetXattr { name: name.into() }, "", Some(no), prev);
    self.attribute_writes_in_place = self.attribute_writes_in_place.saturating_add(1);
    Ok(())
  }

  /// The attributes (size and times) of inode `no`'s working copy, if it has one.
  pub fn sidecar_attrs(&self, store: &Store, no: InodeNo) -> Result<Option<Attrs>, VfsError> {
    match self.sidecar_of(store, no)? {
      Some(copy) => Ok(Some(self.inode(store, copy)?.attrs)),
      None => Ok(None),
    }
  }

  /// Reads inode `no`'s working copy from `off` (`NoAttribute` when it has none).
  pub fn sidecar_read(
    &self,
    store: &Store,
    no: InodeNo,
    off: u64,
    buf: &mut [u8],
  ) -> Result<usize, VfsError> {
    let copy = self.sidecar_of(store, no)?.ok_or(VfsError::NoAttribute)?;
    self.read_body(store, copy, off, buf)
  }

  /// Writes `bytes` into inode `no`'s working copy at `off`, creating an empty one first when it has
  /// none. Charged to the quota like a file's bytes; not journaled (the reconcile that follows is).
  pub fn sidecar_write(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    off: u64,
    bytes: &[u8],
  ) -> Result<usize, VfsError> {
    self.live()?;
    let copy = self.sidecar_or_create(store, no)?;
    self.write_unrecorded(store, copy, off, bytes)
  }

  /// Sets inode `no`'s working copy to `len` bytes (a truncate, or an extension with zeros),
  /// creating an empty one first when it has none.
  pub fn sidecar_truncate(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    len: u64,
  ) -> Result<(), VfsError> {
    self.live()?;
    let copy = self.sidecar_or_create(store, no)?;
    self.truncate_body(store, copy, len)
  }

  /// Drops inode `no`'s working copy; nothing when it has none.
  pub fn sidecar_drop(&mut self, store: &mut Store, no: InodeNo) -> Result<(), VfsError> {
    self.live()?;
    if self.sidecar_of(store, no)?.is_none() {
      return Ok(());
    }
    let folded = self.dropped_copy_counter(store, no)?;
    let handle = self.make_current_inode(store, no)?;
    let inode = store.inodes.get_mut(handle)?;
    let dropped = inode
      .xattrs
      .as_deref_mut()
      .and_then(|table| table.sidecar.take());
    if inode.xattrs.as_deref().is_some_and(XattrTable::is_vacant) {
      inode.xattrs = None;
    }
    if dropped.is_some() {
      inode.fold_counter(folded);
    }
    match dropped {
      Some(copy) => self.drop_link(store, copy),
      None => Ok(()),
    }
  }

  /// The working copy's attribute inode, if inode `no` has one.
  fn sidecar_of(&self, store: &Store, no: InodeNo) -> Result<Option<InodeNo>, VfsError> {
    Ok(
      owner_of(self.namespace_inode(store, no)?)?
        .xattrs
        .as_deref()
        .and_then(|table| table.sidecar),
    )
  }

  /// The working copy's attribute inode, created empty when inode `no` has none.
  fn sidecar_or_create(&mut self, store: &mut Store, no: InodeNo) -> Result<InodeNo, VfsError> {
    if let Some(copy) = self.sidecar_of(store, no)? {
      return Ok(copy);
    }
    let (uid, gid) = {
      let owner = self.inode(store, no)?;
      (owner.attrs.uid, owner.attrs.gid)
    };
    let fresh = self.new_attribute_inode(store, no, uid, gid)?;
    let handle = match self.make_current_inode(store, no) {
      Ok(handle) => handle,
      Err(refusal) => return Err(self.abandon_attribute(store, fresh, refusal)),
    };
    let inode = store.inodes.get_mut(handle)?;
    inode.xattrs.get_or_insert_with(Box::default).sidecar = Some(fresh);
    // The view now shows the empty copy in place of the encoding: its counter moves.
    inode.fold_counter(1);
    Ok(fresh)
  }

  /// What inode `no`'s owner absorbs when its working copy is dropped: the copy's counter and one
  /// more, so the view's counter (owner plus copy) moves past every value it showed; 0 when it has
  /// no copy.
  fn dropped_copy_counter(&self, store: &Store, no: InodeNo) -> Result<u64, VfsError> {
    match self.sidecar_of(store, no)? {
      Some(copy) => Ok(self.inode(store, copy)?.version + 1),
      None => Ok(0),
    }
  }

  /// The change counter of inode `no`'s AppleDouble view: its own plus its working copy's, which
  /// moves on every change to what the view shows and never repeats (A-38; `Inode::fold_counter`).
  pub fn view_change(&self, store: &Store, no: InodeNo) -> Result<u64, VfsError> {
    let own = owner_of(self.namespace_inode(store, no)?)?.version;
    match self.sidecar_of(store, no)? {
      Some(copy) => Ok(own + self.inode(store, copy)?.version),
      None => Ok(own),
    }
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
    Ok(self.inode_in(store, id, attribute)?.attrs.size)
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
    self.read_in_body(store, id, attribute, off, buf)
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
  /// Returns the replaced attribute inode and, under [`Sidecar::Drop`], the dropped working copy.
  fn name_attribute(
    &mut self,
    store: &mut Store,
    owner: InodeNo,
    name: &[u8],
    fresh: InodeNo,
    sidecar: Sidecar,
  ) -> Result<(Option<InodeNo>, Option<InodeNo>), VfsError> {
    let folded = self.dropped_copy_counter(store, owner)?;
    let handle = self.make_current_inode(store, owner)?;
    let now = self.clock.wall_ns();
    let inode = store.inodes.get_mut(handle)?;
    let prev = inode.version;
    let table = inode.xattrs.get_or_insert_with(Box::default);
    let replaced = table.insert(name, fresh);
    let dropped = match sidecar {
      Sidecar::Drop => table.sidecar.take(),
      Sidecar::Keep => None,
    };
    if dropped.is_some() {
      inode.fold_counter(folded);
    }
    inode.stamp_change(now);
    self.record(Op::SetXattr { name: name.into() }, "", Some(owner), prev);
    Ok((replaced, dropped))
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
