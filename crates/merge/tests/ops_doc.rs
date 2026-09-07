//! The ops document's tests (Phase 6; §4.16 "its identity is the test"): the same declared work
//! serializes to the same bytes and the same identity regardless of the order operations were
//! declared or paths interned, and a round trip of the kinds holds. Pure, every host.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_merge::ops_doc::{Op, OpKind, OpsDoc};

/// Builds a document from (path, kind, at, len) tuples in the given order.
fn doc(entries: &[(&str, OpKind, u64, u64)]) -> OpsDoc {
  let mut d = OpsDoc::new();
  for &(path, kind, at, len) in entries {
    let idx = d.paths.intern(path);
    d.ops.push(Op {
      kind,
      flags: 0,
      path: idx,
      at,
      len,
      src: u64::MAX,
    });
  }
  d.canonicalize();
  d
}

/// The same operations declared in different orders produce the same canonical bytes and the
/// same identity (the determinism gate, §4.16).
#[test]
fn the_identity_is_independent_of_declaration_order() {
  let a = doc(&[
    ("src/lib.rs", OpKind::Overwrite, 100, 20),
    ("src/lib.rs", OpKind::Overwrite, 0, 10),
    ("build.rs", OpKind::Create, 0, 0),
  ]);
  let b = doc(&[
    ("build.rs", OpKind::Create, 0, 0),
    ("src/lib.rs", OpKind::Overwrite, 0, 10),
    ("src/lib.rs", OpKind::Overwrite, 100, 20),
  ]);
  assert_eq!(a.encode(), b.encode(), "same work, same bytes");
  assert_eq!(a.identity(), b.identity(), "same work, same identity");
}

/// Encoding is deterministic: the same document encodes to the same bytes every time, and the
/// identity is its BLAKE3.
#[test]
fn encoding_is_stable_and_the_identity_is_its_hash() {
  let d = doc(&[("a", OpKind::Insert, 5, 3), ("a", OpKind::Delete, 20, 4)]);
  assert_eq!(d.encode(), d.encode());
  assert_eq!(d.identity(), *blake3::hash(&d.encode()).as_bytes());
}

/// Different work produces a different identity (the hash separates increments).
#[test]
fn different_work_has_a_different_identity() {
  let a = doc(&[("a", OpKind::Overwrite, 0, 10)]);
  let b = doc(&[("a", OpKind::Overwrite, 0, 11)]);
  assert_ne!(a.identity(), b.identity());
}

/// Every op kind round-trips through its wire value.
#[test]
fn op_kinds_round_trip() {
  for value in 0u8..=14 {
    let kind = OpKind::from_wire(value).unwrap();
    assert_eq!(kind.to_wire(), value);
  }
  assert!(OpKind::from_wire(15).is_none());
}

/// Interning is stable and deduplicated during building; canonicalize then sorts the table so
/// the encoded order is canonical and the ops' path indices are remapped to match.
#[test]
fn interning_is_stable_and_canonicalize_sorts() {
  let mut d = OpsDoc::new();
  let z = d.paths.intern("z");
  let a = d.paths.intern("a");
  assert_eq!(d.paths.intern("z"), z, "interning is idempotent");
  assert_ne!(a, z);
  d.ops.push(Op {
    kind: OpKind::Create,
    flags: 0,
    path: z,
    at: 0,
    len: 0,
    src: u64::MAX,
  });
  d.ops.push(Op {
    kind: OpKind::Create,
    flags: 0,
    path: a,
    at: 0,
    len: 0,
    src: u64::MAX,
  });
  d.canonicalize();
  assert_eq!(d.paths.paths(), &["a".to_owned(), "z".to_owned()]);
  assert_eq!(d.paths.path(d.ops[0].path), Some("a"));
  assert_eq!(d.paths.path(d.ops[1].path), Some("z"));
}

