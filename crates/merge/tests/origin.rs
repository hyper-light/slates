//! Tests for the green's origin, the head identity, the invalidation span and the increment's
//! evidence (§4.16 "the origin version from a snapshot", "Apply on holders", "Attachments and
//! versions"; D-27; the A-9 integration requirement; AC-6.8, AC-6.13): an origin encodes to one
//! canonical byte sequence whatever order it was declared in and decodes exactly, every
//! malformation of those bytes is a typed refusal, a green seeded from an origin sits at version 0
//! and merges an increment based there on the fast path, the head identity is deterministic and
//! pinned by a golden vector, `changed_between` names every path a version in the span changed,
//! and an increment's evidence references round-trip with a wild count refused before allocation.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::Build;
use slates_merge::engine::{Green, Increment, Outcome};
use slates_merge::ops_doc::DocDecodeError;
use slates_merge::origin::Origin;

/// A fixed origin: two files, a directory, a mode, a symlink, a hard link and an attribute.
fn fixed_origin() -> Origin {
  Origin {
    files: vec![
      ("src/lib.rs".to_owned(), b"pub fn f() {}\n".to_vec()),
      ("README".to_owned(), b"hello\n".to_vec()),
    ],
    dirs: vec!["src".to_owned()],
    modes: vec![("src/lib.rs".to_owned(), 0o644)],
    symlinks: vec![("link".to_owned(), "README".to_owned())],
    hardlinks: vec![("alias".to_owned(), "README".to_owned())],
    xattrs: vec![("README".to_owned(), "user.k".to_owned(), b"v".to_vec())],
  }
}

