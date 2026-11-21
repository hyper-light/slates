//! The content history against today's (`crate::history_reference`, main's ruling of 2026-11-21) and against
//! `apply`; and what it retains, by the design's words: a change keeps what it removed, nothing more, and walks no
//! version.
#![allow(
  clippy::unwrap_used,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation
)]

use super::*;
use crate::dims::Files;
use crate::history_reference as reference;
use proptest::prelude::*;

/// A generated op over a file of `len` bytes: kind, offset share, length, and the post-state offset.
fn op_of(kind: u8, at: u64, len: u64, src: u64) -> Op {
  let kind = match kind % 5 {
    0 => OpKind::Overwrite,
    1 => OpKind::Insert,
    2 => OpKind::Delete,
    3 => OpKind::Extend,
    _ => OpKind::Truncate,
  };
  Op {
    kind,
    flags: 0,
    path: 0,
    at,
    len,
    src,
  }
}

/// The engine's `apply`, restated: the oracle the values are held to.
fn apply(base: &[u8], ops: &[Op], post: &[u8]) -> Vec<u8> {
  crate::engine::apply_for_tests(base, ops, post)
}

/// Ops sorted by offset (the deriver's shape) over a file of `len` bytes, from generated tuples.
fn ops_over(len: u64, raw: &[(u8, u16, u16, u16)]) -> Vec<Op> {
  let mut ops: Vec<Op> = raw
    .iter()
    .map(|(kind, at, op_len, src)| {
      let at = if len == 0 {
        0
      } else {
        u64::from(*at) % (len + 1)
      };
      op_of(*kind, at, u64::from(*op_len) % 64, u64::from(*src) % 256)
    })
    .collect();
  ops.sort_by_key(|op| op.at);
  ops
}

/// The file `f`'s first value: `bytes` set and its block named.
fn first_value(chunks: &mut Chunks, files: &mut Files, bytes: &[u8]) -> Value {
  files.insert("f", bytes.to_vec());
  let block = files.hold_block_of("f").unwrap();
  Value::named(chunks, files.blocks_mut(), block, bytes.len())
}

/// One version as the engine makes it: the file set to `next`, the latest superseded, the change taken, or the new
/// block named whole when it is no change of the latest. Whether the change was taken.
fn version(
  chunks: &mut Chunks,
  files: &mut Files,
  values: &mut Vec<Value>,
  (ops, next): (&[Op], &[u8]),
) -> bool {
  files.insert("f", next.to_vec());
  let block = files.block_of("f").unwrap();
  let changed = values.last_mut().and_then(|latest| {
    latest.supersede();
    change(latest, chunks, files.blocks_mut(), (block, ops))
  });
  let (value, taken) = match changed {
    Some(changed) => (changed.value, true),
    None => {
      let block = files.hold_block_of("f").unwrap();
      (
        Value::named(chunks, files.blocks_mut(), block, next.len()),
        false,
      )
    }
  };
  values.push(value);
  taken
}

/// The same version as today's history makes it (the reference).
fn reference_version(
  chunks: &mut reference::Chunks,
  files: &mut Files,
  values: &mut Vec<reference::Value>,
  (ops, next): (&[Op], &[u8]),
) {
  files.insert("f", next.to_vec());
  if let Some(latest) = values.last_mut() {
    latest.supersede();
  }
  let block = files.block_of("f").unwrap();
  let anchored = reference::reanchor(
    values.iter_mut().collect(),
    chunks,
    files.blocks_mut(),
    (block, ops),
  );
  let value = match anchored {
    Some(done) => done.value,
    None => {
      let block = files.hold_block_of("f").unwrap();
      reference::Value::named(chunks, files.blocks_mut(), block, next.len())
    }
  };
  values.push(value);
}

/// Releases every value, then removes the file: neither the chunks nor the blocks may hold anything after.
fn release_all(chunks: &mut Chunks, files: &mut Files, values: Vec<Value>) {
  for value in values {
    value.release(chunks, files.blocks_mut());
  }
  files.remove("f");
  assert_eq!((chunks.bytes(), files.blocks().live()), (0, 0));
}

