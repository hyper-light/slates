//! A-728's tests (step S2 of A-723's content-once plan): a green's content history names a file's block where it holds
//! a value whole, so a changed file's bytes are held once; earlier versions still read back exactly; a small edit
//! re-anchors the history on the new block and owns only what it overwrote (A-793); and the running totals stay equal to
//! their recount.

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
      green.content_ref_at_bytes("f", *version).as_deref(),
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

/// A-728, A-793 (the shared path's non-vacuity counters). Do: create a 64 KiB file, replace it whole, overwrite 8 of
/// its bytes, overwrite 4 more, remove it, then fold the history to the head. Expect: the create and the whole replace
/// each name the file's block and the history owns no byte after them; the first overwrite re-anchors the history on
/// its new block and cuts the replaced block to the 8 bytes it overwrote, letting the block go; the second cuts the
/// next block to its 4; every version reads back exactly; the fold keeps the head; the running totals equal their
/// recount throughout.
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
    2,
    "a re-anchor names no whole block beyond the new file's"
  );
  assert_eq!(
    green.history_reanchored(),
    (1, 1, 8),
    "re-anchored, the replaced block cut to the 8 bytes overwritten"
  );
  assert_eq!(green.history_owned_bytes(), 8);
  reads_back(&green, &[(1, &first), (2, &second), (3, &third)]);

  let fourth = overwrite(&mut green, 4, (200, b"WXYZ"), &third);
  assert_eq!(
    green.history_reanchored(),
    (2, 2, 12),
    "version 3's block cut to its 4 overwritten bytes"
  );
  assert_eq!(green.history_owned_bytes(), 12);
  reads_back(&green, &[(2, &second), (3, &third), (4, &fourth)]);

  removed_then_folded(&mut green, &fourth);
}

/// Removes the test's file as version 5 and folds the history to it. Expect the removal to keep version 4's block whole
/// (a removal keeps nothing of the value, and versions 2 to 4 read their kept runs from it), nothing re-anchored,
/// version 4 to read back, and the file to stay absent after the fold, the totals balanced.
fn removed_then_folded(green: &mut Green, fourth: &[u8]) {
  let mut unlink = increment(5, 4, "f", &[(OpKind::Unlink, 0, 0)], b"");
  unlink.doc.ops[0].src = u64::MAX;
  assert_eq!(green.submit(&unlink), Outcome::Accepted { version: 5 });
  assert_eq!(green.content_bytes("f"), None);
  assert_eq!(
    (green.history_reanchored(), green.history_owned_bytes()),
    ((2, 2, 12), 12),
    "a removal re-anchors nothing and copies nothing"
  );
  reads_back(green, &[(4, fourth)]);
  green.fold_history_before(5);
  assert_eq!(green.content_ref_at_bytes("f", 5).as_deref(), None);
  balanced(green);
}

