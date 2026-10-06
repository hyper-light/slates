//! The dispatch's tests (Phase 3 task 1; §4.6): a mock in-memory bridge is driven through the
//! FUSE-wire-to-operation-layer dispatch, so the seam is exercised on every host without a mount.
//! INIT negotiates, LOOKUP/GETATTR/OPEN/READ/WRITE/CREATE/READDIR reach the bridge and their
//! replies decode, a bridge refusal becomes the kernel's negated errno, and an unserved opcode is
//! answered ENOSYS. The mock implements the shared `slates-bridge-core` trait (neutral attributes
//! and typed refusals); the dispatch converts them to the FUSE wire.
// Test harness code: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use slates_bridge_core::{
  Attachments, CacheLifetime, FsStat, NodeAttr, ObjectId, OpContext, RenameFlags, Rights, SetAttr,
  View,
};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::bridge::CACHE_FOREVER;
use slates_bridge_fuse::bridge::{
  Bridge, DirEntry, EILSEQ, EIO, ENOSYS, Reclaimed, reclaim_unreported,
};
use slates_bridge_fuse::reply::{AttrOut, EntryOut};
use slates_bridge_fuse::request::{RenameIn, SetAttrIn};
use slates_db::catalog::{Principal, VolumeId};
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

/// A one-file mock: the root directory (inode 1) holds "hello" (inode 2) with some bytes. It
/// records what reaches the seam (the counters and the last `setattr`/`rename` arguments), so a
/// test can assert both what the edge passed through and what it refused before the seam.
struct Mock {
  content: Vec<u8>,
  mode: u32,
  mtime: i64,
  ctime: i64,
  forgotten: u64,
  referenced: u64,
  /// Open handles released.
  released: u64,
  swept: u64,
  setattr_calls: u64,
  last_setattr: Option<SetAttr>,
  rename_calls: u64,
  last_rename: Option<RenameFlags>,
  /// The mode the last `create` reached the seam with (what the volume would keep).
  last_create_mode: Option<u32>,
  /// The file's extended attributes, by name (the volume's table, A-32).
  xattrs: std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
}

/// Format: the file mode of a regular file, and of a directory.
const FILE_MODE: u32 = 0o100_644;
const DIR_MODE: u32 = 0o040_755;
/// Format: `EINVAL`.
const EINVAL: i32 = 22;
/// Shape: the mock volume's "now" — a value no kernel-filled time in these tests equals, so a
/// resolved `UTIME_NOW` is told apart from a passed-through kernel time.
const NOW_NS: i64 = 1_700_000_000_000_000_000;
/// Shape: the mock's bounded cache window for its live-source file: 1.5 s, so both the seconds
/// and the nanoseconds halves of the wire lifetime are exercised.
const LIVE_WINDOW_NS: u64 = 1_500_000_000;

impl Mock {
  fn file_attr(&self) -> NodeAttr {
    NodeAttr {
      ino: 2,
      generation: 1,
      kind: Kind::File,
      mode: self.mode,
      nlink: 1,
      uid: 0,
      gid: 0,
      size: self.content.len() as u64,
      atime: 0,
      mtime: self.mtime,
      ctime: self.ctime,
      change: 0,
    }
  }
}

impl Bridge for Mock {
  fn root(&mut self, _cx: &OpContext) -> Result<u64, VfsError> {
    Ok(1)
  }
  fn lookup(
    &mut self,
    parent: ObjectId,
    _cx: &OpContext,
    name: &str,
  ) -> Result<NodeAttr, VfsError> {
    if parent.inode == 1 && name == "hello" {
      Ok(self.file_attr())
    } else {
      Err(VfsError::NotFound)
    }
  }
  fn getattr(&mut self, object: ObjectId, _cx: &OpContext) -> Result<NodeAttr, VfsError> {
    match object.inode {
      1 => Ok(NodeAttr {
        ino: 1,
        generation: 0,
        kind: Kind::Dir,
        mode: DIR_MODE,
        nlink: 2,
        uid: 0,
        gid: 0,
        size: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        change: 0,
      }),
      2 => Ok(self.file_attr()),
      _ => Err(VfsError::NotFound),
    }
  }
  fn open(&mut self, object: ObjectId, _cx: &OpContext, _flags: u32) -> Result<u64, VfsError> {
    if object.inode == 2 {
      Ok(7)
    } else {
      Err(VfsError::NotFound)
    }
  }
  /// The mock's attributes follow the volume's rules (`slates_vfs::xattr`): create and replace refuse as
  /// `setxattr(2)` does, a missing name is `NoAttribute`.
  fn xattr_get(
    &mut self,
    _object: ObjectId,
    _cx: &OpContext,
    name: &[u8],
  ) -> Result<Vec<u8>, VfsError> {
    self.xattrs.get(name).cloned().ok_or(VfsError::NoAttribute)
  }

  fn xattr_set(
    &mut self,
    _object: ObjectId,
    _cx: &OpContext,
    name: &[u8],
    value: &[u8],
    how: slates_vfs::xattr::XattrSet,
  ) -> Result<(), VfsError> {
    use slates_vfs::xattr::XattrSet;
    match (how, self.xattrs.contains_key(name)) {
      (XattrSet::Create, true) => return Err(VfsError::AlreadyExists),
      (XattrSet::Replace, false) => return Err(VfsError::NoAttribute),
      _ => {}
    }
    self.xattrs.insert(name.to_vec(), value.to_vec());
    Ok(())
  }

  fn xattr_list(&mut self, _object: ObjectId, _cx: &OpContext) -> Result<Vec<Box<[u8]>>, VfsError> {
    Ok(
      self
        .xattrs
        .keys()
        .map(|name| name.clone().into_boxed_slice())
        .collect(),
    )
  }

  fn xattr_remove(
    &mut self,
    _object: ObjectId,
    _cx: &OpContext,
    name: &[u8],
  ) -> Result<(), VfsError> {
    self
      .xattrs
      .remove(name)
      .map(|_| ())
      .ok_or(VfsError::NoAttribute)
  }

  /// The mock's content has no holes: every byte before its end is data, and the end is the hole.
  fn seek(
    &mut self,
    _object: ObjectId,
    _cx: &OpContext,
    offset: u64,
    data: bool,
  ) -> Result<Option<u64>, VfsError> {
    let len = u64::try_from(self.content.len()).unwrap_or(u64::MAX);
    Ok(match (offset < len, data) {
      (false, _) => None,
      (true, true) => Some(offset),
      (true, false) => Some(len),
    })
  }

