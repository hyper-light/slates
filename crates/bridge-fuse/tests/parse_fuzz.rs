//! Deterministic hostile-input fuzz of the FUSE request and opcode-body parsers — the agent-reachable `/dev/fuse`
//! parse logic. A mounting kernel (or anything speaking the FUSE protocol over the channel) hands these parsers
//! arbitrary bytes; the memory's decoder-fuzz sweep covered the NFS/format/wire decoders but noted the FUSE
//! dispatch as "parse-then-EIO + 3 hand tests", not fuzzed. Property: ANY bytes, at ANY opcode, parse to a typed
//! `Result`/`Option` and never panic, read out of bounds, or overflow (every field is `.get()`-read and every
//! offset is `saturating`/`checked`). bridge-fuse has no proptest dev-dep, so this is a self-contained
//! xorshift — reproducible, no disk. Non-vacuity: a well-formed body of each shape parses `Ok`, so the parsers'
//! success paths are live, not short-circuited.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_fuse::request::{InHeader, ReadIn, Request, RenameIn, SetAttrIn, WriteIn, parse_name};

/// A small deterministic xorshift, so the fuzz is reproducible and allocates no RNG state on disk.
struct Rng(u64);

impl Rng {
  fn next(&mut self) -> u64 {
    self.0 ^= self.0 << 13;
    self.0 ^= self.0 >> 7;
    self.0 ^= self.0 << 17;
    self.0
  }
  fn bytes(&mut self, max: usize) -> Vec<u8> {
    let len = usize::try_from(self.next() % (max as u64 + 1)).unwrap_or(0);
    let mut out = vec![0u8; len];
    for byte in out.iter_mut() {
      *byte = (self.next() & 0xff) as u8;
    }
    out
  }
}

/// Every FUSE parser, over arbitrary bytes at an arbitrary opcode, returns a typed result and never panics.
#[test]
fn any_bytes_to_a_fuse_parser_never_panic_and_a_well_formed_one_parses() {
  // Non-vacuity: each parser accepts a well-formed body of its exact shape (so "never panics over random bytes"
  // is not vacuously true because the parser always bails at byte 0).
  assert!(ReadIn::parse(0, &[0u8; 40]).is_ok(), "a full ReadIn body parses");
  assert!(WriteIn::parse(0, &[0u8; 48]).is_ok(), "a full WriteIn body (header + no data) parses");
  assert!(SetAttrIn::parse(&[0u8; 88]).is_ok(), "a full SetAttrIn body parses");
  {
    // RenameIn: newdir (8) then the two NUL-terminated names.
    let mut body = vec![0u8; 8];
    body.extend_from_slice(b"old\0new\0");
    assert!(RenameIn::parse(0, &body, false).is_ok(), "a well-formed RenameIn parses");
  }

  let mut rng = Rng(0x1234_5678_9ABC_DEF0);
  for _ in 0..50_000 {
    let body = rng.bytes(320);
    let opcode = (self_opcode(&mut rng)) as u32;
    let flagged = rng.next() & 1 == 0;

    // The opcode-body parsers: each must return without panicking on any bytes at any opcode.
    let _ = ReadIn::parse(opcode, &body);
    let _ = WriteIn::parse(opcode, &body);
    let _ = SetAttrIn::parse(&body);
    let _ = RenameIn::parse(opcode, &body, flagged);
    let _ = parse_name(&body);

    // The message-level parsers: random bytes as a whole FUSE message (header + body).
    let _ = InHeader::parse(&body);
    let _ = Request::parse(&body);
  }
}

/// A random byte, widened — the opcode the body is parsed under (its meaning does not matter to the parsers,
/// which key only off the length, but it exercises the opcode-dependent arms of `ReadIn`/`WriteIn`/`RenameIn`).
fn self_opcode(rng: &mut Rng) -> u8 {
  (rng.next() & 0xff) as u8
}
