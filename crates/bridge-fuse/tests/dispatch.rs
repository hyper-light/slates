//! The dispatch's tests (Phase 3 task 1; §4.6): a mock in-memory bridge is driven through the
//! FUSE-wire-to-operation-layer dispatch, so the seam is exercised on every host without a mount.
//! INIT negotiates, LOOKUP/GETATTR/OPEN/READ/WRITE/CREATE/READDIR reach the bridge and their
//! replies decode, a bridge refusal becomes the kernel's negated errno, and an unserved opcode is
//! answered ENOSYS. The mock implements the shared `slates-bridge-core` trait (neutral attributes
//! and typed refusals); the dispatch converts them to the FUSE wire.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{
  Attachments, CacheLifetime, FsStat, NodeAttr, ObjectId, OpContext, RenameFlags, Rights, SetAttr,
  View,
};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::bridge::{Bridge, DirEntry, ENOSYS};
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
  swept: u64,
  setattr_calls: u64,
  last_setattr: Option<SetAttr>,
  rename_calls: u64,
  last_rename: Option<RenameFlags>,
}

/// Format: the file mode of a regular file, and of a directory.
const FILE_MODE: u32 = 0o100_644;
const DIR_MODE: u32 = 0o040_755;
/// Format: `ENOENT`.
const ENOENT: i32 = 2;
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
    offset: u64,
  ) -> Result<Vec<DirEntry>, VfsError> {
    if offset > 0 {
      return Ok(Vec::new());
    }
    Ok(vec![DirEntry {
      ino: 2,
      kind: Kind::File,
      name: "hello".to_owned(),
    }])
  }
  fn create(
    &mut self,
    _parent: ObjectId,
    _cx: &OpContext,
    _name: &str,
    _mode: u32,
    _flags: u32,
  ) -> Result<(NodeAttr, u64), VfsError> {
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
      },
      8,
    ))
  }
  fn release(&mut self, _object: ObjectId, _cx: &OpContext, _fh: u64) -> Result<(), VfsError> {
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
    swept: 0,
    setattr_calls: 0,
    last_setattr: None,
    rename_calls: 0,
    last_rename: None,
  }
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

/// LOOKUP reaches the bridge and its entry reply decodes; a missing name is ENOENT.
#[test]
fn lookup_dispatches_and_a_miss_is_enoent() {
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
  assert_eq!(reply_error(&out, n), -ENOENT, "the negated errno");
  assert_eq!(m.referenced, 1, "a LOOKUP miss takes no reference");
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

/// The extended-attribute operations (`SETXATTR`, `GETXATTR`, `LISTXATTR`, `REMOVEXATTR`) are
/// answered `ENOSYS`, the precise unsupported error: the kernel turns a daemon's `ENOSYS` into
/// `EOPNOTSUPP` for the caller and stops asking, so a tool sees "not supported", never an ignored
/// success (§4.6 "Extended attributes ... have explicit capability contracts"; T-1.21 xattrs). The
/// volume core carries no extended attributes (`crates/vfs/src/export.rs`).
#[test]
fn xattr_operations_are_answered_enosys_the_precise_unsupported_error() {
  /// Format: `FUSE_SETXATTR`, `FUSE_GETXATTR`, `FUSE_LISTXATTR`, `FUSE_REMOVEXATTR`.
  const XATTR_OPCODES: [u32; 4] = [21, 22, 23, 24];
  let mut m = mock();
  let mut out = [0u8; 256];
  for opcode in XATTR_OPCODES {
    // fuse_setxattr_in / fuse_getxattr_in bodies are irrelevant: the opcode is unserved.
    let n = dispatch(&message(opcode, 1, 2, b"user.k\0v"), &mut m, &mut out);
    assert_eq!(reply_error(&out, n), -ENOSYS, "opcode {opcode}");
  }
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