  fn read(
    &mut self,
    _object: ObjectId,
    _cx: &OpContext,
    offset: u64,
    size: u32,
    out: &mut Vec<u8>,
  ) -> Result<(), VfsError> {
    let start = usize::try_from(offset)
      .unwrap_or(usize::MAX)
      .min(self.content.len());
    let end = start
      .saturating_add(usize::try_from(size).unwrap_or(0))
      .min(self.content.len());
    out.extend_from_slice(&self.content[start..end]);
    Ok(())
  }
  fn write(
    &mut self,
    _object: ObjectId,
    _cx: &OpContext,
    offset: u64,
    data: &[u8],
  ) -> Result<u32, VfsError> {
    let at = usize::try_from(offset).unwrap_or(0);
    if self.content.len() < at + data.len() {
      self.content.resize(at + data.len(), 0);
    }
    self.content[at..at + data.len()].copy_from_slice(data);
    Ok(u32::try_from(data.len()).unwrap_or(u32::MAX))
  }
  fn admit_allocation(
    &mut self,
    _object: ObjectId,
    _cx: &OpContext,
    _offset: u64,
    _len: u64,
  ) -> Result<(), VfsError> {
    Ok(())
  }
  fn allocate(
    &mut self,
    _object: ObjectId,
    _cx: &OpContext,
    offset: u64,
    len: u64,
  ) -> Result<(), VfsError> {
    let end = usize::try_from(offset + len).unwrap_or(0);
    if self.content.len() < end {
      self.content.resize(end, 0);
    }
    Ok(())
  }
  fn opendir(&mut self, object: ObjectId, _cx: &OpContext) -> Result<u64, VfsError> {
    if object.inode == 1 {
      Ok(9)
    } else {
      Err(VfsError::NotFound)
    }
  }
  fn readdir(
    &mut self,
    _object: ObjectId,
    _cx: &OpContext,
    _fh: u64,
    cookie: u64,
    _limit: usize,
  ) -> Result<Vec<DirEntry>, VfsError> {
    if cookie > 0 {
      return Ok(Vec::new());
    }
    Ok(vec![DirEntry {
      ino: 2,
      kind: Kind::File,
      name: "hello".to_owned(),
      cookie: slates_vfs::FIRST_CHILD_COOKIE,
    }])
  }
  fn create(
    &mut self,
    _parent: ObjectId,
    _cx: &OpContext,
    _name: &str,
    mode: u32,
    _flags: u32,
  ) -> Result<(NodeAttr, u64), VfsError> {
    self.last_create_mode = Some(mode);
    Ok((
      NodeAttr {
        ino: 3,
        generation: 1,
        kind: Kind::File,
        mode: FILE_MODE,
        nlink: 1,
        uid: 0,
        gid: 0,
        size: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        change: 0,
      },
      8,
    ))
  }
  fn release(&mut self, _object: ObjectId, _cx: &OpContext, _fh: u64) -> Result<(), VfsError> {
    self.released = self.released.saturating_add(1);
    Ok(())
  }
  fn reference(&mut self, _object: ObjectId, _cx: &OpContext) -> Result<(), VfsError> {
    self.referenced = self.referenced.saturating_add(1);
    Ok(())
  }
  fn forget(&mut self, _object: ObjectId, _cx: &OpContext, nlookup: u64) {
    self.forgotten = self.forgotten.saturating_add(nlookup);
  }
  fn flush(&mut self, _object: ObjectId, _cx: &OpContext, _fh: u64) -> Result<(), VfsError> {
    Ok(())
  }
  // The operations below are not exercised by these dispatch tests; the mock refuses them.
  fn mknod(
    &mut self,
    _parent: ObjectId,
    _cx: &OpContext,
    _name: &str,
    _mode: u32,
    _kind: Kind,
  ) -> Result<NodeAttr, VfsError> {
    Err(VfsError::SpecialFileOperation)
  }

  fn mkdir(
    &mut self,
    _parent: ObjectId,
    _cx: &OpContext,
    _name: &str,
    _mode: u32,
  ) -> Result<NodeAttr, VfsError> {
    Err(VfsError::Invalid)
  }
  fn unlink(&mut self, _parent: ObjectId, _cx: &OpContext, _name: &str) -> Result<(), VfsError> {
    Err(VfsError::Invalid)
  }
  fn rmdir(&mut self, _parent: ObjectId, _cx: &OpContext, _name: &str) -> Result<(), VfsError> {
    Err(VfsError::Invalid)
  }
  fn symlink(
    &mut self,
    _parent: ObjectId,
    _cx: &OpContext,
    _name: &str,
    _target: &str,
  ) -> Result<NodeAttr, VfsError> {
    Err(VfsError::Invalid)
  }
  fn link(
    &mut self,
    target: ObjectId,
    _new_parent: ObjectId,
    _cx: &OpContext,
    _new_name: &str,
  ) -> Result<NodeAttr, VfsError> {
    if target.inode == 2 {
      Ok(self.file_attr())
    } else {
      Err(VfsError::NotFound)
    }
  }
  fn readlink(&mut self, _object: ObjectId, _cx: &OpContext) -> Result<String, VfsError> {
    Err(VfsError::Invalid)
  }
  fn rename(
    &mut self,
    _op: ObjectId,
    _np: ObjectId,
    _cx: &OpContext,
    _on: &str,
    _nn: &str,
    flags: RenameFlags,
  ) -> Result<(), VfsError> {
    self.rename_calls = self.rename_calls.saturating_add(1);
    self.last_rename = Some(flags);
    Ok(())
  }
  fn setattr(
    &mut self,
    object: ObjectId,
    _cx: &OpContext,
    changes: SetAttr,
  ) -> Result<NodeAttr, VfsError> {
    if object.inode != 2 {
      return Err(VfsError::NotFound);
    }
    self.setattr_calls = self.setattr_calls.saturating_add(1);
    self.last_setattr = Some(changes);
    if let Some(mode) = changes.mode {
      self.mode = mode;
    }
    if let Some(mtime) = changes.mtime {
      self.mtime = mtime;
    }
    if let Some(ctime) = changes.ctime {
      self.ctime = ctime;
    }
    Ok(self.file_attr())
  }
  fn statfs(&mut self, _object: ObjectId, _cx: &OpContext) -> Result<FsStat, VfsError> {
    Err(VfsError::Invalid)
  }
  fn now(&mut self) -> i64 {
    NOW_NS
  }
  fn cache_lifetime(&mut self, object: ObjectId, _cx: &OpContext) -> CacheLifetime {
    // The file follows a live source in this mock (a bounded window of 1.5 s); the root is the
    // volume's own (forever).
    if object.inode == 2 {
      CacheLifetime::Bounded { ns: LIVE_WINDOW_NS }
    } else {
      CacheLifetime::Forever
    }
  }
  fn change_token(&mut self, _object: ObjectId, _cx: &OpContext) -> Result<u64, VfsError> {
    Ok(0)
  }
  fn sweep_attachment(&mut self, _cx: &OpContext) -> Result<(), VfsError> {
    self.swept = self.swept.saturating_add(1);
    Ok(())
  }
}

/// A read-write current-view context, built through the attachment registry the way the daemon
/// would (the registry mints the only `OpContext`; a test cannot fabricate one).
fn test_cx() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

/// Drives the crate's dispatch with a real read-write context, so the call sites stay unchanged.
fn dispatch(message: &[u8], bridge: &mut dyn Bridge, out: &mut [u8]) -> usize {
  slates_bridge_fuse::bridge::dispatch(message, bridge, &test_cx(), out)
}

