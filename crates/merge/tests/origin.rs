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
    specials: Vec::new(),
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
  assert_eq!(
    fixed_origin().identity(),
    fixed_origin_reversed().identity()
  );
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

/// A green seeded from an origin is at version 0 holding every dimension of the origin's state,
/// with nothing changed after version 0.
#[test]
fn a_green_seeded_from_an_origin_holds_it_at_version_zero() {
  let green = Green::with_origin(fixed_origin());
  assert_eq!(green.head(), 0, "the origin is not a delta");
  assert_eq!(green.content("README"), Some(b"hello\n".as_slice()));
  assert!(green.is_dir("src"));
  assert_eq!(green.mode("src/lib.rs"), Some(0o644));
  assert_eq!(green.symlink("link"), Some("README"));
  assert_eq!(green.hardlink("alias"), Some("README"));
  assert_eq!(green.xattr("README", "user.k"), Some(b"v".as_slice()));
  assert!(green.changed_since(0).is_empty(), "nothing changed after 0");
}

/// An increment based on version 0 that edits an origin file merges on the fast path (last changed
/// at 0 ≤ base 0), and the origin reconstructs as `base_at(0)` and `content_at(_, 0)` after the head
/// moved.
#[test]
fn an_origin_file_merges_on_the_fast_path_and_version_zero_reconstructs() {
  let mut green = Green::with_origin(fixed_origin());
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
  let origin_identity = Green::with_origin(fixed_origin()).head_identity();
  assert_eq!(
    hex(&origin_identity),
    // A-202 (2026-11-05): the identity became a set hash over the green's entries; before it, the BLAKE3 of the
    // tables in order, `1fe971c080517dbe400bfbacca80db3df1fc451d8b4c934a9e947c7ed2ab50d8`.
    "d6caa3225c45d46abd0a0df26a956a59479af07bcabd8f6ce1cec61f293e4146",
    "the head identity changed; regenerate the golden vector only for a deliberate format change"
  );
  assert_eq!(
    Green::with_origin(fixed_origin_reversed()).head_identity(),
    origin_identity,
    "declaration order does not reach the identity"
  );
  let mut one = Green::with_origin(fixed_origin());
  let mut two = Green::with_origin(fixed_origin());
  let increment = Build::new().overwrite("README", 0, b"HELLO").at(1, 0);
  assert_eq!(one.submit(&increment), Outcome::Accepted { version: 1 });
  assert_eq!(two.submit(&increment), Outcome::Accepted { version: 1 });
  assert_eq!(one.head_identity(), two.head_identity());
  assert_ne!(
    one.head_identity(),
    origin_identity,
    "a version has its own identity"
  );
  let mut other = Green::with_origin(fixed_origin());
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
  assert_eq!(
    Increment::decode(&padded),
    Err(DocDecodeError::TrailingBytes)
  );
}

/// §4.16, A-217 (an origin's identity taken in slices): the identity hashed a share at a time from any share size
/// equals the BLAKE3 of the encoding, and equals [`Origin::identity`]; a cursor is refused for an origin not yet
/// canonical, whose bytes as it stands are not the canonical ones. Do: hash the canonical fixed origin in shares of
/// 1, 3, 17 and 64 bytes and in one share. Expect: one identity, the encoding's BLAKE3, pinned by a golden vector so the
/// streamed writer cannot drift from the bytes a held origin's recorded identity was taken over. The decoder's grammar
/// admits one byte string per origin, so the round trip in `an_origin_encodes_canonically_and_round_trips` holds the
/// writer to the bytes every earlier encoder wrote.
#[test]
fn an_origins_identity_taken_in_shares_is_the_hash_of_its_encoding() {
  let mut canonical = fixed_origin();
  canonical.canonicalize();
  let whole = *blake3::hash(&canonical.encode()).as_bytes();
  for share in [1, 3, 17, 64, usize::MAX] {
    let mut cursor = slates_merge::origin::IdentityCursor::new(&canonical).expect("canonical");
    let mut steps = 0usize;
    let identity = loop {
      steps += 1;
      if let Some(identity) = cursor.step(&canonical, share) {
        break identity;
      }
    };
    assert_eq!(identity, whole, "shares of {share} bytes ({steps} steps)");
  }
  assert_eq!(canonical.identity(), whole);
  assert_eq!(fixed_origin().identity(), whole);
  assert_eq!(fixed_origin_reversed().identity(), whole);
  // `fixed_origin` declares its files out of order, so it is not canonical as it stands.
  assert!(slates_merge::origin::IdentityCursor::new(&fixed_origin()).is_none());
  assert_eq!(
    hex(&whole),
    "31913d4ffb4e02f7e66f52b386ba430358c93042e693b5888db20a8aac9ce042",
    "the fixed origin's identity (golden vector)"
  );
}