/// The same origin with every table declared in the reverse order.
fn fixed_origin_reversed() -> Origin {
  let mut origin = fixed_origin();
  origin.files.reverse();
  origin.dirs.reverse();
  origin.modes.reverse();
  origin.symlinks.reverse();
  origin.hardlinks.reverse();
  origin.xattrs.reverse();
  origin
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Determinism gate: two declaration orders of one origin encode to identical bytes and one
/// identity, and the decode is exact (the canonical form).
#[test]
fn an_origin_encodes_canonically_and_round_trips() {
  let bytes = fixed_origin().encode();
  assert_eq!(
    bytes,
    fixed_origin_reversed().encode(),
    "declaration order does not reach the encoding"
  );
  assert_eq!(fixed_origin().identity(), fixed_origin_reversed().identity());
  let decoded = Origin::decode(&bytes).expect("decodes");
  let mut canonical = fixed_origin();
  canonical.canonicalize();
  assert_eq!(decoded, canonical, "the decode is the canonical origin");
  assert!(Origin::default().is_empty());
  assert!(!decoded.is_empty());
}

/// Hostile input: every truncation, a trailing byte, a foreign magic and a wild count are typed
/// refusals, never a panic and never an allocation the bytes cannot fill.
#[test]
fn a_malformed_origin_is_refused_typed_at_every_cut() {
  let bytes = fixed_origin().encode();
  for cut in 0..bytes.len() {
    assert!(
      Origin::decode(&bytes[..cut]).is_err(),
      "cut to {cut} bytes must refuse"
    );
  }
  let mut padded = bytes.clone();
  padded.push(0);
  assert_eq!(
    Origin::decode(&padded),
    Err(DocDecodeError::TrailingBytes),
    "a trailing byte is refused"
  );
  let mut foreign = bytes.clone();
  foreign[0] ^= 0xff;
  assert_eq!(Origin::decode(&foreign), Err(DocDecodeError::BadMagic));
  // The first table's count sits right after the magic and version: claim more entries than the
  // bytes could hold at the smallest entry size.
  let mut wild = bytes.clone();
  wild[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
  assert_eq!(
    Origin::decode(&wild),
    Err(DocDecodeError::Truncated),
    "a wild count is refused before any allocation"
  );
}

/// A green seeded from an origin is at version 0 holding the origin's state; an increment based on
/// version 0 that edits an origin file merges on the fast path (last changed at 0 ≤ base 0) and the
/// origin reconstructs as `base_at(0)` after the head moved.
#[test]
fn a_green_seeded_from_an_origin_is_at_version_zero_and_merges_on_the_fast_path() {
  let mut green = Green::with_origin(&fixed_origin());
  assert_eq!(green.head(), 0, "the origin is not a delta");
  assert_eq!(green.content("README"), Some(b"hello\n".as_slice()));
  assert!(green.is_dir("src"));
  assert_eq!(green.mode("src/lib.rs"), Some(0o644));
  assert_eq!(green.symlink("link"), Some("README"));
  assert_eq!(green.hardlink("alias"), Some("README"));
  assert_eq!(green.xattr("README", "user.k"), Some(b"v".as_slice()));
  assert!(green.changed_since(0).is_empty(), "nothing changed after 0");

  let before = green.fast_path_hits();
  let outcome = green.submit(&Build::new().overwrite("README", 0, b"HELLO").at(1, 0));
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  assert_eq!(
    green.fast_path_hits(),
    before + 1,
    "an origin file untouched since version 0 takes the fast path"
  );
  assert_eq!(green.content("README"), Some(b"HELLO\n".as_slice()));
  let base = green.base_at(0);
  assert!(
    base.files.contains(&("README".to_owned(), 6)),
    "the origin reconstructs at version 0: {:?}",
    base.files
  );
  assert_eq!(
    green.content_at("README", 0),
    Some(b"hello\n".to_vec()),
    "a version-0 read serves the origin bytes"
  );
}

/// The head identity is a pure function of the state — two greens that applied the same increments
/// to the same origin agree, a differing byte anywhere disagrees, and each version has its own —
/// and the identity of the fixed origin is pinned by a golden vector so a change to the canonical
/// encoding is caught across versions, not only within a run.
#[test]
fn the_head_identity_is_deterministic_and_matches_its_golden_vector() {
  let origin_identity = Green::with_origin(&fixed_origin()).head_identity();
  assert_eq!(
    hex(&origin_identity),
    "1fe971c080517dbe400bfbacca80db3df1fc451d8b4c934a9e947c7ed2ab50d8",
    "the head identity changed; regenerate the golden vector only for a deliberate format change"
  );
  assert_eq!(
    Green::with_origin(&fixed_origin_reversed()).head_identity(),
    origin_identity,
    "declaration order does not reach the identity"
  );
  let mut one = Green::with_origin(&fixed_origin());
  let mut two = Green::with_origin(&fixed_origin());
  let increment = Build::new().overwrite("README", 0, b"HELLO").at(1, 0);
  assert_eq!(one.submit(&increment), Outcome::Accepted { version: 1 });
  assert_eq!(two.submit(&increment), Outcome::Accepted { version: 1 });
  assert_eq!(one.head_identity(), two.head_identity());
  assert_ne!(
    one.head_identity(),
    origin_identity,
    "a version has its own identity"
  );
  let mut other = Green::with_origin(&fixed_origin());
  assert_eq!(
    other.submit(&Build::new().overwrite("README", 0, b"HELLo").at(1, 0)),
    Outcome::Accepted { version: 1 }
  );
  assert_ne!(
    other.head_identity(),
    one.head_identity(),
    "one differing byte changes the identity"
  );
}

/// `changed_between(from, to)` names exactly the paths some dimension changed at a version in
/// `(from, to]` — including a path changed inside the span and again after it, which the
/// last-changed index alone would miss.
#[test]
fn changed_between_names_every_path_a_version_in_the_span_changed() {
  let mut green = Green::new();
  green.submit(&Build::new().create("a", b"aa").create("b", b"bb").at(1, 0));
  green.submit(&Build::new().overwrite("a", 0, b"AA").at(2, 1));
  green.submit(&Build::new().overwrite("b", 0, b"BB").mkdir("d").at(3, 2));
  green.submit(&Build::new().overwrite("a", 0, b"xx").at(4, 3));
  assert_eq!(green.changed_between(1, 2), vec!["a".to_owned()]);
  assert_eq!(
    green.changed_between(2, 3),
    vec!["b".to_owned(), "d".to_owned()]
  );
  assert_eq!(
    green.changed_between(0, 3),
    vec!["a".to_owned(), "b".to_owned(), "d".to_owned()]
  );
  assert_eq!(
    green.changed_between(1, 3),
    vec!["a".to_owned(), "b".to_owned(), "d".to_owned()],
    "a changed at 2 and again at 4 is named for a span ending at 3"
  );
  assert!(green.changed_between(3, 3).is_empty());
  assert!(green.changed_between(4, 4).is_empty());
}

/// An increment's evidence references survive the chain encoding exactly; a wild evidence count
/// and a truncated reference are typed refusals before any allocation.
#[test]
fn an_increments_evidence_round_trips_and_a_wild_count_is_refused() {
  let mut increment = Build::new().create("f", b"bytes").at(1, 0);
  increment.evidence = vec![[7u8; 32], [9u8; 32]];
  let bytes = increment.encode();
  assert_eq!(Increment::decode(&bytes).expect("decodes"), increment);
  // The evidence count is the u64 before the two references.
  let count_at = bytes.len() - 2 * 32 - 8;
  let mut wild = bytes.clone();
  wild[count_at..count_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
  assert_eq!(
    Increment::decode(&wild),
    Err(DocDecodeError::Truncated),
    "a wild evidence count is refused before allocation"
  );
  assert!(
    Increment::decode(&bytes[..bytes.len() - 1]).is_err(),
    "a truncated reference is refused"
  );
  let mut padded = bytes;
  padded.push(0);
  assert_eq!(Increment::decode(&padded), Err(DocDecodeError::TrailingBytes));
}