/// The most of a value of `len` bytes that `ops` can leave unkept: what they overwrite, delete or truncate.
fn removed_by(ops: &[Op], len: usize) -> usize {
  ops
    .iter()
    .map(|op| match op.kind {
      OpKind::Overwrite | OpKind::Delete => op.len as usize,
      OpKind::Truncate => len.saturating_sub(op.at as usize),
      _ => 0,
    })
    .sum::<usize>()
    .min(len)
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]
  /// A-818 piece 2b, the oracle (main's rulings of 2026-11-21 09:40 and 10:35: every version's reads equal to today's
  /// re-anchor on generated histories). Do: from a generated first value, apply generated op chains version after
  /// version, both as this history makes them and as today's did (`history_reference`), each over its own files; after
  /// every change read every version kept, whole and at a generated range; once, release the versions before a
  /// generated floor in both. Expect: every version reads as today's history reads it and as `apply` makes it; the
  /// history owns no more than what its changes removed (and the earlier block of a change that removed it all); its
  /// running total is its recount; and after the release nothing is left.
  #[test]
  fn every_version_reads_as_todays_reanchored_history(
    first in prop::collection::vec(any::<u8>(), 0..512),
    chain in prop::collection::vec(prop::collection::vec((any::<u8>(), any::<u16>(), any::<u16>(), any::<u16>()), 0..6), 1..16),
    post in prop::collection::vec(any::<u8>(), 256..512),
    range in (any::<u16>(), any::<u16>()),
    floor_at in any::<u8>(),
  ) {
    let (mut chunks, mut files) = (Chunks::default(), Files::default());
    let (mut old_chunks, mut old_files) = (reference::Chunks::default(), Files::default());
    let mut values = vec![first_value(&mut chunks, &mut files, &first)];
    old_files.insert("f", first.clone());
    let block = old_files.hold_block_of("f").unwrap();
    let mut old_values = vec![reference::Value::named(&mut old_chunks, old_files.blocks_mut(), block, first.len())];
    let mut bytes = vec![first.clone()];
    let (mut floor, mut removed) = (0usize, 0usize);
    for (step, raw) in chain.iter().enumerate() {
      let previous = bytes.last().unwrap().clone();
      let ops = ops_over(previous.len() as u64, raw);
      let next = apply(&previous, &ops, &post);
      let owned = chunks.owned_bytes();
      if version(&mut chunks, &mut files, &mut values, (&ops, &next)) {
        let grew = chunks.owned_bytes().saturating_sub(owned);
        prop_assert!(grew <= removed_by(&ops, previous.len()) || grew == previous.len());
        removed += grew;
      }
      reference_version(&mut old_chunks, &mut old_files, &mut old_values, (&ops, &next));
      bytes.push(next);
      if step == usize::from(floor_at) % chain.len() {
        floor = values.len() - 1;
        for (value, old) in values.iter_mut().zip(old_values.iter_mut()).take(floor) {
          std::mem::take(value).release(&mut chunks, files.blocks_mut());
          std::mem::take(old).release(&mut old_chunks, old_files.blocks_mut());
        }
      }
      for ((value, old), expected) in values.iter().zip(&old_values).zip(&bytes).skip(floor) {
        let read = value.materialize(&chunks, files.blocks());
        prop_assert_eq!(&read, &old.materialize(&old_chunks, old_files.blocks()));
        prop_assert_eq!(&read, expected);
        let len = expected.len();
        let from = if len == 0 { 0 } else { usize::from(range.0) % len };
        let to = from + usize::from(range.1) % (len - from + 1);
        prop_assert_eq!(
          value.read((&chunks, files.blocks()), from, to),
          old.read((&old_chunks, old_files.blocks()), from, to)
        );
      }
      prop_assert_eq!(chunks.referenced_bytes(values.iter()), chunks.bytes());
      prop_assert!(chunks.owned_bytes() <= removed);
    }
    release_all(&mut chunks, &mut files, values);
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]
  /// §4.16 "The verdict" (the whole-file identity check). Do: from a generated first value, build a head by a generated
  /// chain of versions (one, at a generated place, stored whole so it shares nothing); pick a generated base and
  /// generated ops over it, the head's own last ops among them so the two converge. Expect: `applied_equals` answers
  /// exactly whether `apply` of the ops to the base's bytes equals the head's bytes, for every pick.
  #[test]
  fn applied_equals_is_the_bytes_compared(
    first in prop::collection::vec(any::<u8>(), 0..256),
    chain in prop::collection::vec(prop::collection::vec((any::<u8>(), any::<u16>(), any::<u16>(), any::<u16>()), 0..5), 1..8),
    whole_at in any::<u8>(),
    pick in any::<u8>(),
    raw in prop::collection::vec((any::<u8>(), any::<u16>(), any::<u16>(), any::<u16>()), 0..5),
    converge in any::<bool>(),
    post in prop::collection::vec(any::<u8>(), 0..300),
  ) {
    let mut chunks = Chunks::default();
    let mut files = Files::default();
    let mut values = vec![first_value(&mut chunks, &mut files, &first)];
    let mut made = vec![(first.clone(), Vec::new())];
    for (n, step) in chain.iter().enumerate() {
      let previous = made.last().unwrap().0.clone();
      let ops = ops_over(previous.len() as u64, step);
      let next = apply(&previous, &ops, &post);
      if n == usize::from(whole_at) % chain.len() {
        files.insert("f", next.clone());
        if let Some(latest) = values.last_mut() {
          latest.supersede();
        }
        values.push(Value::whole(&mut chunks, &next));
      } else {
        version(&mut chunks, &mut files, &mut values, (&ops, &next));
      }
      made.push((next, ops));
    }
    let (head_bytes, head_ops) = made.last().unwrap();
    let at = if converge { made.len() - 2 } else { usize::from(pick) % made.len() };
    let base_bytes = &made[at].0;
    let ops = if converge { head_ops.clone() } else { ops_over(base_bytes.len() as u64, &raw) };
    let expected = apply(base_bytes, &ops, &post) == *head_bytes;
    prop_assert_eq!(values[at].applied_equals(
      (&chunks, files.blocks()),
      (&ops, crate::named::Post::of(&post, &[])),
      values.last().unwrap(),
    ), expected);
  }
}