/// §4.16, §4.3 (a green seeded in shares): a green loaded from its origin a few bytes at a time is the green
/// [`Green::with_origin`] seeds in one call. Do: load the fixed origin in shares of 1 and 16 bytes and whole. Expect:
/// one head identity, version 0's files read back alike, and more than one step for the small shares (non-vacuity).
#[test]
fn a_green_seeded_in_shares_is_the_green_seeded_whole() {
  let mut whole = Green::with_origin(fixed_origin());
  for share in [1, 16] {
    let mut load = slates_merge::engine::OriginLoad::new(fixed_origin());
    let mut steps = 0usize;
    let mut green = loop {
      steps += 1;
      if let Some(green) = load.step(share) {
        break green;
      }
    };
    assert!(steps > 1, "shares of {share} bytes took {steps} steps");
    assert_eq!(
      green.head_identity(),
      whole.head_identity(),
      "shares of {share} bytes"
    );
    for path in ["README", "src/lib.rs"] {
      assert_eq!(
        green.content_at(path, 0),
        whole.content_at(path, 0),
        "{path}"
      );
    }
  }
}

/// Writes `origin` (canonical) through an [`slates_merge::origin::EncodeCursor`] in shares of `share` bytes: the bytes
/// and the steps it took.
fn encoded_in_shares(origin: &Origin, share: usize) -> (Vec<u8>, usize) {
  let mut cursor = slates_merge::origin::EncodeCursor::new(origin).expect("canonical");
  let mut out = Vec::new();
  let mut steps = 0usize;
  loop {
    steps += 1;
    if cursor.step(origin, share, &mut |bytes| out.extend_from_slice(bytes)) {
      return (out, steps);
    }
  }
}

/// Reads `bytes` through a [`slates_merge::origin::DecodeCursor`] in shares of `share` bytes: what it answered and
/// the steps it took.
fn decoded_in_shares(bytes: &[u8], share: usize) -> (Result<Origin, DocDecodeError>, usize) {
  let mut cursor = slates_merge::origin::DecodeCursor::new();
  let mut steps = 0usize;
  loop {
    steps += 1;
    match cursor.step(bytes, share) {
      Ok(Some(origin)) => return (Ok(origin), steps),
      Ok(None) => {}
      Err(refusal) => return (Err(refusal), steps),
    }
  }
}

/// §4.16 "Commit", §4.3 (a green's version-0 inputs written in shares). Do: write the fixed origin's canonical bytes a
/// few bytes at a time and whole. Expect: the bytes [`Origin::encode`] writes, every time, in more than one step for
/// the small shares (non-vacuity); a cursor refuses an origin that is not canonical.
#[test]
fn an_origins_encoding_written_in_shares_is_its_encoding() {
  let mut canonical = fixed_origin();
  canonical.canonicalize();
  let whole = canonical.encode();
  for share in [1, 3, 17, 64, usize::MAX] {
    let (bytes, steps) = encoded_in_shares(&canonical, share);
    assert_eq!(bytes, whole, "shares of {share} bytes ({steps} steps)");
    if share < whole.len() {
      assert!(steps > 1, "shares of {share} bytes took {steps} steps");
    }
  }
  assert!(slates_merge::origin::EncodeCursor::new(&fixed_origin()).is_none());
}

