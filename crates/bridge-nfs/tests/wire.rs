//! Tests for the NFS bridge's wire codec (§4.6; Phase 4). XDR values round-trip with correct
//! four-byte padding and refuse hostile lengths; ONC RPC records frame and deframe, a partial
//! stream is `Incomplete` (not an error), an oversized record is refused, a call header parses, and
//! a reply builds to the exact bytes the kernel expects. All pure, every host — no socket, no mount.

use slates_bridge_nfs::rpc::{
  AcceptStatus, RpcError, parse_call, read_record, reply_bytes, write_record,
};
use slates_bridge_nfs::xdr::{XdrError, XdrReader, XdrWriter};

/// XDR scalars round-trip.
#[test]
fn xdr_scalars_round_trip() {
  let mut w = XdrWriter::new();
  w.u32(0x1122_3344);
  w.i32(-5);
  w.u64(0x0102_0304_0506_0708);
  w.bool(true);
  w.bool(false);
  let bytes = w.into_bytes();
  let mut r = XdrReader::new(&bytes);
  assert_eq!(r.u32().unwrap(), 0x1122_3344);
  assert_eq!(r.i32().unwrap(), -5);
  assert_eq!(r.u64().unwrap(), 0x0102_0304_0506_0708);
  assert!(r.bool().unwrap());
  assert!(!r.bool().unwrap());
  assert_eq!(r.remaining(), 0);
}

/// A variable opaque is length-prefixed and padded to four bytes; the reader skips the padding.
#[test]
fn xdr_opaque_is_length_prefixed_and_padded() {
  let mut w = XdrWriter::new();
  w.opaque(b"hello"); // 4 (len) + 5 + 3 (pad) = 12
  w.u32(0x99); // must land right after the padding
  let bytes = w.into_bytes();
  assert_eq!(bytes.len(), 16);
  assert_eq!(&bytes[0..4], &5u32.to_be_bytes());
  let mut r = XdrReader::new(&bytes);
  assert_eq!(r.opaque(64).unwrap(), b"hello");
  assert_eq!(r.u32().unwrap(), 0x99, "the reader skipped the padding");
}

/// A string round-trips and non-UTF-8 is refused.
#[test]
fn xdr_string_round_trips_and_rejects_non_utf8() {
  let mut w = XdrWriter::new();
  w.opaque("readme.md".as_bytes());
  let bytes = w.into_bytes();
  assert_eq!(XdrReader::new(&bytes).string(255).unwrap(), "readme.md");

  let mut bad = XdrWriter::new();
  bad.opaque(&[0xff, 0xfe, 0xfd, 0xfc]);
  assert_eq!(
    XdrReader::new(&bad.into_bytes()).string(255),
    Err(XdrError::BadLength)
  );
}

/// A hostile opaque length is refused before allocating, and a truncated buffer is refused.
#[test]
fn xdr_refuses_hostile_and_truncated_input() {
  // A length of u32::MAX with only a few bytes present.
  let mut w = XdrWriter::new();
  w.u32(u32::MAX);
  w.fixed(b"xx");
  assert_eq!(
    XdrReader::new(&w.into_bytes()).opaque(64),
    Err(XdrError::BadLength)
  );
  // A length past the caller's cap.
  let mut over = XdrWriter::new();
  over.opaque(&[0u8; 40]);
  assert_eq!(
    XdrReader::new(&over.into_bytes()).opaque(8),
    Err(XdrError::BadLength)
  );
  // A truncated scalar.
  assert_eq!(XdrReader::new(&[0u8; 2]).u32(), Err(XdrError::Truncated));
}

/// A record frames and deframes, returning the body and the bytes consumed.
#[test]
fn rpc_record_round_trips() {
  let body = b"\x00\x01\x02\x03some rpc message".to_vec();
  let framed = write_record(&body);
  assert_eq!(framed.len(), body.len() + 4);
  assert_eq!(framed[0] & 0x80, 0x80, "last-fragment bit set");
  let (message, consumed) = read_record(&framed).unwrap();
  assert_eq!(message, body);
  assert_eq!(consumed, framed.len());
}

