//! The ops document's tests (Phase 6; §4.16 "its identity is the test"; A-621): the same declared work serializes to
//! the same bytes and the same identity whatever order its operations were declared or its paths interned (the
//! determinism gate), a fixed document encodes to recorded bytes and identity (the golden vectors), the encoding
//! round-trips, hostile bytes are refused by type, and a work names as many paths as it declares, each its own, at
//! a cost in comparisons a scan of the table could never match. Moved from `tests/ops_doc.rs` with the format's
//! version 2, so they run with the crate's own tests. Since version 3 (A-646) a hard link names a record of the node
//! table: its tests round-trip the table, hold it canonical whatever order nodes were named, and refuse hostile
//! records by type.

use std::cell::Cell;

use proptest::prelude::*;

use super::{DocDecodeError, NodeOrigin, NodeRef, Op, OpKind, OpsDoc, PathTable};

thread_local! {
  /// The comparisons of two names the path table's index made on this thread ([`super::Name`]'s order).
  static COMPARISONS: Cell<u64> = const { Cell::new(0) };
}

/// Counts one comparison of two names (called by [`super::Name`]'s order in test builds).
pub(super) fn count_comparison() {
  COMPARISONS.with(|count| count.set(count.get().saturating_add(1)));
}

/// The comparisons counted on this thread since the last read, the count reset.
fn comparisons() -> u64 {
  COMPARISONS.with(|count| count.replace(0))
}

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

/// Shape: the distinct names of a work past what a 16-bit index held: 65,536 indices (0 to `u16::MAX`), then two more.
const PAST_SIXTEEN_BITS: usize = (1 << 16) + 1;

/// The `count` distinct names of a large work, in a declaration order that is not their sorted order.
fn names(count: usize) -> Vec<String> {
  (0..count)
    .map(|n| format!("tree/{}/file-{n}.c", n % 97))
    .collect()
}