/// W2, A-793, A-818. Do: a 1 MiB file, then 64 versions each overwriting 64 bytes of its own range; then release
/// everything. Expect: the history holds the current file's block and the 64 bytes each change overwrote, owned; not
/// the first file's block, and not 64 copies of the file; every version reads back; nothing is left after the release.
#[test]
fn small_overwrites_of_a_large_file_cost_what_they_wrote() {
  const FILE: usize = 1 << 20;
  const EDIT: u64 = 64;
  const EDITS: usize = 64;
  let mut chunks = Chunks::default();
  let mut files = Files::default();
  let mut bytes: Vec<u8> = (0..FILE).map(|n| (n % 251) as u8).collect();
  let mut values = vec![first_value(&mut chunks, &mut files, &bytes)];
  let mut versions = vec![bytes.clone()];
  for n in 0..EDITS {
    let at = (n * FILE / EDITS) as u64;
    let post = vec![n as u8; EDIT as usize];
    let ops = [op_of(0, at, EDIT, 0)];
    let next = apply(&bytes, &ops, &post);
    assert!(
      version(&mut chunks, &mut files, &mut values, (&ops, &next)),
      "edit {n}"
    );
    bytes = next;
    versions.push(bytes.clone());
  }
  assert_eq!(
    chunks.owned_bytes(),
    EDITS * EDIT as usize,
    "each change's overwritten bytes, once"
  );
  assert_eq!(files.blocks().live(), 1, "the current file's block alone");
  for (value, expected) in values.iter().zip(&versions) {
    assert_eq!(&value.materialize(&chunks, files.blocks()), expected);
  }
  release_all(&mut chunks, &mut files, values);
}

