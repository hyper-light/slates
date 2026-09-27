//! The AppleDouble view (§4.6 "Extended attributes over NFSv3"): the `._name` file a macOS client
//! keeps a file's extended attributes in when the file system has none, served as a *view* of
//! `name`'s attributes in the volume's attribute store (§4.5, A-32) rather than as a file of its own.
//!
//! A view is named by its owner's derived inode number (`InodeNo::derived`), which no inode holds,
//! so a handle to it needs no server table and survives a restart like any handle. It exists while
//! the owner has an attribute the AppleDouble format can carry, or a working copy.
//!
//! Its bytes are the working copy when there is one (the bytes exactly as the client last wrote
//! them, `XattrTable::sidecar`), otherwise a canonical encoding rendered on demand from the values
//! (`appledouble::encode`). Every write lands in the working copy (materialized from the encoding
//! first), and the attributes are then reconciled with what the working copy now says:
//! - an incomplete file (a write sequence in progress) changes nothing;
//! - a write that falls wholly inside attribute values with the layout unchanged (a resource fork
//!   arriving in pieces) is written into those values in place, so each piece costs its own length;
//! - otherwise every attribute the file names is set to the bytes it holds, and every
//!   representable attribute it no longer names is removed.
//!
//! Attributes the format cannot carry (a name longer than 254 bytes, a value past 4 GiB) are neither
//! shown nor removed. A change through any other path (FUSE, the SDK, a merge) drops the working copy,
//! so the next read encodes afresh. The view's size and times move with each change, which is what an
//! NFSv3 client revalidates its cache by.
//!
//! Evidence: the format and its rules are xnu's (`crate::appledouble`), and five sidecars that macOS
//! 26.4.1 wrote decode, four of them re-encoding byte for byte. Samba's `vfs_fruit` and Netatalk's
//! `appledouble = ea` store AppleDouble contents as extended attributes the same way.

use slates_vfs::error::VfsError;
use slates_vfs::ids::InodeNo;
use slates_vfs::inode::Kind;
use slates_vfs::xattr::{Sidecar, XattrSet};

use super::VolumeBridge;
use crate::appledouble::{self, Decoded, Encoding, Entry, Layout, MAX_HEADER_BYTES, Span};
use crate::{NodeAttr, ObjectId, OpContext, SetAttr};

/// Format: the permission bits a view carries: its owner's read and write bits, as xnu creates a
/// `._` file (`open_xattrfile`: the target's `S_IRUSR | S_IWUSR | S_IRGRP | S_IWGRP | S_IROTH |
/// S_IWOTH`), so the client's own permission check on the view is the owner's.
const VIEW_MODE_MASK: u32 = 0o666;

/// Format: the permission bits of a mode (`S_ISUID | S_ISGID | S_ISVTX | 0o777`), which a `setattr`
/// of a view's mode is compared on.
const PERMISSION_BITS: u32 = 0o7777;

/// An owner's representable attributes: each name and its value's length, in name order.
type Held = Vec<(Box<[u8]>, u64)>;

/// The attribute names an encoding's entry indices refer to.
type Names = Vec<Box<[u8]>>;