/// A document creating each of `names`, declared in the order of `numbers` (each a name's number), each op's `len` its
/// name's number, so a test can tell which name each op was declared for after canonicalization.
fn creating(names: &[String], numbers: impl Iterator<Item = usize>) -> OpsDoc {
  let mut d = OpsDoc::new();
  for number in numbers {
    let path = d.paths.intern(&names[number]);
    d.ops.push(Op {
      kind: OpKind::Create,
      flags: 0,
      path,
      at: 0,
      len: number as u64,
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

/// Every op kind round-trips through its wire value, `Copy` (16, appended by A-586) the last.
#[test]
fn op_kinds_round_trip() {
  for value in 0u8..=16 {
    let kind = OpKind::from_wire(value).unwrap();
    assert_eq!(kind.to_wire(), value);
  }
  assert_eq!(OpKind::from_wire(16), Some(OpKind::Copy));
  assert!(OpKind::from_wire(17).is_none());
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

/// Adds to `d` a hard link at `link` naming the node at `target` from `origin` (A-646).
fn link(d: &mut OpsDoc, link: &str, (target, origin): (&str, NodeOrigin)) {
  let link = d.paths.intern(link);
  let path = d.paths.intern(target);
  let node = d.nodes.intern(NodeRef { path, origin });
  d.ops.push(Op {
    kind: OpKind::Link,
    flags: 0,
    path: link,
    at: 0,
    len: 0,
    src: node,
  });
}

/// The golden document's bytes, laid out field by field as version 3 states them: the header (magic, version, and the
/// path, node and op counts), the three paths, the one node (the node at `src/lib.rs` at the base), the four ops.
fn golden_bytes() -> Vec<u8> {
  let mut golden = Vec::new();
  golden.extend_from_slice(b"SLOD");
  golden.extend_from_slice(&3u32.to_le_bytes());
  for count in [3u64, 1, 4] {
    golden.extend_from_slice(&count.to_le_bytes());
  }
  for path in ["build.rs", "src/lib.rs", "src/link.rs"] {
    golden.extend_from_slice(&u32::try_from(path.len()).unwrap().to_le_bytes());
    golden.extend_from_slice(path.as_bytes());
  }
  golden.push(NodeOrigin::Base.to_wire());
  golden.extend_from_slice(&1u64.to_le_bytes());
  let ops = [
    (5u8, 0u64, 0u64, 0u64, u64::MAX),
    (0, 1, 0, 10, u64::MAX),
    (3, 1, 100, 5, u64::MAX),
    (10, 2, 0, 0, 0),
  ];
  for (kind, path, at, len, src) in ops {
    golden.push(kind);
    golden.push(0);
    for word in [path, at, len, src] {
      golden.extend_from_slice(&word.to_le_bytes());
    }
  }
  golden
}

/// Golden vectors (§4.16 "its identity is the test"; version 3, A-646): a fixed ops document encodes to recorded bytes
/// and hashes to a pinned identity, so any change to the canonical encoding is caught across versions, not only within
/// a single run. The document is version 2's three operations and a hard link naming the node at `src/lib.rs` at the
/// base. Both were derived outside this crate: the bytes by the format's layout, field by field, and the identity by an
/// independent BLAKE3 (Python), which reproduces version 2's vector (`11ede55e…d370763b`) from the version 2 layout
/// and the empty input's (`af1349b9…`). If the format changes deliberately, derive both again the same way (the
/// encoding is what an increment's identity is built on, so a silent change here would silently change every id).
#[test]
fn the_ops_document_matches_its_golden_vectors() {
  let mut d = doc(&[
    ("build.rs", OpKind::Create, 0, 0),
    ("src/lib.rs", OpKind::Overwrite, 0, 10),
    ("src/lib.rs", OpKind::Insert, 100, 5),
  ]);
  link(&mut d, "src/link.rs", ("src/lib.rs", NodeOrigin::Base));
  d.canonicalize();
  let golden = golden_bytes();
  assert_eq!(golden.len(), 218, "the recorded document is 218 bytes");
  assert_eq!(d.encode(), golden, "the encoding changed");
  let hex: String = d.identity().iter().map(|b| format!("{b:02x}")).collect();
  assert_eq!(
    hex, "553a422f323766ffb303343fb617e8651c1c08ee1802e34b1258d1b11c6c0d92",
    "the ops document identity changed; derive the golden vectors again only for a deliberate format change"
  );
}

/// The links of `entries`, `(link, target, origin)`, declared in the given order, canonicalized.
fn linking(entries: &[(&str, &str, NodeOrigin)]) -> OpsDoc {
  let mut d = OpsDoc::new();
  for &(at, target, origin) in entries {
    link(&mut d, at, (target, origin));
  }
  d.canonicalize();
  d
}

/// The links of `d`, each with its node record's path and origin.
fn links_of(d: &OpsDoc) -> Vec<(&str, &str, NodeOrigin)> {
  d.ops
    .iter()
    .filter(|op| op.kind == OpKind::Link)
    .filter_map(|op| {
      let node = d.nodes.node(op.src)?;
      Some((
        d.paths.path(op.path)?,
        d.paths.path(node.path)?,
        node.origin,
      ))
    })
    .collect()
}

/// A-646. Do: build a document whose links name the node at `f` at the base twice, and the node the work makes at
/// `new`, encode and decode it. Expect the same document back, the node named twice recorded once, and each link naming
/// its node's path and origin.
#[test]
fn a_documents_node_table_round_trips_and_records_a_node_once() {
  let d = linking(&[
    ("a", "f", NodeOrigin::Base),
    ("b", "f", NodeOrigin::Base),
    ("c", "new", NodeOrigin::Made),
  ]);
  assert_eq!(
    d.nodes.nodes().len(),
    2,
    "the node named twice is one record"
  );
  let decoded = OpsDoc::decode(&d.encode()).expect("the document decodes");
  assert_eq!(decoded, d, "the node table round-trips");
  assert_eq!(
    links_of(&decoded),
    vec![
      ("a", "f", NodeOrigin::Base),
      ("b", "f", NodeOrigin::Base),
      ("c", "new", NodeOrigin::Made),
    ]
  );
}

/// A-646 (the determinism gate, with nodes). Do: build one work's links twice, declared in opposite orders, so its
/// paths and nodes are named in opposite orders too. Expect the same bytes: the node table's order is canonical, by
/// path, then origin, and each link names its own node through the remap.
#[test]
fn a_documents_node_table_is_the_same_whatever_order_its_nodes_were_named() {
  let entries = [
    ("a", "f", NodeOrigin::Base),
    ("b", "g", NodeOrigin::Made),
    ("c", "f", NodeOrigin::Made),
    ("d", "g", NodeOrigin::Base),
  ];
  let forward = linking(&entries);
  let reversed: Vec<_> = entries.iter().rev().copied().collect();
  assert_eq!(linking(&reversed).encode(), forward.encode());
  assert_eq!(links_of(&forward), entries.to_vec());
}

/// A-646. Do: decode a document of two links naming the node at `f` from either origin, with one node record made
/// hostile: an origin no build knows, a path the table does not hold, the first record twice, and a wild node count.
/// Expect each refused by type: `BadNode`, `BadNode`, `DuplicateNode`, `Truncated`.
#[test]
fn hostile_node_records_are_refused_by_type() {
  let good = linking(&[("a", "f", NodeOrigin::Base), ("b", "f", NodeOrigin::Made)]).encode();
  // The node records follow the header and the three single-character paths.
  let nodes = HEADER_BYTES + 3 * (4 + 1);
  let mut origin = good.clone();
  origin[nodes] = 2;
  assert_eq!(OpsDoc::decode(&origin), Err(DocDecodeError::BadNode));
  let mut path = good.clone();
  path[nodes + 1..nodes + 9].copy_from_slice(&3u64.to_le_bytes());
  assert_eq!(OpsDoc::decode(&path), Err(DocDecodeError::BadNode));
  let mut twice = good.clone();
  twice[nodes + 9] = good[nodes];
  assert_eq!(OpsDoc::decode(&twice), Err(DocDecodeError::DuplicateNode));
  let mut wild = good.clone();
  wild[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
  assert_eq!(OpsDoc::decode(&wild), Err(DocDecodeError::Truncated));
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
      d.paths.intern(name)
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

/// Format: the fixed header of a version 3 document: magic and version (`u32` each), path, node and op counts (`u64`
/// each; A-646 added the node count).
const HEADER_BYTES: usize = 4 + 4 + 8 + 8 + 8;

/// `good` with a wrong magic refused `BadMagic`; with a wrong version, version 1's or version 2's, `BadVersion`.
fn wrong_magic_and_versions_refuse(good: &[u8]) {
  let mut bad_magic = good.to_vec();
  bad_magic[0] ^= 0xff;
  assert_eq!(OpsDoc::decode(&bad_magic), Err(DocDecodeError::BadMagic));
  let mut bad_version = good.to_vec();
  bad_version[4] ^= 0xff;
  assert_eq!(
    OpsDoc::decode(&bad_version),
    Err(DocDecodeError::BadVersion)
  );
  for version in [1u32, 2] {
    let mut old = good.to_vec();
    old[4..8].copy_from_slice(&version.to_le_bytes());
    assert_eq!(OpsDoc::decode(&old), Err(DocDecodeError::BadVersion));
  }
}

/// Hostile inputs decode to a typed refusal, never a panic (§4.8 — a torn or corrupt db entry).
#[test]
fn hostile_documents_refuse_by_type() {
  let good = doc(&[("a", OpKind::Insert, 0, 1), ("b", OpKind::Overwrite, 2, 3)]).encode();

  // Empty and short inputs cannot even hold the magic.
  assert_eq!(OpsDoc::decode(&[]), Err(DocDecodeError::Truncated));
  assert_eq!(OpsDoc::decode(&[1, 2, 3]), Err(DocDecodeError::Truncated));

  // A wrong magic and a wrong version are named; version 1 and 2 documents among them (A-621, A-646).
  wrong_magic_and_versions_refuse(&good);

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

  // A wild path count (u64::MAX at bytes 8..16), node count (at 16..24) and op count (at 24..32) cannot fit the
  // remaining bytes: truncated, no allocation of the claimed size.
  for counts in [8..16, 16..24, 24..32] {
    let mut wild = good.clone();
    wild[counts].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(OpsDoc::decode(&wild), Err(DocDecodeError::Truncated));
  }

  // A corrupt op kind byte names no kind. The op records start after the header and the two single-character paths
  // (4 + 1 each); the first op's kind is the first byte there.
  let mut bad_kind = good.clone();
  let op_start = HEADER_BYTES + (4 + 1) + (4 + 1);
  bad_kind[op_start] = 0xff;
  assert_eq!(OpsDoc::decode(&bad_kind), Err(DocDecodeError::BadKind));
}

/// A-621. Do: decode a document whose path table names one path twice (the second single-character path made equal
/// to the first). Expect it refused `DuplicatePath`: read as the first, every later index would have named the path
/// after its own.
#[test]
fn a_path_table_naming_a_path_twice_is_refused() {
  let mut bytes = doc(&[("a", OpKind::Insert, 0, 1), ("b", OpKind::Overwrite, 2, 3)]).encode();
  let second_path = HEADER_BYTES + (4 + 1) + 4;
  bytes[second_path] = b'a';
  assert_eq!(OpsDoc::decode(&bytes), Err(DocDecodeError::DuplicatePath));
}

/// Injection: a path a merge keyed carries control bytes the tree path's `names::check` would reject,
/// because the merge service canonicalizes a path by only trimming a leading slash
/// (`merge_service::canonical_path`) — an embedded NUL, the document's own `SLOP` magic as text, and bytes
/// shaped like a four-byte length prefix. Each round-trips through the length-prefixed encoding
/// byte-exact and injects nothing: the decoded document has the same paths (none split by a control byte,
/// none merged), the same number of ops, and re-encodes to the same bytes (the identity is unchanged).
/// This is why the codec needs no name check of its own — every field is length-delimited, never a
/// separator a payload could forge (§4.16).
#[test]
fn a_path_with_control_and_framing_bytes_round_trips_without_injection() {
  let original = doc(&[
    ("a\u{0}b", OpKind::Insert, 0, 1),
    ("SLOP", OpKind::Overwrite, 2, 3),
    ("\u{4}\u{0}\u{0}\u{0}x", OpKind::Insert, 4, 5),
  ]);
  let bytes = original.encode();
  let decoded = OpsDoc::decode(&bytes).expect("a control-laden document round-trips");
  assert_eq!(
    decoded.paths.paths(),
    original.paths.paths(),
    "every path is byte-identical; no control byte split or merged one"
  );
  assert_eq!(
    decoded.ops.len(),
    3,
    "three ops in, three ops out: no entry injected or dropped"
  );
  assert_eq!(
    decoded.encode(),
    bytes,
    "the identity is unchanged by the round trip"
  );
}

/// A path whose bytes are not valid UTF-8 is refused `BadPath` at decode, never decoded into a lossy or
/// replaced string: the single path of a good document (its one byte after the header and the 4-byte length
/// prefix) set to a lone `0xFF` is a hostile wire document, refused by type, never a panic.
#[test]
fn a_path_that_is_not_utf8_is_refused_by_type() {
  let mut bytes = doc(&[("a", OpKind::Insert, 0, 1)]).encode();
  bytes[HEADER_BYTES + 4] = 0xFF;
  assert_eq!(OpsDoc::decode(&bytes), Err(DocDecodeError::BadPath));
}

/// A-621 (the failing test first: a 16-bit index answered `u16::MAX` for every name past the 65,535th). Do: declare a
/// work creating 65,537 distinct paths, intern them, canonicalize, encode and decode. Expect: every name its own index
/// while building, each index naming its own name; after canonicalization every op names the path it was declared
/// for; and the decoded document equal to the encoded one, its table as long as the names.
#[test]
fn a_work_with_more_names_than_sixteen_bits_keeps_every_one_distinct() {
  let names = names(PAST_SIXTEEN_BITS);
  let mut table = PathTable::new();
  let indices: Vec<u64> = names.iter().map(|name| table.intern(name)).collect();
  for (number, (name, index)) in names.iter().zip(&indices).enumerate() {
    assert_eq!(*index, number as u64, "{name} took another's index");
    assert_eq!(table.path(*index), Some(name.as_str()));
    assert_eq!(
      table.intern(name),
      *index,
      "interning {name} again moved it"
    );
  }
  let d = creating(&names, 0..names.len());
  assert_eq!(d.paths.paths().len(), PAST_SIXTEEN_BITS);
  for op in &d.ops {
    let declared = &names[usize::try_from(op.len).unwrap()];
    assert_eq!(
      d.paths.path(op.path),
      Some(declared.as_str()),
      "an op names another path than the one it was declared for"
    );
  }
  let decoded = OpsDoc::decode(&d.encode()).unwrap();
  assert_eq!(decoded.paths.paths().len(), PAST_SIXTEEN_BITS);
  assert_eq!(decoded, d);
}

/// A-621 (the determinism gate at that size). Do: build the 65,537-path document twice, its names declared in opposite
/// orders. Expect the same bytes and the same identity.
#[test]
fn a_document_past_sixteen_bits_is_the_same_whatever_order_its_names_were_declared() {
  let names = names(PAST_SIXTEEN_BITS);
  let forward = creating(&names, 0..names.len());
  let backward = creating(&names, (0..names.len()).rev());
  assert_eq!(forward.encode(), backward.encode(), "same work, same bytes");
  assert_eq!(forward.identity(), backward.identity());
}

/// A-621 (the interning counter: no scan). Do: intern n distinct names into a fresh table, then 2n into another,
/// counting the comparisons of two names the table makes. Expect the count to move (non-vacuity), and doubling the
/// names to less than triple the comparisons: an ordered map grows as n log n, about twice, where a scan of the table
/// (each name compared with every one before it) grows as n², four times.
#[test]
fn interning_compares_a_name_logarithmically_never_by_a_scan() {
  let intern_all = |count: usize| {
    let names = names(count);
    let mut table = PathTable::new();
    comparisons();
    for name in &names {
      table.intern(name);
    }
    comparisons()
  };
  let half = intern_all(PAST_SIXTEEN_BITS / 2);
  let whole = intern_all(PAST_SIXTEEN_BITS);
  assert!(
    half > 0,
    "interning compared no names: the counter is not counting"
  );
  assert!(
    whole < 3 * half,
    "doubling the names took {whole} comparisons against {half}: a scan, not a map"
  );
  let scanned = (PAST_SIXTEEN_BITS as u64) * (PAST_SIXTEEN_BITS as u64 - 1) / 2;
  assert!(
    whole < scanned / 64,
    "{whole} comparisons for {PAST_SIXTEEN_BITS} names, against a scan's {scanned}"
  );
}

/// A decode is a typed result and, when it succeeds, re-encoding is a fixed point: decoding those bytes and
/// re-encoding yields the same bytes, so one document identity has no second wire spelling.
fn assert_canonical_or_typed(bytes: &[u8]) {
  if let Ok(doc) = OpsDoc::decode(bytes) {
    let once = doc.encode();
    assert_eq!(
      OpsDoc::decode(&once).map(|d| d.encode()).ok(),
      Some(once),
      "re-encoding a decoded document is a fixed point"
    );
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// Hostile-input fuzz of the ops-document decoder (the merge wire format, §4.16): any byte string decodes to a
  /// typed result, never a panic (proptest fails on one), and a document that decodes is canonical. The second
  /// arm prepends a well-formed magic+version header (the first 8 bytes of a real document) so the fuzz reaches
  /// the count, path and op parsing with a hostile body, not just the magic gate — the bounds checks that keep a
  /// wild path or op count from allocating (CWE-789) are exercised with arbitrary counts.
  #[test]
  fn any_bytes_decode_to_a_typed_result_never_a_panic(body in proptest::collection::vec(any::<u8>(), 0..512)) {
    assert_canonical_or_typed(&body);
    let mut framed = doc(&[("a", OpKind::Insert, 0, 1)]).encode();
    framed.truncate(8); // keep only MAGIC + VERSION, then a hostile body
    framed.extend_from_slice(&body);
    assert_canonical_or_typed(&framed);
  }
}
