//! A-728's tests (step S2 of A-723's content-once plan) and A-818's: a green's content history names a file's block
//! where it holds a value whole, so a changed file's bytes are held once; earlier versions still read back exactly; a
//! small edit stores only what it overwrote and walks no earlier version (A-793, A-818); and the running totals stay
//! equal to their recount.

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
    named: Default::default(),
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

/// Versions 1 and 2 of the test's file, `first` created and `second` overwriting it whole. Expect the create to name the
/// file's block, the overwrite to keep that block whole as version 1's bytes (a change that removes everything stores
/// the block it superseded, copying nothing), the history to own no byte, and to retain that block and the structure
/// one change makes at most.
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
  assert_eq!(green.history_blocks_named(), (1, len));
  assert_eq!(
    green.history_changes().2,
    len,
    "the superseded block kept whole"
  );
  assert_eq!(green.history_owned_bytes(), 0, "the history copied no byte");
  let retained = green.retained_bytes().history;
  assert!(
    retained >= first.len() && retained <= crate::history::change_bound(first.len(), 1),
    "the replaced version's block and one change's structure: {retained}"
  );
  reads_back(green, &[(1, first), (2, second)]);
}

/// A-728, A-818 (the shared path's non-vacuity counters). Do: create a 64 KiB file, overwrite it whole, overwrite 8 of
/// its bytes, overwrite 4 more, remove it, then fold the history to the head. Expect: the create names the file's block
/// and the whole overwrite keeps it, the history owning no byte after them; the first small overwrite stores the 8
/// bytes it overwrote and lets the replaced block go; the second stores its 4; every version reads back exactly; the
/// fold keeps the head; the running totals equal their recount throughout.
#[test]
fn a_whole_value_names_the_files_block_and_the_history_owns_no_copy() {
  const LEN: usize = 64 * 1024;
  let first: Vec<u8> = (0..LEN).map(|n| u8::try_from(n % 251).unwrap()).collect();
  let second: Vec<u8> = (0..LEN)
    .map(|n| u8::try_from(n % 241).unwrap() ^ 0x5a)
    .collect();
  let mut green = Green::new();
  created_then_replaced_whole(&mut green, &first, &second);

  let third = overwrite(&mut green, 3, (100, b"ABCDEFGH"), &second);
  assert_eq!(
    green.history_blocks_named().0,
    1,
    "a change names no whole block"
  );
  let (changes, _, stored) = green.history_changes();
  assert_eq!(
    (changes, stored),
    (2, 64 * 1024 + 8),
    "the 8 bytes overwritten, stored"
  );
  assert_eq!(green.history_owned_bytes(), 8);
  reads_back(&green, &[(1, &first), (2, &second), (3, &third)]);

  let fourth = overwrite(&mut green, 4, (200, b"WXYZ"), &third);
  let (changes, _, stored) = green.history_changes();
  assert_eq!(
    (changes, stored),
    (3, 64 * 1024 + 12),
    "its 4 overwritten bytes"
  );
  assert_eq!(green.history_owned_bytes(), 12);
  reads_back(&green, &[(2, &second), (3, &third), (4, &fourth)]);

  removed_then_folded(&mut green, &fourth);
}

/// Removes the test's file as version 5 and folds the history to it. Expect the removal to keep version 4's block whole
/// (a removal keeps nothing of the value, and versions 2 to 4 read their kept runs from it), no change taken,
/// version 4 to read back, and the file to stay absent after the fold, the totals balanced.
fn removed_then_folded(green: &mut Green, fourth: &[u8]) {
  let mut unlink = increment(5, 4, "f", &[(OpKind::Unlink, 0, 0)], b"");
  unlink.doc.ops[0].src = u64::MAX;
  assert_eq!(green.submit(&unlink), Outcome::Accepted { version: 5 });
  assert_eq!(green.content("f"), None);
  assert_eq!(
    (green.history_changes().0, green.history_owned_bytes()),
    (3, 12),
    "a removal takes no change and copies nothing"
  );
  reads_back(green, &[(4, fourth)]);
  green.fold_history_before(5);
  assert_eq!(green.content_ref_at("f", 5).as_deref(), None);
  balanced(green);
}

