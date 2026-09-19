//! Independent FUSE ABI vectors (§4.6 "FUSE ABI vectors must be checked against the kernel
//! headers independently of the encoder"; AC-3.10/T-3.13; audit BUG-6). Every number here was
//! transcribed by hand from `include/uapi/linux/fuse.h` at torvalds/linux `master` on 2026-09-14
//! (`FUSE_KERNEL_VERSION 7`, `FUSE_KERNEL_MINOR_VERSION 46`) and, for the `renameat2` flags, from
//! `include/uapi/linux/fs.h` the same day — never from the crate's own constants — so a wrong
//! wire value in the codec (the `1 << 8` that once stood for writeback cache, which the header
//! names `FUSE_SPLICE_MOVE`; `FUSE_FILE_OPS` is `1 << 2`) fails here even if the crate's own
//! tests agree with themselves. Layouts are checked by building or reading bodies at the
//! header's byte offsets, independently of the sequential reader and writer the codec uses.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_fuse::abi::{
  FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, IN_HEADER_LEN, OUT_HEADER_LEN, Opcode, flags,
};
use slates_bridge_fuse::reply::{Attr, AttrOut, DirBuffer, EntryOut, OpenOut, StatfsOut, WriteOut};
use slates_bridge_fuse::request::{InHeader, ReadIn, RenameIn, SetAttrIn, WriteIn};
use slates_bridge_fuse::{EXPIRE_ONLY, Notify, delete, inval_entry, inval_inode, negotiate};

/// `enum fuse_opcode`, transcribed: every opcode slates serves, with the header's value.
const OPCODES: &[(Opcode, u32)] = &[
  (Opcode::Lookup, 1),
  (Opcode::Forget, 2),
  (Opcode::GetAttr, 3),
  (Opcode::SetAttr, 4),
  (Opcode::ReadLink, 5),
  (Opcode::SymLink, 6),
  (Opcode::MkNod, 8),
  (Opcode::MkDir, 9),
  (Opcode::Unlink, 10),
  (Opcode::RmDir, 11),
  (Opcode::Rename, 12),
  (Opcode::Link, 13),
  (Opcode::Open, 14),
  (Opcode::Read, 15),
  (Opcode::Write, 16),
  (Opcode::StatFs, 17),
  (Opcode::Release, 18),
  (Opcode::FSync, 20),
  (Opcode::Flush, 25),
  (Opcode::Init, 26),
  (Opcode::OpenDir, 27),
  (Opcode::ReadDir, 28),
  (Opcode::ReleaseDir, 29),
  (Opcode::FSyncDir, 30),
  (Opcode::Create, 35),
  (Opcode::Destroy, 38),
  (Opcode::BatchForget, 42),
  (Opcode::ReadDirPlus, 44),
  (Opcode::Rename2, 45),
];

/// `enum fuse_opcode` values slates does not serve (the header's remaining enumerators; the codec
/// must report each as unserved, never mistake one for a served opcode).
const UNSERVED_OPCODES: &[u32] = &[
  21, 22, 23, 24, // SETXATTR, GETXATTR, LISTXATTR, REMOVEXATTR
  31, 32, 33, 34, // GETLK, SETLK, SETLKW, ACCESS
  36, 37, 39, 40, 41, 43, // INTERRUPT, BMAP, IOCTL, POLL, NOTIFY_REPLY, FALLOCATE
  46, 47, 48, 49, 50, 51, 52, 53,   // LSEEK .. COPY_FILE_RANGE_64
  4096, // CUSE_INIT
];

/// The `FUSE_INIT` flags the codec names, with the header's bit for each (`FUSE_HAS_EXPIRE_ONLY`
/// is `1ULL << 35`: the fourth bit of the second word, `flags2`).
const INIT_FLAGS: &[(u64, u64)] = &[
  (flags::BIG_WRITES, 1 << 5),
  (flags::DONT_MASK, 1 << 6),
  (flags::DO_READDIRPLUS, 1 << 13),
  (flags::READDIRPLUS_AUTO, 1 << 14),
  (flags::WRITEBACK_CACHE, 1 << 16),
  (flags::PARALLEL_DIROPS, 1 << 18),
  (flags::EXPLICIT_INVAL_DATA, 1 << 25),
  (flags::INIT_EXT, 1 << 30),
  (flags::HAS_EXPIRE_ONLY, 1 << 35),
];