/// A-728, A-818. Do: a file's block named as a value; a small overwrite made over it; then a change that deletes it
/// all and inserts new bytes; then the file removed and every value released. Expect: naming copies no byte; the
/// overwrite stores its 8 overwritten bytes and lets the first block go; the change that removes everything keeps the
/// block it superseded whole as the earlier version's bytes, copying nothing; every value reads back, the first ones
/// after the file is gone; nothing is left after the release.
#[test]
fn a_change_that_removes_everything_keeps_the_block_it_superseded() {
  let mut files = Files::default();
  let first: Vec<u8> = (0..4096u32)
    .map(|n| u8::try_from(n % 251).unwrap())
    .collect();
  let mut chunks = Chunks::default();
  let mut values = vec![first_value(&mut chunks, &mut files, &first)];
  assert_eq!(chunks.owned_bytes(), 0, "naming the block copies nothing");
  let ops = [op_of(0, 100, 8, 0)];
  let next = apply(&first, &ops, &[7u8; 8]);
  assert!(version(&mut chunks, &mut files, &mut values, (&ops, &next)));
  assert_eq!(chunks.owned_bytes(), 8, "the 8 bytes overwritten");
  assert_eq!(files.blocks().live(), 1, "the first block let go");
  let replace = [op_of(2, 0, next.len() as u64, 0), op_of(1, 0, 16, 0)];
  let replaced = apply(&next, &replace, &[9u8; 16]);
  assert!(version(
    &mut chunks,
    &mut files,
    &mut values,
    (&replace, &replaced)
  ));
  assert_eq!(
    chunks.owned_bytes(),
    8,
    "the superseded block kept, not copied"
  );
  assert_eq!(
    files.blocks().live(),
    2,
    "the replaced version's block and the replacing file's"
  );
  files.remove("f");
  for (value, expected) in values.iter().zip([&first, &next, &replaced]) {
    assert_eq!(&value.materialize(&chunks, files.blocks()), expected);
  }
  for value in values {
    value.release(&mut chunks, files.blocks_mut());
  }
  assert_eq!((chunks.bytes(), files.blocks().live()), (0, 0));
}

/// A-728 part 4, A-818. Do: overwrite 4 bytes of a file while another holder keeps its first block (as the identity
/// table or a branch's map would); then let that holder go. Expect the change to copy the 4 bytes it overwrote and let
/// its hold on the first block go, so the first block lives only while the other holder keeps it; every value reads
/// back; nothing is left after the release.
#[test]
fn a_change_copies_what_it_overwrote_whoever_holds_the_block() {
  let mut files = Files::default();
  let mut chunks = Chunks::default();
  let first = vec![1u8; 512];
  let mut values = vec![first_value(&mut chunks, &mut files, &first)];
  let held = files.hold_block_of("f").unwrap();
  let ops = [op_of(0, 10, 4, 0)];
  let next = apply(&first, &ops, &[2u8; 4]);
  assert!(version(&mut chunks, &mut files, &mut values, (&ops, &next)));
  assert_eq!(chunks.owned_bytes(), 4, "the overwritten bytes, copied");
  files.blocks_mut().release(held);
  assert_eq!(
    files.blocks().live(),
    1,
    "the current block alone once the holder goes"
  );
  for (value, expected) in values.iter().zip([&first, &next]) {
    assert_eq!(&value.materialize(&chunks, files.blocks()), expected);
  }
  release_all(&mut chunks, &mut files, values);
}

/// A-803's red, A-818 (main's order of 2026-11-21 09:40). Do: a 16 KiB file, then 2,000 versions alternating an
/// overwrite and an insert, every version kept (a reader pinning the floor). Expect: each change makes no more tree
/// nodes than its parts' bound, whatever the history's length (the re-anchor walked every value: 101 to 2,551 a change
/// at 100 to 5,000 versions); the first version reads as it was.
#[test]
fn a_change_walks_no_version_however_long_its_history() {
  const LEN: usize = 16 << 10;
  const CHANGES: u64 = 2000;
  let mut chunks = Chunks::default();
  let mut files = Files::default();
  let first: Vec<u8> = (0..LEN).map(|n| (n % 241) as u8).collect();
  let mut values = vec![first_value(&mut chunks, &mut files, &first)];
  let mut bytes = first.clone();
  let mut most = 0u64;
  for n in 0..CHANGES {
    let at = (n * 97) % 8192;
    let ops = [op_of(if n % 2 == 0 { 0 } else { 1 }, at, 64, 0)];
    let next = apply(&bytes, &ops, &[(n % 251) as u8; 64]);
    let before = chunks.nodes_made();
    assert!(version(&mut chunks, &mut files, &mut values, (&ops, &next)));
    most = most.max(chunks.nodes_made() - before);
    bytes = next;
  }
  let bound = crate::piece_tree::rebuild_bound(3, bytes.len() as u64) as u64;
  assert!(most > 0, "no change made a node");
  assert!(most <= bound, "{most} nodes a change, bound {bound}");
  assert_eq!(values[0].materialize(&chunks, files.blocks()), first);
  release_all(&mut chunks, &mut files, values);
}