/// A-803 (main 02:37, piece 3; w-fed-move's injection (b): the re-anchor walked every value of a changed path's history,
/// so a path's k-th change cost O(k) while a reader pinned the floor), A-818. Do: create a 16 KiB file, then overwrite 8
/// bytes of it at a new offset in each of 300 versions, nothing folded. Expect: no change makes more tree nodes than its
/// parts' bound, however long the history is; every version reads back exactly; the history owns only the bytes the
/// overwrites replaced; the running totals equal their recount.
#[test]
fn a_length_preserving_change_makes_a_bounded_change_however_long_its_history() {
  const LEN: usize = 16 * 1024;
  const VERSIONS: u64 = 300;
  let first: Vec<u8> = (0..LEN).map(|n| u8::try_from(n % 251).unwrap()).collect();
  let mut green = Green::new();
  let len = u64::try_from(LEN).unwrap();
  let made = green.submit(&increment(
    1,
    0,
    "f",
    &[(OpKind::Create, 0, 0), (OpKind::Insert, 0, len)],
    &first,
  ));
  assert_eq!(made, Outcome::Accepted { version: 1 });
  let mut versions = vec![first.clone()];
  let mut walked_last = 0;
  for version in 2..=VERSIONS + 1 {
    let at = (version * 50) % (len - 8);
    let bytes = [u8::try_from(version % 256).unwrap() ^ 0xa5; 8];
    let before = green.history_changes().1;
    let next = overwrite_long(&mut green, version, (at, &bytes), versions.last().unwrap());
    walked_last = walked_last.max(green.history_changes().1 - before);
    versions.push(next);
  }
  let bound = crate::piece_tree::rebuild_bound(3, len) as u64;
  assert!(
    walked_last > 0 && walked_last <= bound,
    "{walked_last} nodes a change, bound {bound}"
  );
  for (index, bytes) in versions.iter().enumerate() {
    let version = u64::try_from(index).unwrap() + 1;
    assert_eq!(
      green.content_ref_at("f", version).as_deref(),
      Some(bytes.as_slice()),
      "version {version}"
    );
  }
  assert_eq!(
    green.history_owned_bytes(),
    usize::try_from(VERSIONS).unwrap() * 8,
    "only the overwritten bytes"
  );
  balanced(&green);
}

/// [`overwrite`] for a version past 255: the increment's identity carries the version's two low bytes.
fn overwrite_long(
  green: &mut Green,
  version: u64,
  (at, bytes): (u64, &[u8]),
  previous: &[u8],
) -> Vec<u8> {
  let len = u64::try_from(bytes.len()).unwrap();
  let mut inc = increment(0, version - 1, "f", &[(OpKind::Overwrite, at, len)], bytes);
  inc.id[..8].copy_from_slice(&version.to_le_bytes());
  let outcome = green.submit(&inc);
  assert_eq!(outcome, Outcome::Accepted { version });
  let mut next = previous.to_vec();
  let from = usize::try_from(at).unwrap();
  next[from..from + bytes.len()].copy_from_slice(bytes);
  next
}

/// A generated step of [`the_history_reads_as_the_model_through_folds_and_checkpoints`]: a kind, a path,
/// an offset share, a length and a fill byte.
type Step = (u8, u8, u16, u8, u8);