/// A stream that does not yet hold the whole record is `Incomplete`, not an error.
#[test]
fn rpc_partial_record_is_incomplete() {
  let framed = write_record(b"a whole message here");
  assert_eq!(read_record(&framed[..3]), Err(RpcError::Incomplete)); // header not even complete
  assert_eq!(read_record(&framed[..8]), Err(RpcError::Incomplete)); // body not complete
}

/// A record marker claiming more than the message cap is refused (a hostile or corrupt length).
#[test]
fn rpc_oversized_record_is_refused() {
  let marker = (0x7fff_ffffu32 | 0x8000_0000).to_be_bytes();
  assert_eq!(read_record(&marker), Err(RpcError::RecordTooLarge));
}

/// A NFSv3 NULL call header parses to its program, version and procedure.
#[test]
fn rpc_parses_a_call_header() {
  let mut w = XdrWriter::new();
  w.u32(0x1122_3344); // xid
  w.u32(0); // CALL
  w.u32(2); // rpcvers
  w.u32(100_003); // NFS program
  w.u32(3); // version 3
  w.u32(0); // NULL procedure
  w.u32(0); // cred flavor AUTH_NONE
  w.opaque(&[]); // cred body
  w.u32(0); // verf flavor AUTH_NONE
  w.opaque(&[]); // verf body
  let body = w.into_bytes();
  let (call, args) = parse_call(&body).unwrap();
  assert_eq!(call.xid, 0x1122_3344);
  assert_eq!(call.program, 100_003);
  assert_eq!(call.version, 3);
  assert_eq!(call.procedure, 0);
  assert_eq!(args.remaining(), 0, "NULL has no arguments");
}

/// A message that is not a version-2 call is refused, and garbage does not panic.
#[test]
fn rpc_refuses_a_non_call() {
  let mut w = XdrWriter::new();
  w.u32(1); // xid
  w.u32(1); // REPLY, not CALL
  let body = w.into_bytes();
  assert!(matches!(parse_call(&body), Err(RpcError::NotACall)));
  assert!(
    parse_call(&[0u8; 3]).is_err(),
    "a short body is refused, not a panic"
  );
}

/// A successful reply builds to the exact bytes the kernel expects (a golden vector).
#[test]
fn rpc_success_reply_is_a_golden_vector() {
  let results = 7u32.to_be_bytes();
  let reply = reply_bytes(0xaabb_ccdd, AcceptStatus::Success, &results);
  let expected: Vec<u8> = [
    &0xaabb_ccddu32.to_be_bytes()[..], // xid
    &1u32.to_be_bytes(),               // MSG_REPLY
    &0u32.to_be_bytes(),               // MSG_ACCEPTED
    &0u32.to_be_bytes(),               // verf flavor AUTH_NONE
    &0u32.to_be_bytes(),               // verf body length 0
    &0u32.to_be_bytes(),               // accept_stat SUCCESS
    &results,                          // the procedure results
  ]
  .concat();
  assert_eq!(reply, expected);
}

/// A program-mismatch reply carries the supported version range and no results.
#[test]
fn rpc_prog_mismatch_reply_carries_the_range() {
  let reply = reply_bytes(
    1,
    AcceptStatus::ProgMismatch { low: 3, high: 3 },
    &[9, 9, 9, 9],
  );
  let mut r = XdrReader::new(&reply);
  assert_eq!(r.u32().unwrap(), 1); // xid
  assert_eq!(r.u32().unwrap(), 1); // MSG_REPLY
  assert_eq!(r.u32().unwrap(), 0); // MSG_ACCEPTED
  assert_eq!(r.u32().unwrap(), 0); // verf flavor
  assert_eq!(r.opaque(400).unwrap(), b""); // verf body
  assert_eq!(r.u32().unwrap(), 2); // accept_stat PROG_MISMATCH
  assert_eq!(r.u32().unwrap(), 3); // low
  assert_eq!(r.u32().unwrap(), 3); // high
  assert_eq!(r.remaining(), 0, "no results on a mismatch");
}
