//! A-728's tests (step S2 of A-723's content-once plan): a green's content history names a file's block where it holds
//! a value whole, so a changed file's bytes are held once; earlier versions still read back exactly; a small edit still
//! shares runs and owns only what it added; and the running totals stay equal to their recount.

use super::{Green, Increment, Outcome};
use crate::ops_doc::{Op, OpKind, OpsDoc};

/// An increment over `base` holding `ops` on one path, whose post-state is `post`.
fn increment(id: u8, base: u64, path: &str, ops: &[(OpKind, u64, u64)], post: &[u8]) -> Increment {
  let mut doc = OpsDoc::default();
  let path = doc.paths.intern(path);
  let mut src = 0u64;
  for (kind, at, len) in ops {
    let from = if matches!(kind, OpKind::Create | OpKind::Truncate) {
      u64::MAX
    } else {
      src
    };
    doc.ops.push(Op {
      kind: *kind,
      flags: 0,
      path,
      at: *at,
      len: *len,
      src: from,
    });
    if from != u64::MAX {
      src += len;
    }
  }
  let mut identity = [0u8; 32];
  identity[0] = id;
  Increment {
    id: identity,
    base,
    doc,
    post_state: post.to_vec(),
    evidence: Vec::new(),
  }
}

/// The history's running retention against its recount.
fn balanced(green: &Green) {
  assert_eq!(
    green.retained_bytes().history,
    green.history_bytes_recounted(),
    "the running history total drifted from its recount"
  );
}

/// Version 3 of the A-728 test's file: 8 bytes overwritten at 100 over `second` (its versions 1 and 2 `first` and
/// `second`, each `len` bytes named). Expect the splice to name nothing and own its 8 bytes, every version to read back
/// exactly and the totals to equal their recount. Returns version 3's bytes.
fn overwritten_by_a_splice(green: &mut Green, first: &[u8], second: &[u8], len: u64) -> Vec<u8> {
  let edited = green.submit(&increment(
    3,
    2,
    "f",
    &[(OpKind::Overwrite, 100, 8)],
    b"ABCDEFGH",
  ));
  assert_eq!(edited, Outcome::Accepted { version: 3 });
  let mut third = second.to_vec();
  third[100..108].copy_from_slice(b"ABCDEFGH");
  assert_eq!(
    green.history_blocks_named(),
    (2, 2 * len),
    "a splice names nothing"
  );
  assert_eq!(
    green.history_owned_bytes(),
    8,
    "a splice owns what it added"
  );
  assert_eq!(green.content("f"), Some(third.as_slice()));
  assert_eq!(green.content_ref_at("f", 2).as_deref(), Some(second));
  assert_eq!(green.content_ref_at("f", 1).as_deref(), Some(first));
  balanced(green);
  third
}

/// A-728 (the shared path's non-vacuity counter). Do: create a 64 KiB file, replace it whole, overwrite 8 of its
/// bytes, then fold the history to the head. Expect: the create and the whole replace each name the file's block (the
/// counter moves by two values and 128 KiB) and the history owns no byte after them; every version reads back exactly;
/// the history retains the replaced version's 64 KiB, its block, and its one piece; the small overwrite owns its 8 bytes and names
/// nothing; the fold keeps the head readable; the running totals equal their recount throughout.
#[test]
fn a_whole_value_names_the_files_block_and_the_history_owns_no_copy() {
  const LEN: u64 = 64 * 1024;
  let first: Vec<u8> = (0..LEN).map(|n| (n % 251) as u8).collect();
  let second: Vec<u8> = (0..LEN).map(|n| (n % 241) as u8 ^ 0x5a).collect();
  let mut green = Green::new();
  let made = green.submit(&increment(
    1,
    0,
    "f",
    &[(OpKind::Create, 0, 0), (OpKind::Insert, 0, LEN)],
    &first,
  ));
  assert_eq!(made, Outcome::Accepted { version: 1 });
  let replaced = green.submit(&increment(
    2,
    1,
    "f",
    &[(OpKind::Overwrite, 0, LEN)],
    &second,
  ));
  assert_eq!(replaced, Outcome::Accepted { version: 2 });
  assert_eq!(green.history_blocks_named(), (2, 2 * LEN));
  assert_eq!(green.history_owned_bytes(), 0, "the history copied no byte");
  assert_eq!(green.content("f"), Some(second.as_slice()));
  assert_eq!(
    green.content_ref_at("f", 1).as_deref(),
    Some(first.as_slice())
  );
  assert_eq!(
    green.retained_bytes().history,
    usize::try_from(LEN).unwrap() + crate::history::PIECE_BYTES,
    "the replaced version's block and its one piece"
  );
  balanced(&green);

  let third = overwritten_by_a_splice(&mut green, &first, &second, LEN);

  green.fold_history_before(3);
  assert_eq!(green.content("f"), Some(third.as_slice()));
  assert_eq!(
    green.content_ref_at("f", 3).as_deref(),
    Some(third.as_slice())
  );
  balanced(&green);
}