/// §4.16 "Apply on holders", §4.3 (a holder's replica of version 0 decoded in shares). Do: read the fixed origin's
/// bytes a few bytes at a time and whole. Expect: the canonical origin every time, in more than one step for the small
/// shares (non-vacuity).
#[test]
fn an_origin_decoded_in_shares_is_the_origin_decoded_whole() {
  let mut canonical = fixed_origin();
  canonical.canonicalize();
  let bytes = canonical.encode();
  for share in [1, 3, 17, 64, usize::MAX] {
    let (decoded, steps) = decoded_in_shares(&bytes, share);
    assert_eq!(
      decoded.as_ref(),
      Ok(&canonical),
      "shares of {share} bytes ({steps} steps)"
    );
    if share < bytes.len() {
      assert!(steps > 1, "shares of {share} bytes took {steps} steps");
    }
  }
}

/// Hostile input, in shares: every truncation, every single-bit flip, a trailing byte and a wild count, each read a
/// byte at a time, seven at a time and whole. Expect: every share answers exactly what [`Origin::decode`] answers, the
/// same origin or the same typed refusal (a resumed read never accepts what the whole refuses, nor the reverse).
#[test]
fn a_malformed_origin_is_refused_alike_in_every_share() {
  let mut canonical = fixed_origin();
  canonical.canonicalize();
  let bytes = canonical.encode();
  let mut hostile: Vec<Vec<u8>> = (0..bytes.len()).map(|cut| bytes[..cut].to_vec()).collect();
  for at in 0..bytes.len() {
    for bit in 0..8 {
      let mut flipped = bytes.clone();
      flipped[at] ^= 1 << bit;
      hostile.push(flipped);
    }
  }
  let mut padded = bytes.clone();
  padded.push(0);
  hostile.push(padded);
  let mut wild = bytes.clone();
  wild[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
  hostile.push(wild);
  let mut refused = 0usize;
  for input in &hostile {
    let whole = Origin::decode(input);
    refused += usize::from(whole.is_err());
    for share in [1, 7, usize::MAX] {
      assert_eq!(
        decoded_in_shares(input, share).0,
        whole,
        "shares of {share} bytes over {} hostile bytes",
        input.len()
      );
    }
  }
  assert!(
    refused > bytes.len(),
    "the hostile inputs were refused ({refused})"
  );
}

/// A generated origin's tables, each entry's key from a small alphabet so keys repeat and sort non-trivially.
fn origin_strategy() -> impl proptest::strategy::Strategy<Value = Origin> {
  use proptest::prelude::*;
  let key = || proptest::string::string_regex("[a-c]{1,3}(/[a-c]{1,2})?").unwrap();
  let bytes = || proptest::collection::vec(any::<u8>(), 0..40);
  (
    proptest::collection::vec((key(), bytes()), 0..12),
    proptest::collection::vec(key(), 0..6),
    proptest::collection::vec((key(), any::<u32>()), 0..6),
    proptest::collection::vec((key(), key()), 0..4),
    proptest::collection::vec((key(), key()), 0..4),
    proptest::collection::vec((key(), key(), bytes()), 0..4),
  )
    .prop_map(|(files, dirs, modes, symlinks, hardlinks, xattrs)| {
      let mut origin = Origin {
        files,
        dirs,
        modes,
        symlinks,
        hardlinks,
        specials: Vec::new(),
        xattrs,
      };
      origin.canonicalize();
      origin
    })
}

proptest::proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]
  /// §4.16, §4.3 (property). Do: write a generated canonical origin in generated shares, and read the bytes back in
  /// other generated shares. Expect: the shares write exactly [`Origin::encode`]'s bytes, and read back exactly the
  /// origin.
  #[test]
  fn every_origin_written_and_read_in_shares_round_trips(
    origin in origin_strategy(),
    write_share in 1usize..64,
    read_share in 1usize..64,
  ) {
    let whole = origin.encode();
    let (bytes, _) = encoded_in_shares(&origin, write_share);
    proptest::prop_assert_eq!(&bytes, &whole);
    let (decoded, _) = decoded_in_shares(&bytes, read_share);
    proptest::prop_assert_eq!(decoded, Ok(origin));
  }
}
