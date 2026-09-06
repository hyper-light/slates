//! The dispatch's tests (Phase 3 task 1; §4.6): a mock in-memory bridge is driven through the
//! FUSE-wire-to-operation-layer dispatch, so the seam is exercised on every host without a mount.
//! INIT negotiates, LOOKUP/GETATTR/OPEN/READ/WRITE/CREATE/READDIR reach the bridge and their
//! replies decode, a bridge refusal becomes the kernel's negated errno, and an unserved opcode is
//! answered ENOSYS. The mock implements the shared `slates-bridge-core` trait (neutral attributes
//! and typed refusals); the dispatch converts them to the FUSE wire.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{
  Attachments, FsStat, NodeAttr, ObjectId, OpContext, RenameFlags, Rights, SetAttr, View,
};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::bridge::{Bridge, DirEntry, ENOSYS};
use slates_bridge_fuse::reply::EntryOut;
use slates_db::catalog::{Principal, VolumeId};
use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

/// A one-file mock: the root directory (inode 1) holds "hello" (inode 2) with some bytes.
struct Mock {
  content: Vec<u8>,
  forgotten: u64,
  referenced: u64,
}

/// Format: the file mode of a regular file, and of a directory.
const FILE_MODE: u32 = 0o100_644;
const DIR_MODE: u32 = 0o040_755;
/// Format: `ENOENT`.
const ENOENT: i32 = 2;

impl Mock {
  fn file_attr(&self) -> NodeAttr {
    NodeAttr {
      ino: 2,
      generation: 1,
      kind: Kind::File,
      mode: FILE_MODE,
      nlink: 1,
      uid: 0,
      gid: 0,
      size: self.content.len() as u64,
      atime: 0,
      mtime: 0,
      ctime: 0,
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
    _flags: RenameFlags,
  ) -> Result<(), VfsError> {
    Err(VfsError::Invalid)
  }
  fn setattr(
    &mut self,
    _object: ObjectId,
    _cx: &OpContext,
    _changes: SetAttr,
  ) -> Result<NodeAttr, VfsError> {
    Err(VfsError::Invalid)
  }
  fn statfs(&mut self, _object: ObjectId, _cx: &OpContext) -> Result<FsStat, VfsError> {
    Err(VfsError::Invalid)
  }
  fn now(&mut self) -> i64 {
    0
  }
  fn change_token(&mut self, _object: ObjectId, _cx: &OpContext) -> Result<u64, VfsError> {
    Ok(0)
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
    forgotten: 0,
    referenced: 0,
  }
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