/// The increment of `step` over the model's head file `bytes` at `path`, as version `version`; the file after it.
fn step_increment(step: &Step, path: &str, version: u64, bytes: &[u8]) -> (Increment, Vec<u8>) {
  let (kind, _, share, len, fill) = *step;
  let file_len = bytes.len();
  let at = if file_len == 0 {
    0
  } else {
    usize::from(share) % (file_len + 1)
  };
  let len = usize::from(len % 24);
  let mut next = bytes.to_vec();
  let post = vec![fill; len];
  if kind % 5 == 4 {
    // A same-length replacement as an SDK edit composes it: a delete and an insert at one offset.
    let len = len.min(file_len - at);
    next[at..at + len].copy_from_slice(&post[..len]);
    let ops = [
      (OpKind::Delete, at as u64, len as u64),
      (OpKind::Insert, at as u64, len as u64),
    ];
    let mut inc = increment(0, version - 1, path, &ops, &post[..len]);
    inc.doc.ops[1].src = 0;
    inc.id[..8].copy_from_slice(&version.to_le_bytes());
    return (inc, next);
  }
  let (op, post): ((OpKind, u64, u64), Vec<u8>) = match kind % 4 {
    // Overwrites most often, as agents' edits and the load harness's are.
    0 | 1 => {
      let len = len.min(file_len - at);
      next[at..at + len].copy_from_slice(&post[..len]);
      (
        (OpKind::Overwrite, at as u64, len as u64),
        post[..len].to_vec(),
      )
    }
    2 => {
      next.splice(at..at, post.iter().copied());
      ((OpKind::Insert, at as u64, len as u64), post)
    }
    _ => {
      let len = len.min(file_len - at);
      next.drain(at..at + len);
      ((OpKind::Delete, at as u64, len as u64), Vec::new())
    }
  };
  let mut inc = increment(0, version - 1, path, &[op], &post);
  inc.id[..8].copy_from_slice(&version.to_le_bytes());
  (inc, next)
}

proptest::proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::prelude::ProptestConfig::default()))]

  /// A-803's oracle at the engine (main 02:37, piece 3: "on generated histories, through folds and checkpoints"), A-818.
  /// Do: create two files, then run generated histories of overwrites, inserts, deletes and SDK replacements of either,
  /// folds of the history and checkpoints restored. Expect: after every step every file at every version the green holds
  /// reads as the model's; the running totals equal their recount; and changes were taken without naming the new block
  /// whole (the non-vacuity counter). Equality with today's re-anchor is held at the history's level
  /// (`history::tests::every_version_reads_as_todays_reanchored_history`).
  #[test]
  fn the_history_reads_as_the_model_through_folds_and_checkpoints(
    steps in proptest::collection::vec(
      (proptest::prelude::any::<u8>(), 0u8..2, proptest::prelude::any::<u16>(), proptest::prelude::any::<u8>(), proptest::prelude::any::<u8>()),
      1..48,
    ),
  ) {
    let paths = ["f", "g"];
    let mut green = Green::new();
    let mut model: Vec<Vec<Vec<u8>>> = vec![vec![Vec::new(), Vec::new()]];
    for (index, path) in paths.iter().enumerate() {
      let version = u64::try_from(index).unwrap() + 1;
      let bytes: Vec<u8> = (0..64u8).map(|n| n.wrapping_mul(7).wrapping_add(u8::try_from(index).unwrap())).collect();
      let inc = increment(u8::try_from(version).unwrap(), version - 1, path, &[(OpKind::Create, 0, 0), (OpKind::Insert, 0, 64)], &bytes);
      proptest::prop_assert_eq!(green.submit(&inc), Outcome::Accepted { version });
      let mut files = model.last().unwrap().clone();
      files[index] = bytes;
      model.push(files);
    }
    let mut floor = 0u64;
    let mut changes = 0u64;
    for step in &steps {
      let head = green.head();
      match step.0 % 8 {
        // A fold to a version at most a few behind the head.
        6 => {
          let to = head.saturating_sub(u64::from(step.2 % 4)).max(floor);
          green.fold_history_before(to);
          floor = to;
        }
        // A checkpoint at the head, restored: the history starts again there.
        7 => {
          changes += green.history_changes().0;
          let checkpoint = green.checkpoint(head).unwrap();
          green = Green::restore(crate::origin::Origin::default(), &checkpoint);
          floor = head;
        }
        _ => {
          let which = usize::from(step.1);
          let files = model.last().unwrap().clone();
          let (inc, next) = step_increment(step, paths[which], head + 1, &files[which]);
          let outcome = green.submit(&inc);
          if matches!(outcome, Outcome::Accepted { .. } | Outcome::Identical { .. }) {
            let mut files = files;
            files[which] = next;
            model.push(files);
          }
        }
      }
      let head = green.head();
      for version in floor..=head {
        let expected = &model[usize::try_from(version).unwrap()];
        for (which, path) in paths.iter().enumerate() {
          let read = green.content_ref_at(path, version).map(|bytes| bytes.into_owned());
          let want = (!expected[which].is_empty() || version > u64::try_from(which).unwrap())
            .then(|| expected[which].clone());
          proptest::prop_assert_eq!(&read, &want, "{} at {} against the model", path, version);
        }
      }
      proptest::prop_assert_eq!(green.retained_bytes().history, green.history_bytes_recounted());
    }
    let edits = steps.iter().filter(|step| step.0 % 8 < 6).count();
    proptest::prop_assert!(edits == 0 || changes + green.history_changes().0 > 0, "no change was taken");
  }
}