/// Golden vector (§4.16 "its identity is the test"): a fixed ops document hashes to a pinned
/// identity, so any change to the canonical encoding is caught across versions, not only within a
/// single run. If the format changes deliberately, regenerate this value (the encoding is what an
/// increment's identity is built on, so a silent change here would silently change every id).
#[test]
fn the_ops_document_identity_matches_its_golden_vector() {
  let d = doc(&[
    ("build.rs", OpKind::Create, 0, 0),
    ("src/lib.rs", OpKind::Overwrite, 0, 10),
    ("src/lib.rs", OpKind::Insert, 100, 5),
  ]);
  let hex: String = d.identity().iter().map(|b| format!("{b:02x}")).collect();
  assert_eq!(
    hex, "67613f8e50666f0237f6bef56196280d8e270d1fda3465ccc44f3b88f19b677d",
    "the ops document identity changed; regenerate the golden vector only for a deliberate format change"
  );
}

/// A document round-trips through encode/decode exactly: every kind, several paths, content and
/// namespace and xattr operations (§4.16, §4.8 — the chain is replayed from these bytes on recovery).
#[test]
fn the_document_round_trips_through_encode_and_decode() {
  let mut d = OpsDoc::new();
  for &(path, name, kind, at, len, src) in &[
    ("src/lib.rs", "", OpKind::Overwrite, 10u64, 4u64, 0u64),
    ("src/lib.rs", "", OpKind::Insert, 20, 3, 4),
    ("dir", "", OpKind::Mkdir, 0, 0, u64::MAX),
    ("old", "", OpKind::Rename, 0, 0, 1),
    ("f", "user.k", OpKind::SetXattr, 0, 2, 7),
  ] {
    let path_idx = d.paths.intern(path);
    let extra = if name.is_empty() {
      src
    } else {
      u64::from(d.paths.intern(name))
    };
    d.ops.push(Op {
      kind,
      flags: 0,
      path: path_idx,
      at,
      len,
      src: extra,
    });
  }
  d.canonicalize();
  let decoded = OpsDoc::decode(&d.encode()).expect("a valid document decodes");
  assert_eq!(decoded, d, "decode(encode(d)) == d");
}

/// The empty document round-trips.
#[test]
fn the_empty_document_round_trips() {
  let d = OpsDoc::new();
  assert_eq!(OpsDoc::decode(&d.encode()).expect("empty decodes"), d);
}

/// Hostile inputs decode to a typed refusal, never a panic (§4.8 — a torn or corrupt db entry).
#[test]
fn hostile_documents_refuse_by_type() {
  use slates_merge::ops_doc::DocDecodeError;
  let good = doc(&[("a", OpKind::Insert, 0, 1), ("b", OpKind::Overwrite, 2, 3)]).encode();

  // Empty and short inputs cannot even hold the magic.
  assert_eq!(OpsDoc::decode(&[]), Err(DocDecodeError::Truncated));
  assert_eq!(OpsDoc::decode(&[1, 2, 3]), Err(DocDecodeError::Truncated));

  // A wrong magic and a wrong version are named.
  let mut bad_magic = good.clone();
  bad_magic[0] ^= 0xff;
  assert_eq!(OpsDoc::decode(&bad_magic), Err(DocDecodeError::BadMagic));
  let mut bad_version = good.clone();
  bad_version[4] ^= 0xff;
  assert_eq!(
    OpsDoc::decode(&bad_version),
    Err(DocDecodeError::BadVersion)
  );

  // A truncated tail (drop the last op's bytes) is truncated, not a panic.
  assert_eq!(
    OpsDoc::decode(&good[..good.len() - 5]),
    Err(DocDecodeError::Truncated)
  );

  // Trailing bytes past the last op are rejected.
  let mut trailing = good.clone();
  trailing.push(0);
  assert_eq!(
    OpsDoc::decode(&trailing),
    Err(DocDecodeError::TrailingBytes)
  );

  // A wild path count (u32::MAX at bytes 8..12) cannot fit the remaining bytes: truncated, no
  // allocation of the claimed size.
  let mut wild = good.clone();
  wild[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
  assert_eq!(OpsDoc::decode(&wild), Err(DocDecodeError::Truncated));

  // A corrupt op kind byte names no kind. The op records start after the header (16 bytes) and the
  // two single-character paths (4 + 1 each); the first op's kind is the first byte there.
  let mut bad_kind = good.clone();
  let op_start = 16 + (4 + 1) + (4 + 1);
  bad_kind[op_start] = 0xff;
  assert_eq!(OpsDoc::decode(&bad_kind), Err(DocDecodeError::BadKind));
}
