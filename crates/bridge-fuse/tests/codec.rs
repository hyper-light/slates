//! The FUSE codec's tests (Phase 3 task 1; §4.6, §4.9's hostile-input rule, Part 6's golden
//! vectors): a request parses to the right header and body, a hostile message is refused
//! without a panic, replies encode to the exact bytes the kernel expects, and `FUSE_INIT`
//! negotiates the intersection of flags. These run on every host: the codec is pure.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode, flags};
use slates_bridge_fuse::error::FuseError;
use slates_bridge_fuse::init::negotiate;
use slates_bridge_fuse::reply::{Attr, DirBuffer, EntryOut, OpenOut, ReplyHeader, WriteOut};
use slates_bridge_fuse::request::{ReadIn, Request, WriteIn, parse_name};

/// Builds a request message: the 40-byte header then the body, with `len` set to the total.
fn message(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut m = vec![0u8; total];
  m[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  m[4..8].copy_from_slice(&opcode.to_le_bytes());
  m[8..16].copy_from_slice(&unique.to_le_bytes());
  m[16..24].copy_from_slice(&nodeid.to_le_bytes());
  m[24..28].copy_from_slice(&7u32.to_le_bytes()); // uid
  m[28..32].copy_from_slice(&11u32.to_le_bytes()); // gid
  m[32..36].copy_from_slice(&99u32.to_le_bytes()); // pid
  m[IN_HEADER_LEN..].copy_from_slice(body);
  m
}

/// A LOOKUP request parses to its header, opcode and name.
#[test]
fn a_lookup_request_parses_to_its_header_and_name() {
  let msg = message(Opcode::Lookup.to_wire(), 42, 1, b"file.txt\0");
  let req = Request::parse(&msg).unwrap();
  assert_eq!(req.header.unique, 42);
  assert_eq!(req.header.nodeid, 1);
  assert_eq!(req.header.uid, 7);
  assert_eq!(req.opcode, Some(Opcode::Lookup));
  assert_eq!(parse_name(req.body).unwrap(), "file.txt");
}

/// An opcode slates does not serve parses with `opcode: None` (the caller replies ENOSYS),
/// never a panic.
#[test]
fn an_unserved_opcode_is_none_not_a_panic() {
  let msg = message(4096, 1, 1, &[]);
  let req = Request::parse(&msg).unwrap();
  assert_eq!(req.opcode, None);
}

/// Hostile headers are refused without a panic (§4.9): a truncated header, a length below the
/// header, a length past the buffer.
#[test]
fn hostile_headers_are_refused() {
  assert!(matches!(
    Request::parse(&[0u8; 8]),
    Err(FuseError::ShortHeader { .. })
  ));
  let mut short = message(Opcode::GetAttr.to_wire(), 1, 1, &[]);
  short[0..4].copy_from_slice(&8u32.to_le_bytes());
  assert!(matches!(
    Request::parse(&short),
    Err(FuseError::BadLength { .. })
  ));
  let mut huge = message(Opcode::GetAttr.to_wire(), 1, 1, &[]);
  huge[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
  assert!(matches!(
    Request::parse(&huge),
    Err(FuseError::BadLength { .. })
  ));
}

/// Hostile bodies are refused without a panic: an unterminated name, a short read body, a
/// write whose declared data runs past the body.
#[test]
fn hostile_bodies_are_refused() {
  let no_nul = message(Opcode::Lookup.to_wire(), 1, 1, b"no-terminator");
  let req = Request::parse(&no_nul).unwrap();
  assert!(matches!(
    parse_name(req.body),
    Err(FuseError::UnterminatedName)
  ));
  assert!(matches!(
    ReadIn::parse(Opcode::Read.to_wire(), &[0u8; 4]),
    Err(FuseError::ShortBody { .. })
  ));
  let mut w = vec![0u8; 40];
  w[16..20].copy_from_slice(&1000u32.to_le_bytes());
  assert!(matches!(
    WriteIn::parse(Opcode::Write.to_wire(), &w),
    Err(FuseError::ShortBody { .. })
  ));
}

/// A READ body parses to its handle, offset and size.
#[test]
fn a_read_body_parses() {
  let mut read = vec![0u8; 24];
  read[0..8].copy_from_slice(&5u64.to_le_bytes());
  read[8..16].copy_from_slice(&4096u64.to_le_bytes());
  read[16..20].copy_from_slice(&65536u32.to_le_bytes());
  let r = ReadIn::parse(Opcode::Read.to_wire(), &read).unwrap();
  assert_eq!((r.fh, r.offset, r.size), (5, 4096, 65536));
}

/// A WRITE body parses to its handle, offset and data.
#[test]
fn a_write_body_parses() {
  let mut write = vec![0u8; 40];
  write[0..8].copy_from_slice(&6u64.to_le_bytes());
  write[8..16].copy_from_slice(&10u64.to_le_bytes());
  write[16..20].copy_from_slice(&4u32.to_le_bytes());
  write.extend_from_slice(b"data");
  let w = WriteIn::parse(Opcode::Write.to_wire(), &write).unwrap();
  assert_eq!((w.fh, w.offset, w.data), (6, 10, &b"data"[..]));
}

/// An error reply is the header alone with the negated errno (golden byte check).
#[test]
fn an_error_reply_is_the_header_with_the_negated_errno() {
  let mut out = [0u8; 256];
  let n = ReplyHeader::write_error(42, 2, &mut out).unwrap(); // ENOENT
  assert_eq!(n, OUT_HEADER_LEN);
  assert_eq!(u32::from_le_bytes(out[0..4].try_into().unwrap()), 16);
  assert_eq!(out[4..8], (-2i32).to_le_bytes());
  assert_eq!(u64::from_le_bytes(out[8..16].try_into().unwrap()), 42);
}

/// A success reply is the header then the body, with `len` set to the total; an undersized
/// buffer is refused, not written out of bounds.
#[test]
fn a_success_reply_is_the_header_then_the_body() {
  let mut out = [0u8; 256];
  let entry = EntryOut {
    nodeid: 3,
    generation: 1,
    entry_valid: u64::MAX,
    attr_valid: u64::MAX,
    attr: Attr {
      ino: 3,
      size: 100,
      mode: 0o100_644,
      nlink: 1,
      ..Attr::default()
    },
  };
  let body = entry.to_bytes();
  assert_eq!(body.len(), EntryOut::LEN);
  let n = ReplyHeader::write_ok(7, &body, &mut out).unwrap();
  assert_eq!(n, OUT_HEADER_LEN + EntryOut::LEN);
  assert_eq!(
    usize::try_from(u32::from_le_bytes(out[0..4].try_into().unwrap())).unwrap(),
    n
  );
  assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 0);
  assert_eq!(u64::from_le_bytes(out[16..24].try_into().unwrap()), 3);
  let mut tiny = [0u8; 8];
  assert!(matches!(
    ReplyHeader::write_ok(1, &body, &mut tiny),
    Err(FuseError::ReplyTooSmall { .. })
  ));
}

/// The small open and write replies have their fixed wire sizes.
#[test]
fn the_small_replies_have_their_fixed_sizes() {
  assert_eq!(
    OpenOut {
      fh: 9,
      open_flags: 0
    }
    .to_bytes()
    .len(),
    OpenOut::LEN
  );
  assert_eq!(WriteOut { size: 4 }.to_bytes().len(), WriteOut::LEN);
}

/// A readdir buffer packs entries padded to 8 bytes and stops before it exceeds the request's
/// size, so the reply never overflows.
#[test]
fn readdir_packs_padded_entries_and_respects_the_size() {
  let mut dir = DirBuffer::new(64);
  assert!(dir.push(1, 1, 4, "a"));
  assert!(dir.push(2, 2, 8, "bb"));
  assert_eq!(dir.as_bytes().len() % 8, 0);
  let before = dir.as_bytes().len();
  assert!(!dir.push(3, 3, 4, &"x".repeat(40)));
  assert_eq!(dir.as_bytes().len(), before);
}

/// An INIT body: major, minor, max_readahead, flags.
fn init_body(major: u32, offered: u64) -> Vec<u8> {
  let mut body = vec![0u8; 16];
  body[0..4].copy_from_slice(&major.to_le_bytes());
  body[4..8].copy_from_slice(&40u32.to_le_bytes()); // minor above slates' floor
  body[8..12].copy_from_slice(&(1u32 << 20).to_le_bytes()); // max_readahead
  body[12..16].copy_from_slice(&u32::try_from(offered).unwrap().to_le_bytes());
  body
}

/// FUSE_INIT keeps the intersection of the flags slates wants and the kernel offers, and
/// bounds the minor version to the lesser of the two.
#[test]
fn init_negotiates_the_intersection_of_flags() {
  let offered = flags::WRITEBACK_CACHE | flags::DO_READDIRPLUS | (1u64 << 20);
  let n = negotiate(&init_body(7, offered)).unwrap();
  assert_eq!(n.major, 7);
  assert_eq!(n.minor, 31, "bounded to slates' floor");
  assert!(n.flags & flags::WRITEBACK_CACHE != 0);
  assert!(n.flags & flags::DO_READDIRPLUS != 0);
  assert_eq!(n.flags & (1u64 << 20), 0, "an unwanted flag is dropped");
  assert!(!n.version_mismatch);
  assert!(!n.to_bytes().is_empty());
}

/// An older kernel major triggers a version reply (not a hard fail); a short body is refused.
#[test]
fn init_handles_a_version_mismatch_and_a_short_body() {
  assert!(negotiate(&init_body(6, 0)).unwrap().version_mismatch);
  assert!(matches!(
    negotiate(&[0u8; 4]),
    Err(FuseError::ShortBody { .. })
  ));
}
