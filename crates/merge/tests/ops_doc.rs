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