/// A-803 (main 02:37, piece 3; w-fed-move's injection (b): the re-anchor walked every value of a changed path's history,
/// so a path's k-th change cost O(k) while a reader pinned the floor). Do: create a 16 KiB file, then overwrite 8 bytes
/// of it at a new offset in each of 300 versions, nothing folded. Expect: the last overwrite walks one value however
/// long the history is (today: every value, 300); every version reads back exactly; the history owns only the bytes the
/// overwrites replaced; the running totals equal their recount.
#[test]
fn a_length_preserving_change_walks_one_value_however_long_its_history() {
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
    let before = green.history_reanchored_values();
    let next = overwrite_long(&mut green, version, (at, &bytes), versions.last().unwrap());
    walked_last = green.history_reanchored_values() - before;
    versions.push(next);
  }
  assert_eq!(
    walked_last, 1,
    "the last overwrite walked {walked_last} values"
  );
  for (index, bytes) in versions.iter().enumerate() {
    let version = u64::try_from(index).unwrap() + 1;
    assert_eq!(
      green.content_ref_at_bytes("f", version).as_deref(),
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

/// A generated step of [`the_layered_anchor_reads_as_the_full_reanchor_through_folds_and_checkpoints`]: a kind, a path,
/// an offset share, a length and a fill byte.
type Step = (u8, u8, u16, u8, u8);

/// One twin of the oracle: a green, and whether it takes only the full re-anchor.
struct Twin {
  green: Green,
  full: bool,
}

impl Twin {
  /// `call` on the green, under the full re-anchor where this twin takes it alone.
  fn run<R>(&mut self, call: impl FnOnce(&mut Green) -> R) -> R {
    crate::history::FULL_REANCHOR_ONLY.with(|full| full.set(self.full));
    let answer = call(&mut self.green);
    crate::history::FULL_REANCHOR_ONLY.with(|full| full.set(false));
    answer
  }
}

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

  /// A-803's oracle (main 02:37, piece 3: "today's reanchor, on generated histories, through folds and checkpoints").
  /// Do: create two files, then run generated histories of overwrites, inserts and deletes of either, folds of the
  /// history and checkpoints restored, on a green taking the layered anchor and a twin taking only today's full
  /// re-anchor. Expect: every outcome equal; after every step every file at every version the greens hold reads the
  /// same in both and as the model's; both greens' running totals equal their recount.
  #[test]
  fn the_layered_anchor_reads_as_the_full_reanchor_through_folds_and_checkpoints(
    steps in proptest::collection::vec(
      (proptest::prelude::any::<u8>(), 0u8..2, proptest::prelude::any::<u16>(), proptest::prelude::any::<u8>(), proptest::prelude::any::<u8>()),
      1..48,
    ),
  ) {
    let paths = ["f", "g"];
    let mut twins = [Twin { green: Green::new(), full: false }, Twin { green: Green::new(), full: true }];
    let mut model: Vec<Vec<Vec<u8>>> = vec![vec![Vec::new(), Vec::new()]];
    for (index, path) in paths.iter().enumerate() {
      let version = u64::try_from(index).unwrap() + 1;
      let bytes: Vec<u8> = (0..64u8).map(|n| n.wrapping_mul(7).wrapping_add(u8::try_from(index).unwrap())).collect();
      let inc = increment(u8::try_from(version).unwrap(), version - 1, path, &[(OpKind::Create, 0, 0), (OpKind::Insert, 0, 64)], &bytes);
      for twin in &mut twins {
        proptest::prop_assert_eq!(twin.run(|green| green.submit(&inc)), Outcome::Accepted { version });
      }
      let mut files = model.last().unwrap().clone();
      files[index] = bytes;
      model.push(files);
    }
    let mut floor = 0u64;
    for step in &steps {
      let head = twins[0].green.head();
      match step.0 % 8 {
        // A fold to a version at most a few behind the head.
        6 => {
          let to = head.saturating_sub(u64::from(step.2 % 4)).max(floor);
          for twin in &mut twins {
            twin.run(|green| green.fold_history_before(to));
          }
          floor = to;
        }
        // A checkpoint at the head, restored: the history starts again there.
        7 => {
          for twin in &mut twins {
            let checkpoint = twin.green.checkpoint(head).unwrap();
            twin.green = Green::restore(crate::origin::Origin::default(), &checkpoint);
          }
          floor = head;
        }
        _ => {
          let which = usize::from(step.1);
          let version = head + 1;
          let files = model.last().unwrap().clone();
          let (inc, next) = step_increment(step, paths[which], version, &files[which]);
          let outcomes: Vec<Outcome> = twins.iter_mut().map(|twin| twin.run(|green| green.submit(&inc))).collect();
          proptest::prop_assert_eq!(&outcomes[0], &outcomes[1]);
          if matches!(outcomes[0], Outcome::Accepted { .. } | Outcome::Identical { .. }) {
            let mut files = files;
            files[which] = next;
            model.push(files);
          }
        }
      }
      let head = twins[0].green.head();
      proptest::prop_assert_eq!(head, twins[1].green.head());
      for version in floor..=head {
        let expected = &model[usize::try_from(version).unwrap()];
        for (which, path) in paths.iter().enumerate() {
          let layered = twins[0].green.content_ref_at_bytes(path, version).map(|bytes| bytes.into_owned());
          let full = twins[1].green.content_ref_at_bytes(path, version).map(|bytes| bytes.into_owned());
          proptest::prop_assert_eq!(&layered, &full, "{} at {}", path, version);
          let want = (!expected[which].is_empty() || version > u64::try_from(which).unwrap())
            .then(|| expected[which].clone());
          proptest::prop_assert_eq!(&layered, &want, "{} at {} against the model", path, version);
        }
      }
      for twin in &twins {
        proptest::prop_assert_eq!(twin.green.retained_bytes().history, twin.green.history_bytes_recounted());
      }
    }
    // Non-vacuity: the layered twin walked no more values than the full one (each walks one per change it takes).
    proptest::prop_assert!(
      twins[0].green.history_reanchored_values() <= twins[1].green.history_reanchored_values()
    );
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
/// history of 5,000). Do: create a 16 KiB file, then replace 8 bytes of it in each of 300 versions as an SDK edit
/// composes it, a delete and an insert at one offset. Expect: the last replacement walks one value; every version reads
/// back; the history owns only the bytes replaced; the totals equal their recount.
#[test]
fn a_same_length_replacement_walks_one_value_however_long_its_history() {
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
    let before = green.history_reanchored_values();
    let next = replace_long(&mut green, version, (at, &bytes), versions.last().unwrap());
    walked_last = green.history_reanchored_values() - before;
    versions.push(next);
  }
  assert_eq!(
    walked_last, 1,
    "the last replacement walked {walked_last} values"
  );
  for (index, bytes) in versions.iter().enumerate() {
    let version = u64::try_from(index).unwrap() + 1;
    assert_eq!(
      green.content_ref_at_bytes("f", version).as_deref(),
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

/// A-809's leftover (a) (main 04:11, measured: a read of version 1 took 5.7 µs after 10 overwrites of one range and
/// 191 µs after 10,000). Do: create a 16 KiB file, overwrite the same 64 bytes of it in each of 1,000 versions, then read
/// version 1 whole. Expect: the read examines a layer or two at that offset, not one a version (the lowest numbered at
/// or after its epoch gives every byte there), and reads back exactly; so does every 100th version.
#[test]
fn a_read_of_an_early_version_examines_one_layer_a_range_however_often_it_was_overwritten() {
  const LEN: usize = 16 * 1024;
  const VERSIONS: u64 = 1_000;
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
  for version in 2..=VERSIONS + 1 {
    let bytes = [u8::try_from(version % 256).unwrap(); 64];
    let next = overwrite_long(
      &mut green,
      version,
      (4096, &bytes),
      versions.last().unwrap(),
    );
    versions.push(next);
  }
  let before = green.history_layers_examined();
  assert_eq!(
    green.content_ref_at_bytes("f", 1).as_deref(),
    Some(first.as_slice())
  );
  let examined = green.history_layers_examined() - before;
  assert!(
    examined <= 2,
    "reading version 1 examined {examined} layers"
  );
  for version in (1..=VERSIONS + 1).step_by(100) {
    let index = usize::try_from(version - 1).unwrap();
    assert_eq!(
      green.content_ref_at_bytes("f", version).as_deref(),
      Some(versions[index].as_slice()),
      "version {version}"
    );
  }
}

/// Asserts that every version in `range` (1-based) reads back equal to `versions[v - 1]`.
fn assert_versions_match(green: &Green, versions: &[Vec<u8>], range: impl Iterator<Item = u64>) {
  for version in range {
    let index = usize::try_from(version - 1).unwrap();
    assert_eq!(
      green.content_ref_at_bytes("f", version).as_deref(),
      Some(versions[index].as_slice()),
      "version {version}"
    );
  }
}

/// Applies one alternating overwrite or insert step on `"f"` and returns `(walked, next_bytes)`.
fn apply_alternating_step(green: &mut Green, step: u64, prev: &[u8]) -> (u64, Vec<u8>) {
  let version = step + 2;
  let at = (step * 97) % u64::try_from(prev.len() - 64).unwrap();
  let payload = [u8::try_from(version % 251).unwrap(); 64];
  let kind = if step.is_multiple_of(2) {
    OpKind::Overwrite
  } else {
    OpKind::Insert
  };
  let mut inc = increment(0, version - 1, "f", &[(kind, at, 64)], &payload);
  inc.id[..8].copy_from_slice(&version.to_le_bytes());
  let before_walked = green.history_reanchored_values();
  assert_eq!(green.submit(&inc), Outcome::Accepted { version });
  let walked = green.history_reanchored_values() - before_walked;
  let mut next = prev.to_vec();
  let pos = usize::try_from(at).unwrap();
  if kind == OpKind::Overwrite {
    next[pos..pos + 64].copy_from_slice(&payload);
  } else {
    next.splice(pos..pos, payload);
  }
  (walked, next)
}

/// A-818 piece 2b (non-vacuity for `PieceTrees` + `BytePlaces` in `history.rs`). Do: create a 16 KiB
/// file and submit 300 alternating overwrites and inserts with nothing folded, then verify every version
/// reads back byte-exact, fold partway and verify remaining versions + balanced accounting, then fold
/// to head and verify the tree collapses back cleanly. Expect: `walked_last == 1` (never 300).
#[test]
fn alternating_overwrites_and_inserts_walk_one_value_and_balance_through_folds() {
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
  let mut versions = vec![first];
  let mut walked_last = 0;
  for step in 0..VERSIONS {
    let (walked, next) = apply_alternating_step(&mut green, step, versions.last().unwrap());
    walked_last = walked;
    versions.push(next);
  }
  assert_eq!(
    walked_last, 1,
    "offset-shifting alternating edits must walk 1 value via PathTree, walked {walked_last}"
  );
  balanced(&green);
  assert_versions_match(&green, &versions, 1..=VERSIONS + 1);
  // Partial fold retires dead stored chunks while keeping versions >= 150 readable and balanced.
  green.fold_history_before(150);
  balanced(&green);
  assert_versions_match(&green, &versions, (150..=VERSIONS + 1).step_by(25));
  // Folding to head collapses the lone tree-backed latest value back to a single named block.
  let head = green.head();
  green.fold_history_before(head);
  balanced(&green);
  assert_versions_match(&green, &versions, std::iter::once(head));
  assert_eq!(green.history_owned_bytes(), 0);
}
