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

/// A-728 (the shared path's non-vacuity counters). Do: create a 64 KiB file, replace it whole, overwrite 8 of its
/// bytes, overwrite 4 more, remove it, then fold the history to the head. Expect: the create and the whole replace each
/// name the file's block (two values, 128 KiB) and the history owns no byte after them; the history retains the replaced
/// version's 64 KiB, its block, and its one piece; the first overwrite's added runs name the new block, nothing copied;
/// the second seals it (its 8 bytes copied out, its block let go) and the removal seals the second (4 more); every
/// version reads back exactly; the fold keeps the head; the running totals equal their recount throughout.
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

  let edited = green.submit(&increment(
    3,
    2,
    "f",
    &[(OpKind::Overwrite, 100, 8)],
    b"ABCDEFGH",
  ));
  assert_eq!(edited, Outcome::Accepted { version: 3 });
  let mut third = second.clone();
  third[100..108].copy_from_slice(b"ABCDEFGH");
  assert_eq!(
    green.history_blocks_named(),
    (2, 2 * LEN),
    "a splice names no whole block"
  );
  assert_eq!(
    green.history_splices_named(),
    ((1, usize::try_from(LEN).unwrap()), (0, 0)),
    "the splice's added runs name the new block (A-728 part 2)"
  );
  assert_eq!(green.history_owned_bytes(), 0, "the splice copied nothing");
  assert_eq!(green.content("f"), Some(third.as_slice()));
  assert_eq!(
    green.content_ref_at("f", 2).as_deref(),
    Some(second.as_slice())
  );
  assert_eq!(
    green.content_ref_at("f", 1).as_deref(),
    Some(first.as_slice())
  );
  balanced(&green);

  let again = green.submit(&increment(
    4,
    3,
    "f",
    &[(OpKind::Overwrite, 200, 4)],
    b"WXYZ",
  ));
  assert_eq!(again, Outcome::Accepted { version: 4 });
  let mut fourth = third.clone();
  fourth[200..204].copy_from_slice(b"WXYZ");
  assert_eq!(
    green.history_splices_named(),
    ((2, 2 * usize::try_from(LEN).unwrap()), (1, 8)),
    "superseded, version 3 is sealed: its 8 bytes copied out, its block let go"
  );
  assert_eq!(green.history_owned_bytes(), 8);
  assert_eq!(green.content("f"), Some(fourth.as_slice()));
  assert_eq!(
    green.content_ref_at("f", 3).as_deref(),
    Some(third.as_slice())
  );
  assert_eq!(
    green.content_ref_at("f", 2).as_deref(),
    Some(second.as_slice())
  );
  balanced(&green);

  let removed = green.submit(&{
    let mut inc = increment(5, 4, "f", &[(OpKind::Unlink, 0, 0)], b"");
    inc.doc.ops[0].src = u64::MAX;
    inc
  });
  assert_eq!(removed, Outcome::Accepted { version: 5 });
  assert_eq!(green.content("f"), None);
  assert_eq!(
    green.history_splices_named().1,
    (2, 12),
    "a removal seals the value it supersedes"
  );
  assert_eq!(
    green.content_ref_at("f", 4).as_deref(),
    Some(fourth.as_slice())
  );
  balanced(&green);

  green.fold_history_before(5);
  assert_eq!(green.content("f"), None);
  assert_eq!(green.content_ref_at("f", 5).as_deref(), None);
  balanced(&green);
}
