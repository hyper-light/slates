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

/// Submits an overwrite of `bytes` at `at` over `previous` (the head's bytes) as version `version`, expecting it
/// accepted; the file's new bytes.
fn overwrite(
  green: &mut Green,
  version: u64,
  (at, bytes): (u64, &[u8]),
  previous: &[u8],
) -> Vec<u8> {
  let len = u64::try_from(bytes.len()).unwrap();
  let id = u8::try_from(version).unwrap();
  let outcome = green.submit(&increment(
    id,
    version - 1,
    "f",
    &[(OpKind::Overwrite, at, len)],
    bytes,
  ));
  assert_eq!(outcome, Outcome::Accepted { version });
  let mut next = previous.to_vec();
  let from = usize::try_from(at).unwrap();
  next[from..from + bytes.len()].copy_from_slice(bytes);
  next
}

/// Every `(version, bytes)` reads back exactly, and the running totals equal their recount.
fn reads_back(green: &Green, versions: &[(u64, &[u8])]) {
  for (version, bytes) in versions {
    assert_eq!(
      green.content_ref_at("f", *version).as_deref(),
      Some(*bytes),
      "version {version}"
    );
  }
  balanced(green);
}

/// Versions 1 and 2 of the test's file, `first` created and `second` replacing it whole. Expect both to name the file's
/// block (two values, `2 * len` bytes), the history to own no byte, and to retain the replaced version's block and its
/// one piece.
fn created_then_replaced_whole(green: &mut Green, first: &[u8], second: &[u8]) {
  let len = u64::try_from(first.len()).unwrap();
  let made = green.submit(&increment(
    1,
    0,
    "f",
    &[(OpKind::Create, 0, 0), (OpKind::Insert, 0, len)],
    first,
  ));
  assert_eq!(made, Outcome::Accepted { version: 1 });
  overwrite(green, 2, (0, second), first);
  assert_eq!(green.history_blocks_named(), (2, 2 * len));
  assert_eq!(green.history_owned_bytes(), 0, "the history copied no byte");
  assert_eq!(
    green.retained_bytes().history,
    first.len() + crate::history::PIECE_BYTES,
    "the replaced version's block and its one piece"
  );
  reads_back(green, &[(1, first), (2, second)]);
}

/// A-728 (the shared path's non-vacuity counters). Do: create a 64 KiB file, replace it whole, overwrite 8 of its
/// bytes, overwrite 4 more, remove it, then fold the history to the head. Expect: the create and the whole replace each
/// name the file's block and the history owns no byte after them; the first overwrite's added runs name the new block,
/// nothing copied; the second seals it (its 8 bytes copied out, its block let go) and the removal seals the second (4
/// more); every version reads back exactly; the fold keeps the head; the running totals equal their recount throughout.
#[test]
fn a_whole_value_names_the_files_block_and_the_history_owns_no_copy() {
  const LEN: usize = 64 * 1024;
  let first: Vec<u8> = (0..LEN).map(|n| (n % 251) as u8).collect();
  let second: Vec<u8> = (0..LEN).map(|n| (n % 241) as u8 ^ 0x5a).collect();
  let mut green = Green::new();
  created_then_replaced_whole(&mut green, &first, &second);

  let third = overwrite(&mut green, 3, (100, b"ABCDEFGH"), &second);
  assert_eq!(
    green.history_blocks_named().0,
    2,
    "a splice names no whole block"
  );
  assert_eq!(
    green.history_splices_named(),
    ((1, LEN), (0, 0)),
    "added runs named in the new block"
  );
  assert_eq!(green.history_owned_bytes(), 0, "the splice copied nothing");
  reads_back(&green, &[(1, &first), (2, &second), (3, &third)]);

  let fourth = overwrite(&mut green, 4, (200, b"WXYZ"), &third);
  assert_eq!(
    green.history_splices_named(),
    ((2, 2 * LEN), (1, 8)),
    "version 3 sealed"
  );
  assert_eq!(green.history_owned_bytes(), 8);
  reads_back(&green, &[(2, &second), (3, &third), (4, &fourth)]);

  removed_then_folded(&mut green, &fourth);
}

/// Removes the test's file as version 5 and folds the history to it. Expect the removal to seal version 4 (4 more bytes
/// copied), version 4 to read back, and the file to stay absent after the fold, the totals balanced.
fn removed_then_folded(green: &mut Green, fourth: &[u8]) {
  let mut unlink = increment(5, 4, "f", &[(OpKind::Unlink, 0, 0)], b"");
  unlink.doc.ops[0].src = u64::MAX;
  assert_eq!(green.submit(&unlink), Outcome::Accepted { version: 5 });
  assert_eq!(green.content("f"), None);
  assert_eq!(
    green.history_splices_named().1,
    (2, 12),
    "a removal seals what it supersedes"
  );
  reads_back(green, &[(4, fourth)]);
  green.fold_history_before(5);
  assert_eq!(green.content_ref_at("f", 5).as_deref(), None);
  balanced(green);
}