/// Submits a same-length replacement of `bytes` at `at`, composed as an SDK edit composes it (a delete and an insert at
/// one offset), as version `version`; the file's new bytes.
fn replace_long(
  green: &mut Green,
  version: u64,
  (at, bytes): (u64, &[u8]),
  previous: &[u8],
) -> Vec<u8> {
  let len = u64::try_from(bytes.len()).unwrap();
  let mut inc = increment(
    0,
    version - 1,
    "f",
    &[(OpKind::Delete, at, len), (OpKind::Insert, at, len)],
    bytes,
  );
  // The delete names no post-state bytes: the insert's are the increment's from its start.
  inc.doc.ops[1].src = 0;
  inc.id[..8].copy_from_slice(&version.to_le_bytes());
  assert_eq!(green.submit(&inc), Outcome::Accepted { version });
  let mut next = previous.to_vec();
  let from = usize::try_from(at).unwrap();
  next[from..from + bytes.len()].copy_from_slice(bytes);
  next
}

/// A-809's leftover (b') (main 04:11, measured: an SDK edit's same-length replacement walked 5,100 values a change at a
/// history of 5,000), A-818. Do: create a 16 KiB file, then replace 8 bytes of it in each of 300 versions as an SDK
/// edit composes it, a delete and an insert at one offset. Expect: no replacement makes more tree nodes than its parts'
/// bound; every version reads back; the history owns only the bytes replaced; the totals equal their recount.
#[test]
fn a_same_length_replacement_makes_a_bounded_change_however_long_its_history() {
  const LEN: usize = 16 * 1024;
  const VERSIONS: u64 = 300;
  let first: Vec<u8> = (0..LEN).map(|n| u8::try_from(n % 251).unwrap()).collect();
  let mut green = Green::new();
  let len = u64::try_from(LEN).unwrap();
  let made = green.submit(&increment(
    1,
    0,
    "f",
    &[(OpKind::Create, 0, 0), (OpKind::Insert, 0, len)],
    &first,
  ));
  assert_eq!(made, Outcome::Accepted { version: 1 });
  let mut versions = vec![first.clone()];
  let mut walked_last = 0;
  for version in 2..=VERSIONS + 1 {
    let at = (version * 50) % (len - 8);
    let bytes = [u8::try_from(version % 256).unwrap() ^ 0x3c; 8];
    let before = green.history_changes().1;
    let next = replace_long(&mut green, version, (at, &bytes), versions.last().unwrap());
    walked_last = walked_last.max(green.history_changes().1 - before);
    versions.push(next);
  }
  let bound = crate::piece_tree::rebuild_bound(3, len) as u64;
  assert!(
    walked_last > 0 && walked_last <= bound,
    "{walked_last} nodes a change, bound {bound}"
  );
  for (index, bytes) in versions.iter().enumerate() {
    let version = u64::try_from(index).unwrap() + 1;
    assert_eq!(
      green.content_ref_at("f", version).as_deref(),
      Some(bytes.as_slice()),
      "version {version}"
    );
  }
  assert_eq!(
    green.history_owned_bytes(),
    usize::try_from(VERSIONS).unwrap() * 8,
    "only the replaced bytes"
  );
  balanced(&green);
}