impl VolumeBridge<'_> {
  /// The view of `owner_name`'s attributes in directory `parent`: `"."` names the directory itself
  /// (xnu keeps the mount root's attributes in `._.`). `NotFound` when the owner does not exist or
  /// has no view.
  pub(crate) fn view_lookup(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    owner_name: &str,
  ) -> Result<NodeAttr, VfsError> {
    self.authorize_read(cx)?;
    let owner = self.view_owner(parent, owner_name)?;
    if !self.has_view(owner)? {
      return Err(VfsError::NotFound);
    }
    self.view_attr(owner)
  }

  /// Creates `owner_name`'s view in `parent` with an empty working copy, as a client's create of
  /// the `._` file does. An existing view is returned as it is, or refused `AlreadyExists` when the
  /// create is exclusive.
  pub(crate) fn view_create(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    owner_name: &str,
    exclusive: bool,
  ) -> Result<NodeAttr, VfsError> {
    self.authorize_write(cx)?;
    let owner = self.view_owner(parent, owner_name)?;
    if self.has_view(owner)? {
      if exclusive {
        return Err(VfsError::AlreadyExists);
      }
      return self.view_attr(owner);
    }
    self.volume.sidecar_truncate(self.store, owner, 0)?;
    self.view_attr(owner)
  }

  /// Removes `owner_name`'s view in `parent`, as a client's removal of the `._` file does: the
  /// working copy and every attribute the format carries go. `NotFound` when there is no view.
  pub(crate) fn view_remove(
    &mut self,
    parent: ObjectId,
    cx: &OpContext,
    owner_name: &str,
  ) -> Result<(), VfsError> {
    self.authorize_write(cx)?;
    let owner = self.view_owner(parent, owner_name)?;
    if !self.has_view(owner)? {
      return Err(VfsError::NotFound);
    }
    self.volume.sidecar_drop(self.store, owner)?;
    for (name, _) in self.representable(owner)? {
      self.volume.xattr_remove(self.store, owner, &name)?;
    }
    Ok(())
  }

  /// A rename of the sidecar `._from_owner` in `from` onto `._to_owner` in `to` (§4.6): the rename
  /// xnu issues to carry a `._` file along with a file it has just renamed. Two cases are the view's:
  /// - **the sidecar follows its owner**: neither name is a real entry and both resolve to the same
  ///   owner (the old name through its departure). The attributes already moved with the inode, so
  ///   the rename is complete as it is; the departure is forgotten.
  /// - **an orphan is adopted**: the source is a real file, created through the old name when no
  ///   departure could resolve it (two renames overlapping in one directory), the target is no real
  ///   entry and its owner exists. The orphan holds only what was set in that gap, so its attributes
  ///   are **merged** into the owner (never replacing the rest) and it is removed.
  ///
  /// `Ok(false)` for anything else (an ordinary rename), or for an orphan that is not a complete
  /// AppleDouble file.
  pub(crate) fn view_rename(
    &mut self,
    from: ObjectId,
    to: ObjectId,
    cx: &OpContext,
    from_owner: &str,
    to_owner: &str,
  ) -> Result<bool, VfsError> {
    self.authorize_write(cx)?;
    let from_name = format!("._{from_owner}");
    let to_name = format!("._{to_owner}");
    let real = |bridge: &mut Self, dir: ObjectId, name: &str| {
      bridge
        .volume
        .lookup_no(bridge.store, InodeNo(dir.inode), name)
        .map(|located| located.inode)
    };
    if real(self, to, &to_name).is_ok() {
      return Ok(false);
    }
    let Ok(owner) = self.view_owner(to, to_owner) else {
      return Ok(false);
    };
    match real(self, from, &from_name) {
      Ok(orphan) => self.adopt_orphan(from, owner, orphan, &from_name),
      Err(VfsError::NotFound) => {
        if self.view_owner(from, from_owner).ok() != Some(owner) {
          return Ok(false);
        }
        self.volume.forget_departure(InodeNo(from.inode));
        Ok(true)
      }
      Err(refusal) => Err(refusal),
    }
  }

  /// Merges the orphan sidecar `orphan` (named `name` in `from`) into `owner`'s attributes and
  /// removes it; `Ok(false)` when it is not a complete AppleDouble file.
  fn adopt_orphan(
    &mut self,
    from: ObjectId,
    owner: InodeNo,
    orphan: InodeNo,
    name: &str,
  ) -> Result<bool, VfsError> {
    let size = self.volume.stat(self.store, orphan)?.size;
    let len = usize::try_from(size).map_err(|_| VfsError::FileTooLarge)?;
    let mut bytes = vec![0u8; len];
    let read = self.volume.read(self.store, orphan, 0, &mut bytes)?;
    bytes.truncate(read);
    let prefix = &bytes[..bytes.len().min(MAX_HEADER_BYTES)];
    let Decoded::Complete(layout) = appledouble::decode(prefix, size, &mut |at, buf: &mut [u8]| {
      let start = usize::try_from(at).unwrap_or(usize::MAX).min(bytes.len());
      let end = start.saturating_add(buf.len()).min(bytes.len());
      buf[..end - start].copy_from_slice(&bytes[start..end]);
      end - start
    }) else {
      return Ok(false);
    };
    for (attribute, span) in &layout.attributes {
      let (Ok(start), Ok(end)) = (usize::try_from(span.offset), usize::try_from(span.end())) else {
        return Err(VfsError::FileTooLarge);
      };
      let value = bytes.get(start..end).ok_or(VfsError::Invalid)?;
      if !self.value_equals(owner, attribute, value)? {
        self
          .volume
          .xattr_set(self.store, owner, attribute, value, XattrSet::Either)?;
      }
    }
    self
      .volume
      .unlink_no(self.store, InodeNo(from.inode), name)?;
    self.volume.forget_departure(InodeNo(from.inode));
    Ok(true)
  }

  /// The attributes of the view whose derived number is `object`.
  pub(crate) fn view_getattr(&mut self, owner: InodeNo) -> Result<NodeAttr, VfsError> {
    if !self.has_view(owner)? {
      return Err(VfsError::NotFound);
    }
    self.view_attr(owner)
  }

  /// Reads `size` bytes of the view at `offset` into `out`.
  pub(crate) fn view_read(
    &mut self,
    owner: InodeNo,
    offset: u64,
    size: usize,
    out: &mut Vec<u8>,
  ) -> Result<(), VfsError> {
    let mut buf = vec![0u8; size];
    let read = match self.volume.sidecar_attrs(self.store, owner)? {
      Some(_) => self
        .volume
        .sidecar_read(self.store, owner, offset, &mut buf)?,
      None => {
        let (names, encoding) = self.encoding(owner)?;
        self.render(owner, &names, &encoding, offset, &mut buf)?
      }
    };
    out.extend_from_slice(&buf[..read]);
    Ok(())
  }

  /// Writes `data` into the view at `offset` and reconciles the attributes (the module doc).
  pub(crate) fn view_write(
    &mut self,
    owner: InodeNo,
    offset: u64,
    data: &[u8],
  ) -> Result<usize, VfsError> {
    self.materialize(owner)?;
    let before = self.decode_copy(owner)?;
    let written = self.volume.sidecar_write(self.store, owner, offset, data)?;
    let span = Span {
      offset,
      len: u64::try_from(written).map_err(|_| VfsError::FileTooLarge)?,
    };
    self.reconcile(owner, before, Some(span))?;
    Ok(written)
  }

  /// Applies a `setattr` to the view: a size truncates the working copy and reconciles; a mode or
  /// ownership equal to the view's own is accepted; any other change is refused `NotPermitted`,
  /// since a view's mode and owner are its owner's (§4.6: never acknowledge an ignored field).
  pub(crate) fn view_setattr(
    &mut self,
    owner: InodeNo,
    changes: SetAttr,
  ) -> Result<NodeAttr, VfsError> {
    let current = self.view_getattr(owner)?;
    let same_mode = changes
      .mode
      .is_none_or(|mode| mode & PERMISSION_BITS == current.mode);
    let same_owner = changes.uid.is_none_or(|uid| uid == current.uid)
      && changes.gid.is_none_or(|gid| gid == current.gid);
    let times = changes.atime.is_some() || changes.mtime.is_some() || changes.ctime.is_some();
    if !same_mode || !same_owner || times {
      return Err(VfsError::NotPermitted);
    }
    if let Some(size) = changes.size {
      self.materialize(owner)?;
      self.volume.sidecar_truncate(self.store, owner, size)?;
      self.reconcile(owner, Decoded::Incomplete, None)?;
    }
    self.view_attr(owner)
  }

  /// The owner a view name refers to: the entry in `parent`, or `parent` itself for `"."`.
  fn view_owner(&mut self, parent: ObjectId, owner_name: &str) -> Result<InodeNo, VfsError> {
    let dir = InodeNo(parent.inode);
    match owner_name {
      "" | ".." => Err(VfsError::NotFound),
      "." => Ok(dir),
      name => {
        let located = match self.host.as_mut() {
          Some(host) => self.volume.with_host(host).lookup_no(self.store, dir, name),
          None => self.volume.lookup_no(self.store, dir, name),
        };
        match located {
          Ok(located) => Ok(located.inode),
          // A name a rename just vacated still names the moved file for its sidecar, until the
          // client's rename finishes (`Volume::departed`).
          Err(VfsError::NotFound) => self
            .volume
            .departed(self.store, dir, name)
            .ok_or(VfsError::NotFound),
          Err(refusal) => Err(refusal),
        }
      }
    }
  }

  /// Whether `owner` has a view: a working copy, or an attribute the format carries.
  fn has_view(&self, owner: InodeNo) -> Result<bool, VfsError> {
    Ok(
      self.volume.sidecar_attrs(self.store, owner)?.is_some()
        || !self.representable(owner)?.is_empty(),
    )
  }

  /// The view's attributes: a regular file with its owner's read and write bits and ownership, one
  /// link, the working copy's size and times when it has one, otherwise the encoding's size and the
  /// owner's change time.
  fn view_attr(&self, owner: InodeNo) -> Result<NodeAttr, VfsError> {
    let owner_attrs = self.volume.stat(self.store, owner)?;
    let (size, mtime, ctime) = match self.volume.sidecar_attrs(self.store, owner)? {
      Some(copy) => (copy.size, copy.mtime, copy.ctime),
      None => (
        self.encoding(owner)?.1.len,
        owner_attrs.ctime,
        owner_attrs.ctime,
      ),
    };
    Ok(NodeAttr {
      ino: owner.derived().0,
      generation: 0,
      kind: Kind::File,
      mode: owner_attrs.mode & VIEW_MODE_MASK,
      nlink: 1,
      uid: owner_attrs.uid,
      gid: owner_attrs.gid,
      size,
      atime: mtime,
      mtime,
      ctime,
      change: self.volume.view_change(self.store, owner)?,
    })
  }

  /// The owner's attributes the format can carry, with their lengths, in name order.
  fn representable(&self, owner: InodeNo) -> Result<Held, VfsError> {
    let mut held = Vec::new();
    for name in self.volume.xattr_names(self.store, owner)? {
      let len = self.volume.xattr_len(self.store, owner, &name)?;
      let entry = Entry {
        name: name.to_vec(),
        len,
      };
      if appledouble::representable(&entry) {
        held.push((name, len));
      }
    }
    Ok(held)
  }

  /// The canonical encoding of the owner's attributes and the names its entry indices refer to.
  fn encoding(&self, owner: InodeNo) -> Result<(Names, Encoding), VfsError> {
    let held = self.representable(owner)?;
    let entries: Vec<Entry> = held
      .iter()
      .map(|(name, len)| Entry {
        name: name.to_vec(),
        len: *len,
      })
      .collect();
    let encoding = appledouble::encode(&entries);
    Ok((held.into_iter().map(|(name, _)| name).collect(), encoding))
  }

  /// Renders `[offset, offset + buf.len())` of an encoding, reading values from the store.
  fn render(
    &self,
    owner: InodeNo,
    names: &[Box<[u8]>],
    encoding: &Encoding,
    offset: u64,
    buf: &mut [u8],
  ) -> Result<usize, VfsError> {
    let mut failure = None;
    let rendered = encoding.render(offset, buf, &mut |index, at, dest| {
      let outcome = names
        .get(index)
        .ok_or(VfsError::Invalid)
        .and_then(|name| self.volume.xattr_read(self.store, owner, name, at, dest));
      match outcome {
        Ok(read) if read == dest.len() => {}
        Ok(_) => failure = Some(VfsError::Invalid),
        Err(refusal) => failure = Some(refusal),
      }
    });
    match failure {
      Some(refusal) => Err(refusal),
      None => Ok(rendered),
    }
  }

  /// Gives the owner a working copy holding the canonical encoding, when it has none, so a write
  /// lands on the bytes the client last read.
  fn materialize(&mut self, owner: InodeNo) -> Result<(), VfsError> {
    if self.volume.sidecar_attrs(self.store, owner)?.is_some() {
      return Ok(());
    }
    let (names, encoding) = self.encoding(owner)?;
    let len = usize::try_from(encoding.len).map_err(|_| VfsError::FileTooLarge)?;
    let mut bytes = vec![0u8; len];
    self.render(owner, &names, &encoding, 0, &mut bytes)?;
    self.volume.sidecar_write(self.store, owner, 0, &bytes)?;
    Ok(())
  }

  /// What the working copy now says.
  fn decode_copy(&self, owner: InodeNo) -> Result<Decoded, VfsError> {
    let Some(attrs) = self.volume.sidecar_attrs(self.store, owner)? else {
      return Ok(Decoded::Incomplete);
    };
    let prefix_len = usize::try_from(attrs.size)
      .unwrap_or(usize::MAX)
      .min(MAX_HEADER_BYTES);
    let mut prefix = vec![0u8; prefix_len];
    let read = self
      .volume
      .sidecar_read(self.store, owner, 0, &mut prefix)?;
    prefix.truncate(read);
    let volume = &*self.volume;
    let store = &*self.store;
    Ok(appledouble::decode(
      &prefix,
      attrs.size,
      &mut |at, buf: &mut [u8]| volume.sidecar_read(store, owner, at, buf).unwrap_or(0),
    ))
  }

  /// Brings the attributes in line with the working copy (the module doc). `before` is the layout
  /// before a write of `written`; a truncate passes neither.
  fn reconcile(
    &mut self,
    owner: InodeNo,
    before: Decoded,
    written: Option<Span>,
  ) -> Result<(), VfsError> {
    let Decoded::Complete(after) = self.decode_copy(owner)? else {
      return Ok(());
    };
    if let (Decoded::Complete(before), Some(written)) = (&before, written)
      && *before == after
      && written.offset >= after.header.end()
    {
      return self.write_through(owner, &after, written);
    }
    self.replace_all(owner, &after)
  }

  /// Writes the bytes of `written` that fall inside attribute values into those values in place.
  fn write_through(
    &mut self,
    owner: InodeNo,
    layout: &Layout,
    written: Span,
  ) -> Result<(), VfsError> {
    for (name, span) in &layout.attributes {
      let start = span.offset.max(written.offset);
      let stop = span.end().min(written.end());
      if start >= stop {
        continue;
      }
      let len = usize::try_from(stop - start).map_err(|_| VfsError::FileTooLarge)?;
      let mut bytes = vec![0u8; len];
      self
        .volume
        .sidecar_read(self.store, owner, start, &mut bytes)?;
      self
        .volume
        .xattr_write_at(self.store, owner, name, start - span.offset, &bytes)?;
    }
    Ok(())
  }

  /// Sets every attribute the working copy names to the bytes it holds (when they differ) and
  /// removes every representable attribute it no longer names, keeping the working copy.
  fn replace_all(&mut self, owner: InodeNo, layout: &Layout) -> Result<(), VfsError> {
    for (name, span) in &layout.attributes {
      let len = usize::try_from(span.len).map_err(|_| VfsError::FileTooLarge)?;
      let mut value = vec![0u8; len];
      self
        .volume
        .sidecar_read(self.store, owner, span.offset, &mut value)?;
      if self.value_equals(owner, name, &value)? {
        continue;
      }
      self.volume.xattr_set_with(
        self.store,
        owner,
        name,
        &value,
        XattrSet::Either,
        Sidecar::Keep,
      )?;
    }
    for (name, _) in self.representable(owner)? {
      if !layout
        .attributes
        .iter()
        .any(|(named, _)| named.as_slice() == &*name)
      {
        self
          .volume
          .xattr_remove_with(self.store, owner, &name, Sidecar::Keep)?;
      }
    }
    Ok(())
  }

  /// Whether attribute `name` already holds exactly `value`.
  fn value_equals(&self, owner: InodeNo, name: &[u8], value: &[u8]) -> Result<bool, VfsError> {
    match self.volume.xattr_len(self.store, owner, name) {
      Ok(len) if len == value.len() as u64 => {
        let mut current = vec![0u8; value.len()];
        self
          .volume
          .xattr_read(self.store, owner, name, 0, &mut current)?;
        Ok(current == value)
      }
      Ok(_) | Err(VfsError::NoAttribute) => Ok(false),
      Err(refusal) => Err(refusal),
    }
  }
}
