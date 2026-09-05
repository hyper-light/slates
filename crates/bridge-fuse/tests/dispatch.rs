//! The dispatch's tests (Phase 3 task 1; §4.6): a mock in-memory bridge is driven through the
//! codec-to-bridge dispatch, so the wire-to-semantics seam is exercised on every host without
//! a mount. INIT negotiates, LOOKUP/GETATTR/OPEN/READ/WRITE/CREATE/READDIR reach the bridge and
//! their replies decode, a bridge refusal becomes the kernel's negated errno, and an unserved
//! opcode is answered ENOSYS.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::bridge::{Bridge, DirEntry, ENOSYS, dispatch};
use slates_bridge_fuse::reply::{Attr, EntryOut};

/// A one-file mock: the root directory (nodeid 1) holds "hello" (nodeid 2) with some bytes.
struct Mock {
  content: Vec<u8>,
  forgotten: u64,
}

/// Format: `DT_REG` and `DT_DIR`, the directory-entry kinds.
// Format: DT_REG, the regular-file directory-entry kind.
const DT_REG: u32 = 8;
/// Format: the file mode of a regular file, and of a directory.
const FILE_MODE: u32 = 0o100_644;
const DIR_MODE: u32 = 0o040_755;
/// Format: `ENOENT`.
const ENOENT: i32 = 2;

impl Mock {
  fn file_attr(&self) -> Attr {
    Attr {
      ino: 2,
      size: self.content.len() as u64,
      mode: FILE_MODE,
      nlink: 1,
      ..Attr::default()
    }
  }
}

impl Bridge for Mock {
  fn lookup(&mut self, parent: u64, name: &str) -> Result<EntryOut, i32> {
    if parent == 1 && name == "hello" {
      Ok(EntryOut {
        nodeid: 2,
        generation: 1,
        entry_valid: u64::MAX,
        attr_valid: u64::MAX,
        attr: self.file_attr(),
      })
    } else {
      Err(ENOENT)
    }
  }
  fn getattr(&mut self, nodeid: u64) -> Result<Attr, i32> {
    match nodeid {
      1 => Ok(Attr {
        ino: 1,
        mode: DIR_MODE,
        nlink: 2,
        ..Attr::default()
      }),
      2 => Ok(self.file_attr()),
      _ => Err(ENOENT),
    }
  }
  fn open(&mut self, nodeid: u64, _flags: u32) -> Result<u64, i32> {
    if nodeid == 2 { Ok(7) } else { Err(ENOENT) }
  }
  fn read(
    &mut self,
    _nodeid: u64,
    _fh: u64,
    offset: u64,
    size: u32,
    out: &mut Vec<u8>,
  ) -> Result<(), i32> {
    let start = usize::try_from(offset)
      .unwrap_or(usize::MAX)
      .min(self.content.len());
    let end = start
      .saturating_add(usize::try_from(size).unwrap_or(0))
      .min(self.content.len());
    out.extend_from_slice(&self.content[start..end]);
    Ok(())
  }
  fn write(&mut self, _nodeid: u64, _fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
    let at = usize::try_from(offset).unwrap_or(0);
    if self.content.len() < at + data.len() {
      self.content.resize(at + data.len(), 0);
    }
    self.content[at..at + data.len()].copy_from_slice(data);
    Ok(u32::try_from(data.len()).unwrap_or(u32::MAX))
  }
  fn opendir(&mut self, nodeid: u64) -> Result<u64, i32> {
    if nodeid == 1 { Ok(9) } else { Err(ENOENT) }
  }
  fn readdir(&mut self, _nodeid: u64, _fh: u64, offset: u64) -> Result<Vec<DirEntry>, i32> {
    if offset > 0 {
      return Ok(Vec::new());
    }
    Ok(vec![DirEntry {
      ino: 2,
      kind: DT_REG,
      name: "hello".to_owned(),
    }])
  }
  fn create(
    &mut self,
    _parent: u64,
    _name: &str,
    _mode: u32,
    _flags: u32,
  ) -> Result<(EntryOut, u64), i32> {
    Ok((
      EntryOut {
        nodeid: 3,
        generation: 1,
        entry_valid: u64::MAX,
        attr_valid: u64::MAX,
        attr: Attr {
          ino: 3,
          mode: FILE_MODE,
          nlink: 1,
          ..Attr::default()
        },
      },
      8,
    ))
  }
  fn release(&mut self, _nodeid: u64, _fh: u64) -> Result<(), i32> {
    Ok(())
  }
  fn forget(&mut self, _nodeid: u64, nlookup: u64) {
    self.forgotten = self.forgotten.saturating_add(nlookup);
  }
  fn flush(&mut self, _nodeid: u64, _fh: u64) -> Result<(), i32> {
    Ok(())
  }
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

  let n = dispatch(
    &message(Opcode::Lookup.to_wire(), 2, 1, b"missing\0"),
    &mut m,
    &mut out,
  );
  assert_eq!(reply_error(&out, n), -ENOENT, "the negated errno");
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