/// Header bits the codec must never have confused with the ones above: `FUSE_FILE_OPS`,
/// `FUSE_SPLICE_MOVE` (the bit the writeback flag once wrongly carried), `FUSE_HANDLE_KILLPRIV_V2`,
/// `FUSE_CREATE_SUPP_GROUP` (the `flags2` neighbour of `HAS_EXPIRE_ONLY`), `FUSE_PASSTHROUGH`,
/// `FUSE_OVER_IO_URING`.
const FILE_OPS: u64 = 1 << 2;
const SPLICE_MOVE: u64 = 1 << 8;
const HANDLE_KILLPRIV_V2: u64 = 1 << 28;
const CREATE_SUPP_GROUP: u64 = 1 << 34;
const PASSTHROUGH: u64 = 1 << 37;
const OVER_IO_URING: u64 = 1 << 41;

/// The `FATTR_*` bits, with the header's value.
const FATTR: &[(u32, u32)] = &[
  (SetAttrIn::FATTR_MODE, 1 << 0),
  (SetAttrIn::FATTR_UID, 1 << 1),
  (SetAttrIn::FATTR_GID, 1 << 2),
  (SetAttrIn::FATTR_SIZE, 1 << 3),
  (SetAttrIn::FATTR_ATIME, 1 << 4),
  (SetAttrIn::FATTR_MTIME, 1 << 5),
  (SetAttrIn::FATTR_FH, 1 << 6),
  (SetAttrIn::FATTR_ATIME_NOW, 1 << 7),
  (SetAttrIn::FATTR_MTIME_NOW, 1 << 8),
  (SetAttrIn::FATTR_LOCKOWNER, 1 << 9),
  (SetAttrIn::FATTR_CTIME, 1 << 10),
  (SetAttrIn::FATTR_KILL_SUIDGID, 1 << 11),
];