fn message(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut m = vec![0u8; total];
  m[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  m[4..8].copy_from_slice(&opcode.to_le_bytes());
  m[8..16].copy_from_slice(&unique.to_le_bytes());
  m[16..24].copy_from_slice(&nodeid.to_le_bytes());
  m[IN_HEADER_LEN..].copy_from_slice(body);
  m
}

/// Whether a reply is a negative entry: success, node id 0.
fn is_negative_entry(out: &[u8]) -> bool {
  out[4..8] == [0u8; 4] && u64::from_le_bytes(out[16..24].try_into().unwrap()) == 0
}

fn reply_error(out: &[u8], n: usize) -> i32 {
  assert_eq!(n, OUT_HEADER_LEN);
  i32::from_le_bytes(out[4..8].try_into().unwrap())
}

fn mock() -> Mock {
  Mock {
    content: b"hello world".to_vec(),
    mode: FILE_MODE,
    mtime: 0,
    ctime: 0,
    forgotten: 0,
    referenced: 0,
    released: 0,
    swept: 0,
    setattr_calls: 0,
    last_setattr: None,
    rename_calls: 0,
    last_rename: None,
    last_create_mode: None,
    xattrs: std::collections::BTreeMap::new(),
  }
}

/// A `fuse_create_in` body as the kernel sends it: flags, mode, umask, open_flags, then the name.
fn create_body(mode: u32, name: &str) -> Vec<u8> {
  let mut b = vec![0u8; 16];
  b[4..8].copy_from_slice(&mode.to_le_bytes());
  b.extend_from_slice(name.as_bytes());
  b.push(0);
  b
}

/// Format: where `fuse_attr.mode` sits in a `fuse_attr_out` reply body: after `attr_valid` (8),
/// `attr_valid_nsec` (4), `dummy` (4), and `fuse_attr`'s ino, size, blocks, atime, mtime, ctime (6 × 8)
/// and the three nanosecond parts (3 × 4).
const ATTR_OUT_MODE_AT: usize = 16 + 6 * 8 + 3 * 4;
/// Format: `S_IFREG | 0644` — a regular file's `st_mode` as the kernel sends and expects it — and the
/// permission bits alone, as the volume keeps them.
const KERNEL_FILE_MODE: u32 = 0o100_644;
const FILE_PERMISSIONS: u32 = 0o644;

/// An attribute reply composes the wire `st_mode` from the seam's kind and its permission bits: the
/// volume keeps permission bits alone (`Attrs::mode`), and a real kernel validates every reply's mode
/// for a file type it knows, marking the inode bad (`EIO` after) when there is none. Failed before
/// the fix: the reply carried `0644` (`docs/bugs/2026-09-19-fuse-attribute-replies-carry-no-file-type-bits.md`).
#[test]
fn an_attribute_reply_carries_the_file_type_bits_over_the_seams_permission_bits() {
  let mut m = mock();
  m.mode = FILE_PERMISSIONS;
  let mut out = [0u8; 256];
  let n = dispatch(
    &message(Opcode::GetAttr.to_wire(), 1, 2, &[0u8; 16]),
    &mut m,
    &mut out,
  );
  assert!(
    n > OUT_HEADER_LEN + ATTR_OUT_MODE_AT + 4,
    "an attribute reply"
  );
  let at = OUT_HEADER_LEN + ATTR_OUT_MODE_AT;
  let mode = u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
  assert_eq!(
    mode, KERNEL_FILE_MODE,
    "S_IFREG from the seam's kind, the permission bits from its mode"
  );
}

/// A `CREATE` the kernel sends with `S_IFREG | 0644` reaches the seam as the permission bits alone:
/// the type bits are the wire's, and the volume never keeps them. Failed before the fix (the seam
/// received `0100644`).
#[test]
fn a_create_reaches_the_seam_with_the_permission_bits_alone() {
  let mut m = mock();
  let mut out = [0u8; 512];
  dispatch(
    &message(
      Opcode::Create.to_wire(),
      1,
      1,
      &create_body(KERNEL_FILE_MODE, "new"),
    ),
    &mut m,
    &mut out,
  );
  assert_eq!(
    m.last_create_mode,
    Some(FILE_PERMISSIONS),
    "the kernel's S_IFREG stops at the edge"
  );
}

/// A `fuse_setattr_in` body (88 bytes) with the given `valid` mask and the kernel-filled fields:
/// mode, atime/mtime/ctime in seconds (nanoseconds zero), on the file inode 2.
fn setattr_body(valid: u32, mode: u32, atime_s: u64, mtime_s: u64, ctime_s: u64) -> Vec<u8> {
  let mut body = vec![0u8; 88];
  body[0..4].copy_from_slice(&valid.to_le_bytes());
  body[32..40].copy_from_slice(&atime_s.to_le_bytes());
  body[40..48].copy_from_slice(&mtime_s.to_le_bytes());
  body[48..56].copy_from_slice(&ctime_s.to_le_bytes());
  body[68..72].copy_from_slice(&mode.to_le_bytes());
  body
}

/// A `fuse_rename2_in` body: newdir 1 (the root), the given flags, then `old\0new\0`.
fn rename2_body(flags: u32) -> Vec<u8> {
  let mut body = vec![0u8; 16];
  body[0..8].copy_from_slice(&1u64.to_le_bytes());
  body[8..12].copy_from_slice(&flags.to_le_bytes());
  body.extend_from_slice(b"old\0new\0");
  body
}

/// The reply's `fuse_attr_out.attr` change and modification times in seconds (ctime at 16 + 40,
/// mtime at 16 + 32 within the body after the 16-byte header).
fn reply_times(out: &[u8]) -> (u64, u64) {
  let mtime = u64::from_le_bytes(out[OUT_HEADER_LEN + 16 + 32..][..8].try_into().unwrap());
  let ctime = u64::from_le_bytes(out[OUT_HEADER_LEN + 16 + 40..][..8].try_into().unwrap());
  (mtime, ctime)
}

/// LOOKUP reaches the bridge and its entry reply decodes; a missing name is a negative entry (node id 0) cached
/// for its directory's lifetime, so the kernel answers the next probe of that name itself (§4.6 "Cache posture":
/// a create through any attachment invalidates the name, as it does a positive entry). Python's imports and pip
/// probe thousands of absent names; answered `ENOENT`, each probe was a round trip to the daemon.
#[test]
fn lookup_dispatches_and_a_miss_is_a_negative_entry_cached_for_its_directorys_lifetime() {
  let mut m = mock();
  let mut out = [0u8; 512];
  let n = dispatch(
    &message(Opcode::Lookup.to_wire(), 1, 1, b"hello\0"),
    &mut m,
    &mut out,
  );
  assert_eq!(
    u32::from_le_bytes(out[4..8].try_into().unwrap()),
    0,
    "success"
  );
  assert_eq!(
    u64::from_le_bytes(out[16..24].try_into().unwrap()),
    2,
    "nodeid 2"
  );
  assert_eq!(n, OUT_HEADER_LEN + EntryOut::LEN);
  // The FUSE edge takes exactly one lookup reference on the entry it returns (§3); the kernel's
  // node id now pins the object until FORGET. (NFS, with no FORGET, takes none.)
  assert_eq!(
    m.referenced, 1,
    "a successful FUSE LOOKUP takes one lookup reference"
  );

  let n = dispatch(
    &message(Opcode::Lookup.to_wire(), 2, 1, b"missing\0"),
    &mut m,
    &mut out,
  );
  assert_eq!(n, OUT_HEADER_LEN + EntryOut::LEN, "a full entry reply");
  assert_eq!(
    u32::from_le_bytes(out[4..8].try_into().unwrap()),
    0,
    "success, not ENOENT"
  );
  assert_eq!(
    u64::from_le_bytes(out[16..24].try_into().unwrap()),
    0,
    "node id 0: a negative entry"
  );
  assert_eq!(
    u64::from_le_bytes(out[32..40].try_into().unwrap()),
    CACHE_FOREVER,
    "entry_valid: the root directory's lifetime (the volume's own, forever)"
  );
  assert_eq!(m.referenced, 1, "a LOOKUP miss takes no reference");
}

/// The open flags tell the kernel what it may keep (§4.6 "Cache posture"). Do: open the mock's file (a live source,
/// bounded lifetime) read-only and write-only, and open its root directory (the volume's own, forever). Expect: the
/// read-only file handle carries `FOPEN_NOFLUSH` and no kept cache (an outsider may change a live source), the
/// write-only one neither, and the root's listing is kept (`FOPEN_KEEP_CACHE | FOPEN_CACHE_DIR`).
#[test]
fn open_flags_keep_what_invalidation_covers_and_skip_a_read_only_flush() {
  use slates_bridge_fuse::abi::open::{CACHE_DIR, KEEP_CACHE, NOFLUSH};
  let mut m = mock();
  let mut out = [0u8; 512];
  let open_flags = |out: &[u8]| {
    u32::from_le_bytes(
      out[OUT_HEADER_LEN + 8..OUT_HEADER_LEN + 12]
        .try_into()
        .unwrap(),
    )
  };
  let open_in = |flags: u32| {
    let mut body = flags.to_le_bytes().to_vec();
    body.extend_from_slice(&[0; 4]);
    body
  };
  dispatch(
    &message(Opcode::Open.to_wire(), 1, 2, &open_in(0)),
    &mut m,
    &mut out,
  );
  assert_eq!(
    open_flags(&out),
    NOFLUSH,
    "a read-only handle on a live source: no flush, no kept pages"
  );
  dispatch(
    &message(Opcode::Open.to_wire(), 2, 2, &open_in(1)),
    &mut m,
    &mut out,
  );
  assert_eq!(open_flags(&out), 0, "a write-only handle flushes at close");
  dispatch(
    &message(Opcode::OpenDir.to_wire(), 3, 1, &open_in(0)),
    &mut m,
    &mut out,
  );
  assert_eq!(
    open_flags(&out),
    KEEP_CACHE | CACHE_DIR,
    "the volume's own directory keeps its listing"
  );
}

/// READ returns the requested slice; WRITE mutates the file and reports the count.
#[test]
fn read_and_write_dispatch_to_the_bridge() {
  let mut m = mock();
  let mut out = [0u8; 512];
  // read 5 bytes at offset 6: "world".
  let mut body = vec![0u8; 24];
  body[0..8].copy_from_slice(&7u64.to_le_bytes()); // fh
  body[8..16].copy_from_slice(&6u64.to_le_bytes()); // offset
  body[16..20].copy_from_slice(&5u32.to_le_bytes()); // size
  let n = dispatch(
    &message(Opcode::Read.to_wire(), 1, 2, &body),
    &mut m,
    &mut out,
  );
  assert_eq!(&out[OUT_HEADER_LEN..n], b"world");

  // write "!!" at offset 11.
  let mut w = vec![0u8; 40];
  w[0..8].copy_from_slice(&7u64.to_le_bytes());
  w[8..16].copy_from_slice(&11u64.to_le_bytes());
  w[16..20].copy_from_slice(&2u32.to_le_bytes());
  w.extend_from_slice(b"!!");
  dispatch(
    &message(Opcode::Write.to_wire(), 2, 2, &w),
    &mut m,
    &mut out,
  );
  assert_eq!(m.content, b"hello world!!");
}

/// READDIR packs the directory's entries; INIT negotiates; FORGET reaches the bridge with no
/// reply; an unserved opcode is ENOSYS.
#[test]
fn readdir_init_forget_and_unserved_dispatch() {
  let mut m = mock();
  let mut out = [0u8; 512];
  let mut rd = vec![0u8; 24];
  rd[16..20].copy_from_slice(&256u32.to_le_bytes()); // size
  let n = dispatch(
    &message(Opcode::ReadDir.to_wire(), 1, 1, &rd),
    &mut m,
    &mut out,
  );
  assert!(n > OUT_HEADER_LEN, "the directory entry was packed");

  let mut init = vec![0u8; 16];
  init[0..4].copy_from_slice(&7u32.to_le_bytes());
  init[4..8].copy_from_slice(&31u32.to_le_bytes());
  let n = dispatch(
    &message(Opcode::Init.to_wire(), 1, 0, &init),
    &mut m,
    &mut out,
  );
  assert_eq!(
    u32::from_le_bytes(out[4..8].try_into().unwrap()),
    0,
    "init ok"
  );
  assert!(n > OUT_HEADER_LEN);

  let mut forget = vec![0u8; 8];
  forget[0..8].copy_from_slice(&3u64.to_le_bytes());
  let n = dispatch(
    &message(Opcode::Forget.to_wire(), 1, 2, &forget),
    &mut m,
    &mut out,
  );
  assert_eq!(n, 0, "FORGET has no reply");
  assert_eq!(m.forgotten, 3);

  let n = dispatch(&message(4096, 1, 1, &[]), &mut m, &mut out);
  assert_eq!(reply_error(&out, n), -ENOSYS);
}

/// FSYNC and FSYNCDIR are dispatched (they were answered ENOSYS before — audit BUG-7): the data is
/// already in the anchor, so each is a success no-op with an empty reply, not an unimplemented op.
#[test]
fn fsync_and_fsyncdir_are_served_as_success() {
  let mut m = mock();
  let mut out = [0u8; 256];
  // fuse_fsync_in: fh (8), fsync_flags (4). The file is inode 2.
  let mut body = [0u8; 12];
  body[0..8].copy_from_slice(&7u64.to_le_bytes());
  for opcode in [Opcode::FSync, Opcode::FSyncDir] {
    let n = dispatch(&message(opcode.to_wire(), 1, 2, &body), &mut m, &mut out);
    assert_eq!(
      u32::from_le_bytes(out[4..8].try_into().unwrap()),
      0,
      "{opcode:?} succeeds (not ENOSYS)"
    );
    assert_eq!(n, OUT_HEADER_LEN, "an empty success reply");
  }
}

/// LINK is dispatched (it was ENOSYS before — audit BUG-7): it links an existing inode under a new
/// name and returns that entry, taking a lookup reference on it as LOOKUP/CREATE do.
#[test]
fn link_dispatches_and_returns_the_target_entry() {
  let mut m = mock();
  let mut out = [0u8; 512];
  // fuse_link_in: oldnodeid (8) = inode 2, then the new name in the request's directory (root).
  let mut body = Vec::new();
  body.extend_from_slice(&2u64.to_le_bytes());
  body.extend_from_slice(b"hardlink\0");
  let n = dispatch(
    &message(Opcode::Link.to_wire(), 1, 1, &body),
    &mut m,
    &mut out,
  );
  assert_eq!(
    u32::from_le_bytes(out[4..8].try_into().unwrap()),
    0,
    "LINK succeeded (not ENOSYS)"
  );
  assert_eq!(
    u64::from_le_bytes(out[16..24].try_into().unwrap()),
    2,
    "the new name resolves to the target inode 2"
  );
  assert_eq!(n, OUT_HEADER_LEN + EntryOut::LEN);
  assert_eq!(
    m.referenced, 1,
    "LINK takes a lookup reference on the entry"
  );
}

/// READDIRPLUS is dispatched (it was ENOSYS before — audit BUG-7): each entry carries its
/// attributes (a fuse_entry_out, so the kernel needs no follow-up LOOKUP) and takes a lookup
/// reference, so the reply is larger than a plain readdir and the entry is referenced.
#[test]
fn readdirplus_dispatches_with_entry_attributes_and_references() {
  let mut m = mock();
  let mut out = [0u8; 1024];
  let mut rd = vec![0u8; 24];
  rd[16..20].copy_from_slice(&512u32.to_le_bytes()); // size
  let n = dispatch(
    &message(Opcode::ReadDirPlus.to_wire(), 1, 1, &rd),
    &mut m,
    &mut out,
  );
  assert_eq!(
    u32::from_le_bytes(out[4..8].try_into().unwrap()),
    0,
    "READDIRPLUS succeeded (not ENOSYS)"
  );
  assert!(
    n >= OUT_HEADER_LEN + EntryOut::LEN,
    "the reply carries the entry's attributes (a fuse_entry_out), got {n} bytes"
  );
  assert_eq!(
    m.referenced, 1,
    "READDIRPLUS takes a lookup reference on each returned entry"
  );
}

/// A time flagged `FATTR_ATIME_NOW`/`FATTR_MTIME_NOW` (`UTIME_NOW`) is resolved through the
/// volume's own clock (`Bridge::now`), the NOW resolution the design places at the transport
/// (AC-3.10) — not passed through as the value the kernel filled in from *its* clock; a time set
/// explicitly (`utimensat` with a value) is passed through. Failed at `991c84e`: the kernel's
/// value was passed through for both.
#[test]
fn setattr_now_flags_resolve_to_the_volume_clock_not_the_kernels_value() {
  let mut m = mock();
  let mut out = [0u8; 256];
  let valid = SetAttrIn::FATTR_ATIME
    | SetAttrIn::FATTR_ATIME_NOW
    | SetAttrIn::FATTR_MTIME
    | SetAttrIn::FATTR_MTIME_NOW;
  let n = dispatch(
    &message(
      Opcode::SetAttr.to_wire(),
      1,
      2,
      &setattr_body(valid, 0, 111, 222, 0),
    ),
    &mut m,
    &mut out,
  );
  assert_eq!(
    u32::from_le_bytes(out[4..8].try_into().unwrap()),
    0,
    "success"
  );
  assert_eq!(n, OUT_HEADER_LEN + AttrOut::LEN);
  let changes = m.last_setattr.expect("the seam was reached");
  assert_eq!(changes.atime, Some(NOW_NS), "atime is the volume's now");
  assert_eq!(changes.mtime, Some(NOW_NS), "mtime is the volume's now");

  // An explicit mtime with a NOW atime: the explicit value passes through, the NOW resolves.
  let valid = SetAttrIn::FATTR_ATIME | SetAttrIn::FATTR_ATIME_NOW | SetAttrIn::FATTR_MTIME;
  dispatch(
    &message(
      Opcode::SetAttr.to_wire(),
      2,
      2,
      &setattr_body(valid, 0, 111, 222, 0),
    ),
    &mut m,
    &mut out,
  );
  let changes = m.last_setattr.unwrap();
  assert_eq!(changes.atime, Some(NOW_NS));
  assert_eq!(
    changes.mtime,
    Some(222 * 1_000_000_000),
    "an explicit time passes through"
  );
  assert_eq!(m.setattr_calls, 2);
}

/// `FATTR_CTIME` — a kernel flushing the timestamps it kept under its writeback cache — is
/// carried to the seam as the change time, never dropped.
#[test]
fn setattr_carries_the_change_time_a_writeback_kernel_flushes() {
  let mut m = mock();
  let mut out = [0u8; 256];
  let valid = SetAttrIn::FATTR_MTIME | SetAttrIn::FATTR_CTIME;
  dispatch(
    &message(
      Opcode::SetAttr.to_wire(),
      1,
      2,
      &setattr_body(valid, 0, 0, 222, 333),
    ),
    &mut m,
    &mut out,
  );
  let changes = m.last_setattr.expect("the seam was reached");
  assert_eq!(changes.mtime, Some(222 * 1_000_000_000));
  assert_eq!(
    changes.ctime,
    Some(333 * 1_000_000_000),
    "the change time is carried"
  );
  assert_eq!(changes.atime, None, "a time not asked for is not set");
}

/// A `valid` bit the edge does not honour is refused `EINVAL` before the seam — never a success
/// that silently ignored a requested field (§4.6; audit BUG-8). Failed at `991c84e`: the unknown
/// bit was ignored and the mode applied with a success reply.
#[test]
fn setattr_with_an_unhonoured_valid_bit_is_einval_and_never_reaches_the_seam() {
  let mut m = mock();
  let mut out = [0u8; 256];
  // A header bit above every FATTR the kernel defines today, alongside a legitimate mode change.
  let unknown = 1u32 << 20;
  let n = dispatch(
    &message(
      Opcode::SetAttr.to_wire(),
      1,
      2,
      &setattr_body(SetAttrIn::FATTR_MODE | unknown, 0o600, 0, 0, 0),
    ),
    &mut m,
    &mut out,
  );
  assert_eq!(reply_error(&out, n), -EINVAL, "refused, not acknowledged");
  assert_eq!(m.setattr_calls, 0, "nothing reached the seam");
  assert_eq!(m.mode, FILE_MODE, "the mode is untouched");
}

/// `FATTR_KILL_SUIDGID` (a truncate by a caller without `CAP_FSETID`) clears the set-user-id bit
/// and — the group-execute bit being set — the set-group-id bit, from the object's current mode
/// when no mode was requested, and is applied together with the size. Failed at `991c84e`: the
/// bit was ignored and the file kept its privileges.
#[test]
fn setattr_kill_suidgid_clears_the_privilege_bits() {
  let mut m = mock();
  m.mode = 0o106_755; // S_IFREG | S_ISUID | S_ISGID | rwxr-xr-x
  let mut out = [0u8; 256];
  let valid = SetAttrIn::FATTR_SIZE | SetAttrIn::FATTR_KILL_SUIDGID;
  let mut body = setattr_body(valid, 0, 0, 0, 0);
  body[16..24].copy_from_slice(&5u64.to_le_bytes()); // size
  let n = dispatch(
    &message(Opcode::SetAttr.to_wire(), 1, 2, &body),
    &mut m,
    &mut out,
  );
  assert_eq!(
    u32::from_le_bytes(out[4..8].try_into().unwrap()),
    0,
    "success"
  );
  assert_eq!(n, OUT_HEADER_LEN + AttrOut::LEN);
  let changes = m.last_setattr.expect("the seam was reached");
  assert_eq!(changes.size, Some(5));
  assert_eq!(
    changes.mode,
    Some(0o100_755),
    "suid and sgid cleared, the rest kept"
  );

  // A set-group-id bit without group execute is mandatory locking, not a privilege: kept.
  m.mode = 0o102_745; // S_IFREG | S_ISGID | rwxr--r-x
  dispatch(
    &message(Opcode::SetAttr.to_wire(), 2, 2, &body),
    &mut m,
    &mut out,
  );
  assert_eq!(m.last_setattr.unwrap().mode, Some(0o102_745));
}

/// A `RENAME2` flag the seam does not carry (`RENAME_WHITEOUT`, or a bit the header has not
/// defined) is refused `EINVAL` before the seam — never dropped and performed as a plain rename
/// (§4.6; audit BUG-10) — while the carried flags reach the seam as themselves. Failed at
/// `991c84e`: the flag was dropped and the rename performed.
#[test]
fn rename2_with_a_flag_the_seam_does_not_carry_is_einval_and_never_reaches_the_seam() {
  let mut m = mock();
  let mut out = [0u8; 256];
  for foreign in [RenameIn::RENAME_WHITEOUT, 1u32 << 9] {
    let n = dispatch(
      &message(Opcode::Rename2.to_wire(), 1, 1, &rename2_body(foreign)),
      &mut m,
      &mut out,
    );
    assert_eq!(
      reply_error(&out, n),
      -EINVAL,
      "flag {foreign:#x} is refused"
    );
    assert_eq!(m.rename_calls, 0, "nothing reached the seam");
  }

  let n = dispatch(
    &message(
      Opcode::Rename2.to_wire(),
      2,
      1,
      &rename2_body(RenameIn::RENAME_NOREPLACE),
    ),
    &mut m,
    &mut out,
  );
  assert_eq!(n, OUT_HEADER_LEN, "an empty success reply");
  assert_eq!(
    m.last_rename,
    Some(RenameFlags {
      no_replace: true,
      exchange: false,
    }),
    "a carried flag reaches the seam as itself"
  );
}

/// Format: `S_IFIFO | 0644`, a mode mknod serves.
const FIFO_MODE: u32 = 0o010_644;

/// The body of `opcode` with `name` (and `second` where the opcode carries two names) after the opcode's fixed
/// part, laid out as the kernel sends it (`fuse_kernel.h`: mkdir 8 bytes, mknod and create and rename2 16, link and
/// rename 8, lookup, unlink, rmdir and symlink none).
fn named_body(opcode: Opcode, name: &[u8], second: &[u8]) -> Vec<u8> {
  let fixed = match opcode {
    Opcode::MkDir | Opcode::Link | Opcode::Rename => 8,
    Opcode::MkNod | Opcode::Create | Opcode::Rename2 => 16,
    _ => 0,
  };
  let mut body = vec![0u8; fixed];
  if opcode == Opcode::MkNod {
    // A fifo: mknod of any other type is refused for its type before the name is read.
    body[..4].copy_from_slice(&FIFO_MODE.to_le_bytes());
  }
  body.extend_from_slice(name);
  body.push(0);
  if matches!(opcode, Opcode::SymLink | Opcode::Rename | Opcode::Rename2) {
    body.extend_from_slice(second);
    body.push(0);
  }
  body
}

/// §4.6, D-4: a volume's names are character strings (UTF-8), the form every target slates serves or lands on can
/// hold (NFSv4 refuses others `NFS4ERR_INVAL`, RFC 8881 §14.4; APFS `EILSEQ`; NTFS holds UTF-16). Do: send every
/// opcode that carries a name with a name that is not UTF-8 (`bad\xff`), in either name position; then one name with
/// no terminating NUL. Expect: `EILSEQ` for each, nothing reaching the seam; `EIO` only for the malformed message.
#[test]
fn a_name_that_is_not_utf8_is_refused_eilseq_by_every_opcode_and_never_reaches_the_seam() {
  let not_utf8: &[u8] = b"bad\xff";
  let mut m = mock();
  let mut out = [0u8; 256];
  let opcodes = [
    Opcode::Lookup,
    Opcode::MkDir,
    Opcode::MkNod,
    Opcode::Create,
    Opcode::Unlink,
    Opcode::RmDir,
    Opcode::SymLink,
    Opcode::Link,
    Opcode::Rename,
    Opcode::Rename2,
  ];
  let mut unique = 1;
  for opcode in opcodes {
    let positions: &[(&[u8], &[u8])] =
      if matches!(opcode, Opcode::SymLink | Opcode::Rename | Opcode::Rename2) {
        &[(not_utf8, b"fine"), (b"fine", not_utf8)]
      } else {
        &[(not_utf8, b"")]
      };
    for (name, second) in positions {
      unique += 1;
      let n = dispatch(
        &message(
          opcode.to_wire(),
          unique,
          1,
          &named_body(opcode, name, second),
        ),
        &mut m,
        &mut out,
      );
      assert_eq!(
        reply_error(&out, n),
        -EILSEQ,
        "{opcode:?} with {name:?}, {second:?}"
      );
    }
  }
  assert_eq!(m.rename_calls, 0, "no rename reached the seam");
  assert_eq!(m.last_create_mode, None, "no create reached the seam");
  let n = dispatch(
    &message(Opcode::Lookup.to_wire(), 99, 1, b"unterminated"),
    &mut m,
    &mut out,
  );
  assert_eq!(
    reply_error(&out, n),
    -EIO,
    "a name with no NUL is a malformed message"
  );
}

/// A `fuse_setxattr_in` body (size, flags), the NUL-terminated name, then the value.
fn setxattr_body(name: &[u8], value: &[u8], flags: u32) -> Vec<u8> {
  let mut body = Vec::new();
  body.extend_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
  body.extend_from_slice(&flags.to_le_bytes());
  body.extend_from_slice(name);
  body.push(0);
  body.extend_from_slice(value);
  body
}

/// A `fuse_getxattr_in` body (the caller's buffer size, padding), then the name when there is one.
fn getxattr_body(size: u32, name: Option<&[u8]>) -> Vec<u8> {
  let mut body = Vec::new();
  body.extend_from_slice(&size.to_le_bytes());
  body.extend_from_slice(&0u32.to_le_bytes());
  if let Some(name) = name {
    body.extend_from_slice(name);
    body.push(0);
  }
  body
}

/// One attribute request against `m`: the reply's status (0 or a negative errno) and its body.
fn attribute_request(m: &mut Mock, opcode: Opcode, body: &[u8]) -> (i32, Vec<u8>) {
  let mut out = [0u8; 512];
  let n = dispatch(&message(opcode.to_wire(), 1, 2, body), m, &mut out);
  let status = i32::from_le_bytes(out[4..8].try_into().unwrap());
  (status, out[OUT_HEADER_LEN..n].to_vec())
}

fn set_attribute(m: &mut Mock, name: &[u8], value: &[u8], flags: u32) -> i32 {
  attribute_request(m, Opcode::SetXattr, &setxattr_body(name, value, flags)).0
}

fn get_attribute(m: &mut Mock, name: &[u8], size: u32) -> (i32, Vec<u8>) {
  attribute_request(m, Opcode::GetXattr, &getxattr_body(size, Some(name)))
}

fn list_attributes(m: &mut Mock, size: u32) -> (i32, Vec<u8>) {
  attribute_request(m, Opcode::ListXattr, &getxattr_body(size, None))
}

fn remove_attribute(m: &mut Mock, name: &[u8]) -> i32 {
  let mut body = name.to_vec();
  body.push(0);
  attribute_request(m, Opcode::RemoveXattr, &body).0
}

/// §4.5 A-32 through the Linux mount (T-1.21 xattrs): do set a `user.` attribute, ask its length (size 0), read it,
/// list the names, replace it, then remove it. Expect each answered as `getxattr(2)`, `listxattr(2)` and `setxattr(2)`
/// specify: the length in `fuse_getxattr_out`, the value, the name NUL-terminated, the replacement read back, and a
/// removed name `ENODATA`. Before 2026-10-06 every attribute operation was `ENOSYS` (the kernel then told every caller
/// "not supported"), though the volume core has held attributes since A-32 (found in an adversarial container run).
#[test]
fn a_user_attribute_is_set_read_listed_replaced_and_removed() {
  let mut m = mock();
  assert_eq!(
    set_attribute(&mut m, b"user.origin", b"https://example", 0),
    0
  );
  let (status, length) = get_attribute(&mut m, b"user.origin", 0);
  assert_eq!(
    (status, &length[..4]),
    (0, &15u32.to_le_bytes()[..]),
    "the length when asked with size 0"
  );
  assert_eq!(
    get_attribute(&mut m, b"user.origin", 64),
    (0, b"https://example".to_vec())
  );
  assert_eq!(list_attributes(&mut m, 64), (0, b"user.origin\0".to_vec()));
  assert_eq!(
    set_attribute(&mut m, b"user.origin", b"x", 1),
    -17,
    "XATTR_CREATE over a set name is EEXIST"
  );
  assert_eq!(
    set_attribute(&mut m, b"user.origin", b"replaced", 2),
    0,
    "XATTR_REPLACE of a set name"
  );
  assert_eq!(
    get_attribute(&mut m, b"user.origin", 64),
    (0, b"replaced".to_vec())
  );
  assert_eq!(remove_attribute(&mut m, b"user.origin"), 0);
  assert_eq!(
    get_attribute(&mut m, b"user.origin", 64).0,
    -61,
    "a removed name is ENODATA"
  );
}

/// Names outside `user.` (the host LSM's, its administrator's, its ACLs', and an empty `user.` key): do set and read
/// each. Expect `EOPNOTSUPP` for the set and `ENODATA` for the read, and nothing stored.
#[test]
fn attribute_names_outside_the_user_namespace_are_never_stored() {
  let mut m = mock();
  for name in [
    &b"security.selinux"[..],
    b"trusted.x",
    b"system.posix_acl_access",
    b"user.",
  ] {
    assert_eq!(set_attribute(&mut m, name, b"v", 0), -95, "{name:?}");
    assert_eq!(get_attribute(&mut m, name, 64).0, -61, "{name:?}");
  }
  assert!(m.xattrs.is_empty());
}

/// Hostile and boundary attribute requests: do read and list into a buffer one byte too small, replace a name never
/// set, and send bodies cut short. Expect `ERANGE`, `ERANGE`, `ENODATA` and `EIO`, never a panic, and nothing stored that a request did not carry whole.
#[test]
fn attribute_requests_outside_the_rules_are_refused_typed() {
  let mut m = mock();
  assert_eq!(set_attribute(&mut m, b"user.k", b"value", 0), 0);
  assert_eq!(get_attribute(&mut m, b"user.k", 4).0, -34);
  assert_eq!(list_attributes(&mut m, 3).0, -34);
  assert_eq!(set_attribute(&mut m, b"user.never", b"v", 2), -61);
  let mut cut = setxattr_body(b"user.cut", b"0123456789", 0);
  cut.truncate(cut.len() - 3);
  assert_eq!(
    attribute_request(&mut m, Opcode::SetXattr, &cut).0,
    -5,
    "a value shorter than its size"
  );
  assert_eq!(
    attribute_request(&mut m, Opcode::SetXattr, &[1, 0]).0,
    -5,
    "a body shorter than its fixed part"
  );
  assert_eq!(attribute_request(&mut m, Opcode::GetXattr, &[]).0, -5);
  assert_eq!(
    m.xattrs.len(),
    1,
    "nothing was stored from a refused request"
  );
}

/// An attribute reply carries the object's change time in the wire change time, not its
/// modification time: a chmod moves only the change time, and a tool watching it sees the move.
/// Failed at `991c84e`: the wire ctime was the mtime (a quirk carried since the extraction).
#[test]
fn replies_carry_the_change_time_not_the_modification_time() {
  let mut m = mock();
  m.mtime = 100 * 1_000_000_000;
  m.ctime = 200 * 1_000_000_000;
  let mut out = [0u8; 256];
  dispatch(
    &message(Opcode::GetAttr.to_wire(), 1, 2, &[0u8; 16]),
    &mut m,
    &mut out,
  );
  assert_eq!(
    reply_times(&out),
    (100, 200),
    "(mtime, ctime) as the object has them"
  );
}

/// The kernel cache lifetime a reply carries is the seam's posture for that object (§4.6 "Cache
/// posture"): a live-source object's LOOKUP entry and GETATTR attributes carry the bounded window
/// (1.5 s: seconds 1, nanoseconds 500,000,000, at `fuse_entry_out`'s offsets 16/24 and 32/36 and
/// `fuse_attr_out`'s 0 and 8), and the volume's own root carries forever. Before the sweep every
/// reply carried forever, whatever the source.
#[test]
fn replies_carry_the_seams_cache_lifetime_for_each_object() {
  let mut m = mock();
  let mut out = [0u8; 512];
  dispatch(
    &message(Opcode::Lookup.to_wire(), 1, 1, b"hello\0"),
    &mut m,
    &mut out,
  );
  let entry = &out[OUT_HEADER_LEN..];
  let at_u64 = |at: usize| u64::from_le_bytes(entry[at..at + 8].try_into().unwrap());
  let at_u32 = |at: usize| u32::from_le_bytes(entry[at..at + 4].try_into().unwrap());
  assert_eq!(
    (at_u64(16), at_u32(32)),
    (1, 500_000_000),
    "entry_valid and entry_valid_nsec: the live-source window"
  );
  assert_eq!(
    (at_u64(24), at_u32(36)),
    (1, 500_000_000),
    "attr_valid and attr_valid_nsec"
  );

  dispatch(
    &message(Opcode::GetAttr.to_wire(), 2, 2, &[0u8; 16]),
    &mut m,
    &mut out,
  );
  let attrs = &out[OUT_HEADER_LEN..];
  assert_eq!(
    (
      u64::from_le_bytes(attrs[0..8].try_into().unwrap()),
      u32::from_le_bytes(attrs[8..12].try_into().unwrap())
    ),
    (1, 500_000_000),
    "a GETATTR of the live file carries the window"
  );

  dispatch(
    &message(Opcode::GetAttr.to_wire(), 3, 1, &[0u8; 16]),
    &mut m,
    &mut out,
  );
  let attrs = &out[OUT_HEADER_LEN..];
  assert_eq!(
    (
      u64::from_le_bytes(attrs[0..8].try_into().unwrap()),
      u32::from_le_bytes(attrs[8..12].try_into().unwrap())
    ),
    (u64::MAX, 0),
    "the volume's own root is cached until an explicit invalidation"
  );
}

/// Format: `FUSE_BATCH_FORGET`, the batched forget's opcode (`include/uapi/linux/fuse.h`).
const FUSE_BATCH_FORGET: u32 = 42;

/// FUSE_BATCH_FORGET — the batched form of FORGET the kernel sends over `/dev/fuse` and the
/// high-priority queue of virtio-fs carries (virtio 1.2 §5.11.6.2) — drops every listed reference
/// and, like FORGET, has no reply. A count that claims more entries than the body holds applies
/// only the complete entries present: the parser never reads past the body.
#[test]
fn batch_forget_drops_every_listed_reference_with_no_reply() {
  let mut m = mock();
  let mut out = [0u8; 256];
  // fuse_batch_forget_in: count (4), dummy (4); then count × fuse_forget_one: nodeid (8), nlookup (8).
  let mut body = Vec::new();
  body.extend_from_slice(&2u32.to_le_bytes());
  body.extend_from_slice(&0u32.to_le_bytes());
  for (nodeid, nlookup) in [(2u64, 3u64), (3u64, 4u64)] {
    body.extend_from_slice(&nodeid.to_le_bytes());
    body.extend_from_slice(&nlookup.to_le_bytes());
  }
  let n = dispatch(&message(FUSE_BATCH_FORGET, 1, 0, &body), &mut m, &mut out);
  assert_eq!(n, 0, "BATCH_FORGET has no reply (was ENOSYS: {n} bytes)");
  assert_eq!(m.forgotten, 7, "both entries' references were dropped");

  let mut overclaimed = Vec::new();
  overclaimed.extend_from_slice(&u32::MAX.to_le_bytes());
  overclaimed.extend_from_slice(&0u32.to_le_bytes());
  overclaimed.extend_from_slice(&2u64.to_le_bytes());
  overclaimed.extend_from_slice(&5u64.to_le_bytes());
  overclaimed.extend_from_slice(&[0xFF; 7]);
  let n = dispatch(
    &message(FUSE_BATCH_FORGET, 2, 0, &overclaimed),
    &mut m,
    &mut out,
  );
  assert_eq!(n, 0);
  assert_eq!(
    m.forgotten, 12,
    "the one complete entry applied; the claimed count and the trailing partial entry did not"
  );
}

/// FUSE_DESTROY — the kernel's last request at unmount (on virtio-fs, when the guest unmounts the
/// tag) — is served, not answered ENOSYS: the attachment's references are swept (the bridge's
/// `sweep_attachment`, since the kernel guarantees no FORGET per outstanding reference) and the
/// reply is an empty success.
#[test]
fn destroy_sweeps_the_attachment_and_replies_success() {
  let mut m = mock();
  let mut out = [0u8; 256];
  let n = dispatch(
    &message(Opcode::Destroy.to_wire(), 1, 0, &[]),
    &mut m,
    &mut out,
  );
  assert_eq!(n, OUT_HEADER_LEN, "an empty success reply");
  assert_eq!(
    u32::from_le_bytes(out[4..8].try_into().unwrap()),
    0,
    "DESTROY succeeds (not ENOSYS)"
  );
  assert_eq!(m.swept, 1, "the attachment's references were swept once");
}

/// AUD-29-85. Do: dispatch a CREATE, a LOOKUP and a WRITE with reply room for the header alone (the guest
/// driver posted 16 bytes), then a READDIRPLUS asking for 512 bytes with room for the header and less than
/// one entry. Expect: the three fixed-size requests answer `EIO` before any effect — no create reaches the
/// seam, no lookup reference is taken, the file's bytes are unchanged (before, the CREATE made the file and
/// its handle and the WRITE changed the bytes, then the reply did not fit); and the page is clamped to the
/// room, so it takes no reference it cannot return.
#[test]
fn a_reply_with_no_room_refuses_before_any_effect() {
  let mut m = mock();
  let mut header_only = [0u8; OUT_HEADER_LEN];
  let n = dispatch(
    &message(
      Opcode::Create.to_wire(),
      1,
      1,
      &create_body(0o100_644, "new"),
    ),
    &mut m,
    &mut header_only,
  );
  assert_eq!(reply_error(&header_only, n), -5, "CREATE refused EIO");
  assert_eq!(
    m.last_create_mode, None,
    "the create never reached the seam"
  );
  let mut lookup = b"child".to_vec();
  lookup.push(0);
  let n = dispatch(
    &message(Opcode::Lookup.to_wire(), 2, 1, &lookup),
    &mut m,
    &mut header_only,
  );
  assert_eq!(reply_error(&header_only, n), -5, "LOOKUP refused EIO");
  assert_eq!(m.referenced, 0, "no lookup reference was taken");
  let mut write = vec![0u8; 40];
  write[0..8].copy_from_slice(&7u64.to_le_bytes());
  write[8..16].copy_from_slice(&0u64.to_le_bytes());
  write[16..20].copy_from_slice(&2u32.to_le_bytes());
  write.extend_from_slice(b"XX");
  let n = dispatch(
    &message(Opcode::Write.to_wire(), 3, 2, &write),
    &mut m,
    &mut header_only,
  );
  assert_eq!(reply_error(&header_only, n), -5, "WRITE refused EIO");
  assert_eq!(m.content, b"hello world", "the bytes are unchanged");
  /// Shape: room for the header and less than one directory-plus entry.
  const SHORT_PAGE: usize = OUT_HEADER_LEN + EntryOut::LEN / 2;
  let mut short_page = [0u8; SHORT_PAGE];
  let mut readdir = vec![0u8; 24];
  readdir[16..20].copy_from_slice(&512u32.to_le_bytes());
  let n = dispatch(
    &message(Opcode::ReadDirPlus.to_wire(), 4, 1, &readdir),
    &mut m,
    &mut short_page,
  );
  assert!(
    (OUT_HEADER_LEN..=SHORT_PAGE).contains(&n),
    "the page fits its room: {n}"
  );
  assert_eq!(
    m.referenced, 0,
    "no entry was referenced that the page could not return"
  );
}

/// AUD-29-85. Do: dispatch a READDIRPLUS and a CREATE with room, then treat each reply as one that never
/// reached its caller and reclaim it; reclaim an error reply too. Expect: every lookup reference the page
/// took is forgotten (the net is zero) without the synthetic "." and ".." ever being forgotten; the CREATE's
/// reference is forgotten and its handle released; an error reply gives back nothing.
#[test]
fn reclaiming_an_unreported_reply_gives_back_exactly_what_it_granted() {
  let mut m = mock();
  let cx = test_cx();
  let mut out = [0u8; 1024];
  let mut readdir = vec![0u8; 24];
  readdir[16..20].copy_from_slice(&512u32.to_le_bytes());
  let n = dispatch(
    &message(Opcode::ReadDirPlus.to_wire(), 1, 1, &readdir),
    &mut m,
    &mut out,
  );
  let taken = m.referenced;
  assert!(taken > 0, "the page referenced its entries");
  let reclaimed = reclaim_unreported(Some(Opcode::ReadDirPlus), 1, &out[..n], &mut m, &cx);
  assert_eq!(
    reclaimed,
    Reclaimed {
      references: taken,
      handles: 0
    }
  );
  assert_eq!(m.forgotten, taken, "every reference the page took, no more");

  let before = (m.referenced, m.forgotten);
  let n = dispatch(
    &message(
      Opcode::Create.to_wire(),
      2,
      1,
      &create_body(FILE_MODE, "made"),
    ),
    &mut m,
    &mut out,
  );
  assert_eq!(&out[4..8], &[0u8; 4], "the create succeeded");
  let reclaimed = reclaim_unreported(Some(Opcode::Create), 1, &out[..n], &mut m, &cx);
  assert_eq!(
    reclaimed,
    Reclaimed {
      references: m.referenced - before.0,
      handles: 1
    }
  );
  assert_eq!(m.forgotten - before.1, m.referenced - before.0);
  assert_eq!(m.released, 1);

  let mut lookup = b"absent".to_vec();
  lookup.push(0);
  let n = dispatch(
    &message(Opcode::Lookup.to_wire(), 3, 1, &lookup),
    &mut m,
    &mut out,
  );
  // The miss is a negative entry (node id 0): a lost reply of it granted nothing, so nothing is reclaimed.
  assert!(
    is_negative_entry(&out),
    "the lookup missed as a negative entry"
  );
  assert_eq!(
    reclaim_unreported(Some(Opcode::Lookup), 1, &out[..n], &mut m, &cx),
    Reclaimed::default()
  );
}
