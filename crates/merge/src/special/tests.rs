//! A-637's tests of the special-node encoding (§4.17.2 "Trees as a volume" item 5; §4.16): a submodule's link encoded
//! after a FIFO's fields under a kind of its own, read back, and refused when its id is empty, past the longest or cut
//! short; and the earlier kinds' encodings unchanged.

use super::{ENCODED_BYTES, SpecialKind, SpecialNode};
use crate::ops_doc::DocDecodeError;
use crate::push::{LONGEST_ID, ObjectKey};

/// A node of `kind` with every owner word and time set apart, so a field out of place shows.
fn node(kind: SpecialKind) -> SpecialNode {
  SpecialNode {
    kind,
    uid: 7,
    gid: 8,
    atime: -1,
    mtime: 2,
    ctime: 3,
    btime: 4,
  }
}

/// The bytes a FIFO name's fields take after its kind byte, laid out by hand: the owner words, then the four times.
fn fields() -> Vec<u8> {
  let mut out = Vec::new();
  out.extend_from_slice(&7_u32.to_le_bytes());
  out.extend_from_slice(&8_u32.to_le_bytes());
  for time in [-1_i64, 2, 3, 4] {
    out.extend_from_slice(&time.to_le_bytes());
  }
  out
}

/// A submodule's link as laid out by hand: kind byte 4, the FIFO's fields, then `len` and `id`.
fn gitlink_bytes(len: u8, id: &[u8]) -> Vec<u8> {
  let mut out = vec![4];
  out.extend(fields());
  out.push(len);
  out.extend_from_slice(id);
  out
}

/// A-637. Do: encode a submodule's link to a commit of git's SHA-1 length and one of its SHA-256 length, and decode
/// each. Expect: kind byte 4, the FIFO's fields, the id's length in one byte and its bytes, as laid out by hand; the
/// length the encoding reports; and the same node back.
#[test]
fn a_gitlink_is_a_fifos_fields_then_its_commit_and_decodes_back() {
  for len in [20_u8, 32] {
    let id: Vec<u8> = (1..=len).collect();
    let commit = ObjectKey::new(&id).unwrap();
    let link = node(SpecialKind::Gitlink { commit });
    let wanted = gitlink_bytes(len, &id);
    assert_eq!(link.encode(), wanted, "{len} bytes");
    assert_eq!(link.encoded_len(), wanted.len(), "{len} bytes");
    assert_eq!(link.encoded_len(), ENCODED_BYTES + 1 + usize::from(len));
    assert_eq!(SpecialNode::decode(&wanted), Ok(link), "{len} bytes");
  }
}

/// A-637. Do: decode a submodule's link whose id is empty, one byte past the longest id, cut a byte short, and
/// followed by a byte. Expect: `BadKind`, `BadKind`, `Truncated` and `TrailingBytes`.
#[test]
fn a_gitlink_of_no_id_or_past_the_longest_or_cut_short_is_refused() {
  let past = u8::try_from(LONGEST_ID + 1).unwrap();
  let refused = [
    (gitlink_bytes(0, &[]), DocDecodeError::BadKind),
    (
      gitlink_bytes(past, &vec![9; usize::from(past)]),
      DocDecodeError::BadKind,
    ),
    (gitlink_bytes(20, &[9; 19]), DocDecodeError::Truncated),
    (gitlink_bytes(20, &[9; 21]), DocDecodeError::TrailingBytes),
  ];
  for (bytes, refusal) in refused {
    assert_eq!(SpecialNode::decode(&bytes), Err(refusal), "{bytes:?}");
  }
}

/// A-637 (module docs: no earlier encoding moved). Do: encode a FIFO name and a character device node. Expect: the
/// bytes laid out by hand: kind bytes 0 and 3, the device's numbers after the FIFO's fields.
#[test]
fn the_earlier_kinds_encode_as_they_did() {
  let mut fifo = vec![0];
  fifo.extend(fields());
  assert_eq!(node(SpecialKind::Fifo).encode(), fifo);
  let mut device = vec![3];
  device.extend(fields());
  device.extend_from_slice(&5_u32.to_le_bytes());
  device.extend_from_slice(&6_u32.to_le_bytes());
  let char_device = SpecialKind::CharDevice { major: 5, minor: 6 };
  assert_eq!(node(char_device).encode(), device);
}