fn u32_at(bytes: &[u8], at: usize) -> u32 {
  u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
  u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
  bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
  bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// The ABI version the codec speaks is the header's major, at or below the header's minor.
#[test]
fn the_version_is_the_headers_major_and_a_minor_at_or_below_it() {
  assert_eq!(FUSE_KERNEL_VERSION, 7);
  assert_eq!(
    FUSE_KERNEL_MINOR_VERSION.min(46),
    FUSE_KERNEL_MINOR_VERSION,
    "the floor cannot exceed the header's minor"
  );
}

/// Every served opcode carries the header's value and round-trips through `from_wire`; every
/// unserved header opcode is reported as unserved.
#[test]
fn opcodes_carry_the_headers_values_and_the_unserved_ones_are_unserved() {
  for (opcode, value) in OPCODES {
    assert_eq!(opcode.to_wire(), *value, "{opcode:?}");
    assert_eq!(Opcode::from_wire(*value), Some(*opcode), "{value}");
  }
  for value in UNSERVED_OPCODES {
    assert_eq!(
      Opcode::from_wire(*value),
      None,
      "opcode {value} is not served"
    );
  }
}

/// Every `FUSE_INIT` flag the codec names is the header's bit, and none is a neighbour it could be
/// confused with (the writeback flag once carried `FUSE_SPLICE_MOVE`'s bit, audit BUG-6).
#[test]
fn init_flags_carry_the_headers_bits() {
  for (flag, bit) in INIT_FLAGS {
    assert_eq!(*flag, *bit, "flag {flag:#x}");
  }
  assert_ne!(flags::WRITEBACK_CACHE, SPLICE_MOVE);
  assert_ne!(flags::WRITEBACK_CACHE, FILE_OPS);
  for (named, _) in INIT_FLAGS {
    for foreign in [
      FILE_OPS,
      SPLICE_MOVE,
      HANDLE_KILLPRIV_V2,
      CREATE_SUPP_GROUP,
      PASSTHROUGH,
      OVER_IO_URING,
    ] {
      assert_ne!(*named, foreign, "a named flag is never a foreign bit");
    }
  }
}

/// A kernel that offers every bit of both flag words (`INIT_EXT` set) negotiates exactly the
/// flags slates names and nothing foreign: no unknown or privileged capability (passthrough,
/// killpriv v2, io_uring) leaks into the reply through the intersection.
#[test]
fn a_kernel_offering_every_bit_negotiates_only_the_named_flags() {
  // fuse_init_in: major, minor, max_readahead, flags, flags2, unused[11].
  let mut body = vec![0u8; 64];
  put_u32(&mut body, 0, 7);
  put_u32(&mut body, 4, 46);
  put_u32(&mut body, 8, 1 << 20);
  put_u32(&mut body, 12, u32::MAX);
  put_u32(&mut body, 16, u32::MAX);
  let n = negotiate(&body).unwrap();
  let named = flags::BIG_WRITES
    | flags::DONT_MASK
    | flags::DO_READDIRPLUS
    | flags::READDIRPLUS_AUTO
    | flags::PARALLEL_DIROPS
    | flags::EXPLICIT_INVAL_DATA
    | flags::INIT_EXT
    | flags::HAS_EXPIRE_ONLY;
  assert_eq!(n.flags & !named, 0, "no foreign bit leaks: {:#x}", n.flags);
  assert_eq!(
    n.flags & flags::WRITEBACK_CACHE,
    0,
    "writeback cache is refused however the kernel offers it: the kernel would own a regular file's \
     size and times, which a volume changed through other attachments cannot allow"
  );
  assert_eq!(
    n.flags & (flags::INIT_EXT | flags::HAS_EXPIRE_ONLY),
    flags::INIT_EXT | flags::HAS_EXPIRE_ONLY,
    "flags2 is read from byte 16 (right after flags) and INIT_EXT echoed, so a second-word \
     capability negotiates; the codec had skipped a word and read unused[0] until 2026-09-14"
  );
  for foreign in [
    HANDLE_KILLPRIV_V2,
    CREATE_SUPP_GROUP,
    PASSTHROUGH,
    OVER_IO_URING,
  ] {
    assert_eq!(n.flags & foreign, 0);
  }
  assert_eq!(n.minor, FUSE_KERNEL_MINOR_VERSION, "the lesser minor");

  // The same words with INIT_EXT clear: the second word is not read at all, so a high bit the
  // kernel did not declare cannot be negotiated — HAS_EXPIRE_ONLY included.
  put_u32(&mut body, 12, !u32::try_from(flags::INIT_EXT).unwrap());
  let n = negotiate(&body).unwrap();
  assert_eq!(n.flags >> 32, 0, "flags2 is ignored without INIT_EXT");
}

/// `fuse_init_out` is 64 bytes with the header's field offsets: flags at 12, max_write at 20,
/// time_gran at 24, flags2 at 32 (after max_pages and map_alignment), then max_stack_depth,
/// request_timeout and the unused words, all zero.
#[test]
fn the_init_reply_has_the_headers_layout() {
  let mut body = vec![0u8; 64];
  put_u32(&mut body, 0, 7);
  put_u32(&mut body, 4, 46);
  put_u32(&mut body, 8, 1 << 20);
  put_u32(&mut body, 12, u32::MAX);
  put_u32(&mut body, 16, u32::MAX);
  let reply = negotiate(&body).unwrap().to_bytes();
  assert_eq!(reply.len(), 64, "sizeof(struct fuse_init_out)");
  assert_eq!(u32_at(&reply, 0), 7);
  assert_eq!(u32_at(&reply, 4), FUSE_KERNEL_MINOR_VERSION);
  assert_eq!(
    u32_at(&reply, 8),
    256 * 1024,
    "max_readahead is the lesser of the kernel's 1 MiB and slates' chunk"
  );
  assert_eq!(
    u64::from(u32_at(&reply, 12)) & (flags::WRITEBACK_CACHE | flags::INIT_EXT),
    flags::INIT_EXT,
    "flags: the low word, INIT_EXT echoed so the kernel reads flags2, writeback cache refused"
  );
  assert_eq!(u32_at(&reply, 20), 256 * 1024, "max_write");
  assert_eq!(u32_at(&reply, 24), 1, "time_gran: one nanosecond");
  assert_eq!(
    u64::from(u32_at(&reply, 32)) << 32,
    flags::HAS_EXPIRE_ONLY,
    "flags2: HAS_EXPIRE_ONLY, the one named bit of the high word"
  );
  assert!(
    reply[36..].iter().all(|b| *b == 0),
    "max_stack_depth, request_timeout, unused"
  );
}

/// The `FATTR_*` bits and the `renameat2` flags carry the headers' values.
#[test]
fn setattr_and_rename_flags_carry_the_headers_bits() {
  for (bit, expected) in FATTR {
    assert_eq!(*bit, *expected, "FATTR {bit:#x}");
  }
  assert_eq!(RenameIn::RENAME_NOREPLACE, 1 << 0);
  assert_eq!(RenameIn::RENAME_EXCHANGE, 1 << 1);
  assert_eq!(RenameIn::RENAME_WHITEOUT, 1 << 2);
}

/// The notification codes carry the header's `enum fuse_notify_code` values.
#[test]
fn notify_codes_carry_the_headers_values() {
  assert_eq!(Notify::InvalInode.code(), 2);
  assert_eq!(Notify::InvalEntry.code(), 3);
  assert_eq!(Notify::Delete.code(), 6);
}

/// The fixed structs have the header's sizes: `fuse_in_header` 40, `fuse_out_header` 16,
/// `fuse_attr` 88, `fuse_entry_out` 128, `fuse_attr_out` 104, `fuse_open_out` 16,
/// `fuse_write_out` 8, `fuse_kstatfs` 80.
#[test]
fn the_fixed_structs_have_the_headers_sizes() {
  assert_eq!(
    [
      IN_HEADER_LEN,
      OUT_HEADER_LEN,
      Attr::LEN,
      EntryOut::LEN,
      AttrOut::LEN,
      OpenOut::LEN,
      WriteOut::LEN,
    ],
    [40, 16, 88, 128, 104, 16, 8],
    "the named sizes"
  );
  assert_eq!(
    [
      Attr::default().to_bytes().len(),
      EntryOut::default().to_bytes().len(),
      AttrOut::default().to_bytes().len(),
      OpenOut::default().to_bytes().len(),
      WriteOut::default().to_bytes().len(),
      StatfsOut::default().to_bytes().len(),
    ],
    [88, 128, 104, 16, 8, 80],
    "the encoded sizes"
  );
}

/// `fuse_in_header` fields are read from the header's offsets: len 0, opcode 4, unique 8,
/// nodeid 16, uid 24, gid 28, pid 32 (then total_extlen and padding).
#[test]
fn the_request_header_is_read_at_the_headers_offsets() {
  let mut message = vec![0u8; 40];
  put_u32(&mut message, 0, 40);
  put_u32(&mut message, 4, 4);
  put_u64(&mut message, 8, 0x1122_3344_5566_7788);
  put_u64(&mut message, 16, 99);
  put_u32(&mut message, 24, 501);
  put_u32(&mut message, 28, 20);
  put_u32(&mut message, 32, 4242);
  let header = InHeader::parse(&message).unwrap();
  assert_eq!(
    (header.len, header.opcode, header.unique, header.nodeid),
    (40, 4, 0x1122_3344_5566_7788, 99)
  );
  assert_eq!((header.uid, header.gid, header.pid), (501, 20, 4242));
}

/// `fuse_setattr_in` (88 bytes) is read at the header's offsets: valid 0, fh 8, size 16,
/// lock_owner 24, atime 32, mtime 40, ctime 48, atimensec 56, mtimensec 60, ctimensec 64, mode 68,
/// uid 76, gid 80; a body one byte short is refused.
#[test]
fn setattr_in_is_read_at_the_headers_offsets() {
  let mut body = vec![0u8; 88];
  put_u32(&mut body, 0, SetAttrIn::FATTR_HONOURED);
  put_u64(&mut body, 8, 7); // fh
  put_u64(&mut body, 16, 1234); // size
  put_u64(&mut body, 24, 0xABCD); // lock_owner
  put_u64(&mut body, 32, 10); // atime
  put_u64(&mut body, 40, 20); // mtime
  put_u64(&mut body, 48, 30); // ctime
  put_u32(&mut body, 56, 1); // atimensec
  put_u32(&mut body, 60, 2); // mtimensec
  put_u32(&mut body, 64, 3); // ctimensec
  put_u32(&mut body, 68, 0o640); // mode
  put_u32(&mut body, 72, 0xFFFF_FFFF); // unused4
  put_u32(&mut body, 76, 501); // uid
  put_u32(&mut body, 80, 20); // gid
  put_u32(&mut body, 84, 0xFFFF_FFFF); // unused5
  let s = SetAttrIn::parse(&body).unwrap();
  assert_eq!(s.valid, SetAttrIn::FATTR_HONOURED);
  assert_eq!(s.size, 1234);
  assert_eq!(s.mode, 0o640);
  assert_eq!((s.uid, s.gid), (501, 20));
  assert_eq!(s.atime, 10 * 1_000_000_000 + 1);
  assert_eq!(s.mtime, 20 * 1_000_000_000 + 2);
  assert_eq!(s.ctime, 30 * 1_000_000_000 + 3);
  assert!(
    SetAttrIn::parse(&body[..87]).is_err(),
    "one byte short is refused"
  );
}

/// `fuse_read_in` and `fuse_write_in` (40 bytes each) are read at the header's offsets: fh 0,
/// offset 8, size 16; a write's data follows the 40-byte struct.
#[test]
fn read_in_and_write_in_are_read_at_the_headers_offsets() {
  let mut body = vec![0u8; 40];
  put_u64(&mut body, 0, 7);
  put_u64(&mut body, 8, 4096);
  put_u32(&mut body, 16, 512);
  put_u32(&mut body, 20, 0xFFFF_FFFF); // read_flags / write_flags: not read
  put_u64(&mut body, 24, 0xFFFF_FFFF_FFFF_FFFF); // lock_owner: not read
  let r = ReadIn::parse(Opcode::Read.to_wire(), &body).unwrap();
  assert_eq!((r.fh, r.offset, r.size), (7, 4096, 512));

  put_u32(&mut body, 16, 3);
  body.extend_from_slice(b"abc");
  let w = WriteIn::parse(Opcode::Write.to_wire(), &body).unwrap();
  assert_eq!((w.fh, w.offset), (7, 4096));
  assert_eq!(w.data, b"abc", "the data starts at byte 40");
}

/// `fuse_rename2_in` (16 bytes) is read at the header's offsets — newdir 0, flags 8, padding 12 —
/// and the two names follow it; `fuse_rename_in` (8 bytes) has the names right after newdir.
#[test]
fn rename_bodies_are_read_at_the_headers_offsets() {
  let mut body = vec![0u8; 16];
  put_u64(&mut body, 0, 42);
  put_u32(&mut body, 8, RenameIn::RENAME_NOREPLACE);
  put_u32(&mut body, 12, 0xFFFF_FFFF); // padding: not read
  body.extend_from_slice(b"old\0new\0");
  let r = RenameIn::parse(Opcode::Rename2.to_wire(), &body, true).unwrap();
  assert_eq!(
    (r.newdir, r.flags, r.old_name, r.new_name),
    (42, 1, "old", "new")
  );

  let mut plain = vec![0u8; 8];
  put_u64(&mut plain, 0, 42);
  plain.extend_from_slice(b"old\0new\0");
  let r = RenameIn::parse(Opcode::Rename.to_wire(), &plain, false).unwrap();
  assert_eq!(
    (r.newdir, r.flags, r.old_name, r.new_name),
    (42, 0, "old", "new")
  );
}

/// `fuse_entry_out` and its `fuse_attr` are written at the header's offsets: nodeid 0, generation
/// 8, entry_valid 16, attr_valid 24, the nsec parts 32 and 36, then the attributes from 40 — ino
/// 40, size 48, blocks 56, atime 64, mtime 72, ctime 80, atimensec 88, mtimensec 92, ctimensec 96,
/// mode 100, nlink 104, uid 108, gid 112, rdev 116, blksize 120, flags 124.
#[test]
fn entry_out_is_written_at_the_headers_offsets() {
  let entry = EntryOut {
    nodeid: 5,
    generation: 9,
    entry_valid: u64::MAX,
    attr_valid: 3,
    entry_valid_nsec: 11,
    attr_valid_nsec: 22,
    attr: Attr {
      ino: 5,
      size: 100,
      blocks: 1,
      mtime: (20, 2),
      ctime: (30, 3),
      atime: (10, 1),
      mode: 0o100_644,
      nlink: 2,
      uid: 501,
      gid: 20,
      blksize: 4096,
    },
  };
  let b = entry.to_bytes();
  assert_eq!(
    [u64_at(&b, 0), u64_at(&b, 8), u64_at(&b, 16), u64_at(&b, 24)],
    [5, 9, u64::MAX, 3],
    "nodeid, generation, entry_valid, attr_valid"
  );
  assert_eq!(
    (u32_at(&b, 32), u32_at(&b, 36)),
    (11, 22),
    "entry_valid_nsec, attr_valid_nsec"
  );
  assert_fuse_attr_at(&b, 40);

  // fuse_attr_out: attr_valid 0, attr_valid_nsec 8, dummy 12, then the same fuse_attr from 16.
  let a = AttrOut {
    attr_valid: 7,
    attr_valid_nsec: 33,
    attr: entry.attr,
  }
  .to_bytes();
  assert_eq!((u64_at(&a, 0), u32_at(&a, 8), u32_at(&a, 12)), (7, 33, 0));
  assert_eq!(&a[16..], &b[40..], "the attributes follow at 16");
}

/// The `fuse_attr` written at `at` holds the attributes `entry_out_is_written_at_the_headers_offsets`
/// built, each at the header's offset from `at`.
fn assert_fuse_attr_at(b: &[u8], at: usize) {
  assert_eq!(
    [u64_at(b, at), u64_at(b, at + 8), u64_at(b, at + 16)],
    [5, 100, 1],
    "ino, size, blocks"
  );
  assert_eq!(
    [u64_at(b, at + 24), u64_at(b, at + 32), u64_at(b, at + 40)],
    [10, 20, 30],
    "atime, mtime, ctime"
  );
  assert_eq!(
    [u32_at(b, at + 48), u32_at(b, at + 52), u32_at(b, at + 56)],
    [1, 2, 3],
    "atimensec, mtimensec, ctimensec"
  );
  assert_eq!(
    [
      u32_at(b, at + 60),
      u32_at(b, at + 64),
      u32_at(b, at + 68),
      u32_at(b, at + 72)
    ],
    [0o100_644, 2, 501, 20],
    "mode, nlink, uid, gid"
  );
  assert_eq!(
    [u32_at(b, at + 76), u32_at(b, at + 80), u32_at(b, at + 84)],
    [0, 4096, 0],
    "rdev, blksize, flags"
  );
}

/// `fuse_open_out`, `fuse_write_out` and `fuse_kstatfs` are written at the header's offsets.
#[test]
fn open_write_and_statfs_replies_are_written_at_the_headers_offsets() {
  let o = OpenOut {
    fh: 77,
    open_flags: 2,
  }
  .to_bytes();
  assert_eq!((u64_at(&o, 0), u32_at(&o, 8), u32_at(&o, 12)), (77, 2, 0));
  let w = WriteOut { size: 9 }.to_bytes();
  assert_eq!((u32_at(&w, 0), u32_at(&w, 4)), (9, 0));
  let s = StatfsOut {
    blocks: 1,
    bfree: 2,
    bavail: 3,
    files: 4,
    ffree: 5,
    bsize: 4096,
    namelen: 255,
    frsize: 4096,
  }
  .to_bytes();
  assert_eq!(
    (
      u64_at(&s, 0),
      u64_at(&s, 8),
      u64_at(&s, 16),
      u64_at(&s, 24),
      u64_at(&s, 32)
    ),
    (1, 2, 3, 4, 5)
  );
  assert_eq!(
    (u32_at(&s, 40), u32_at(&s, 44), u32_at(&s, 48)),
    (4096, 255, 4096)
  );
  assert!(s[52..80].iter().all(|b| *b == 0), "padding and spare[6]");
}

/// `fuse_dirent` puts the name at byte 24 (ino 0, off 8, namelen 16, type 20) padded to 8.
#[test]
fn a_dirent_puts_the_name_at_the_headers_offset() {
  let mut dir = DirBuffer::new(1024);
  assert!(dir.push(5, 1, 8, "abc"));
  let b = dir.as_bytes();
  assert_eq!(
    (u64_at(b, 0), u64_at(b, 8), u32_at(b, 16), u32_at(b, 20)),
    (5, 1, 3, 8),
    "ino, off, namelen, type"
  );
  assert_eq!(&b[24..27], b"abc", "FUSE_NAME_OFFSET");
  assert_eq!(b.len(), 32, "FUSE_DIRENT_ALIGN(24 + 3)");
}

/// `fuse_direntplus` puts the name at 152: a 128-byte `fuse_entry_out` first, then the dirent.
#[test]
fn a_direntplus_puts_the_name_at_the_headers_offset() {
  let mut plus = DirBuffer::new(1024);
  let entry = EntryOut {
    nodeid: 5,
    ..EntryOut::default()
  };
  assert!(plus.push_plus(&entry, 1, 8, "abc"));
  let p = plus.as_bytes();
  assert_eq!(
    (u64_at(p, 0), u64_at(p, 128)),
    (5, 5),
    "the entry_out's nodeid first, then the dirent's ino at 128"
  );
  assert_eq!(&p[152..155], b"abc", "FUSE_NAME_OFFSET_DIRENTPLUS");
  assert_eq!(p.len(), 160, "FUSE_DIRENTPLUS_SIZE");
}

/// `fuse_notify_inval_inode_out` is written after the 16-byte `fuse_out_header` (a zero unique,
/// the code in `error`): ino 0, off 8, len 16.
#[test]
fn an_inode_invalidation_is_written_at_the_headers_offsets() {
  let mut out = [0u8; 128];
  let n = inval_inode(9, 4096, 8192, &mut out).unwrap();
  assert_eq!(n, 16 + 24);
  assert_eq!(
    (u32_at(&out, 0), u32_at(&out, 4), u64_at(&out, 8)),
    (40, 2, 0),
    "len, FUSE_NOTIFY_INVAL_INODE in error, a zero unique"
  );
  assert_eq!(
    (u64_at(&out, 16), u64_at(&out, 24), u64_at(&out, 32)),
    (9, 4096, 8192),
    "ino, off, len"
  );
}

/// `fuse_notify_inval_entry_out` after the header: parent 0, namelen 8, flags 12, then the
/// NUL-terminated name; `fuse_notify_delete_out`: parent 0, child 8, namelen 16, padding 20, then
/// the name.
#[test]
fn entry_and_delete_notifications_are_written_at_the_headers_offsets() {
  let mut out = [0u8; 128];
  let n = inval_entry(3, "name", 0, &mut out).unwrap();
  assert_eq!(n, 16 + 16 + 5);
  assert_eq!(
    (
      u32_at(&out, 4),
      u64_at(&out, 16),
      u32_at(&out, 24),
      u32_at(&out, 28)
    ),
    (3, 3, 4, 0),
    "FUSE_NOTIFY_INVAL_ENTRY; parent, namelen, flags"
  );
  assert_eq!(&out[32..37], b"name\0");
  // FUSE_EXPIRE_ONLY is the flags word's bit 0.
  inval_entry(3, "name", EXPIRE_ONLY, &mut out).unwrap();
  assert_eq!((EXPIRE_ONLY, u32_at(&out, 28)), (1, 1), "FUSE_EXPIRE_ONLY");

  let n = delete(3, 9, "gone", &mut out).unwrap();
  assert_eq!(n, 16 + 24 + 5);
  assert_eq!(
    (
      u32_at(&out, 4),
      u64_at(&out, 16),
      u64_at(&out, 24),
      u32_at(&out, 32)
    ),
    (6, 3, 9, 4),
    "FUSE_NOTIFY_DELETE; parent, child, namelen"
  );
  assert_eq!(&out[40..45], b"gone\0");
}
