//! Tests of the changesets the server makes (A-731), held to stock 7.2's own: a history committed with `hg commit -u
//! 'Test <test@example.com>' -d '0 0'` (recorded 2026-11-20, macOS arm64, this Mac, mercurial 7.2 through uvx).

use slates_vcs::ContentStore;

use super::{Authored, Change, Unmade, made};
use crate::index::HgIndex;
use crate::manifest::Flag;
use crate::push::commit::fork_with;
use crate::push::held::IndexHeld;
use crate::pushkey::hex_node;
use crate::{Admission, NULL_NODE, Node};

/// The window the tests' stores are made with.
const WINDOW: usize = 1 << 12;

/// Shape: stock 7.2's nodes for `one` (add `a`), `two` (`a` changed, executable `b` added) and `three` (on branch
/// `stable`, `a` removed): each changeset's node and its manifest's.
const STOCK: [(&str, &str); 3] = [
  (
    "951f20955075c11a8299b5dd1a8970f1eeb80bf5",
    "a0c8bcbbb45c63b90b70ad007bf38961f64f2af0",
  ),
  (
    "1778c940704b586499edf9c762a101f3b08a3fd6",
    "a508925fddfff3de68bf2efa2a5442fedd9c6b08",
  ),
  (
    "5a1a54ffc2d508e6a09f3f8f94f9b3799dd0419a",
    "01eebbe42d2490135eaa1e5db11ef5693566c9b6",
  ),
];

fn node(hex: &str) -> Node {
  hex_node(hex.as_bytes()).unwrap().unwrap()
}

fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// Makes the changeset of `changes` over `parent` on `branch` with `message`, keeps it in `index`: its node and its
/// manifest's.
fn commit(
  (index, store): (&mut HgIndex, &mut ContentStore),
  parent: Node,
  changes: &[Change<'_>],
  (branch, message): (&[u8], &[u8]),
) -> (Node, Node) {
  let authored = Authored {
    user: b"Test <test@example.com>",
    seconds: 0,
    branch,
    message,
  };
  let verified = made(
    &IndexHeld::new(index, store),
    parent,
    changes,
    &authored,
    &mut room(),
  )
  .unwrap();
  let fork = fork_with(index, store, &verified, &mut room()).unwrap();
  core::mem::replace(index, fork).release(store).unwrap();
  (verified.changesets[0].node, verified.manifests[0].node)
}

/// A-731. Do: make `one`, `two` and `three` as stock 7.2 committed them, each over the one before, kept in an index.
/// Expect each changeset's and manifest's node stock 7.2's: the file texts, the manifest's flags, the files touched,
/// the branch's extra and the removal written as 7.2 writes them.
#[test]
fn a_made_history_is_the_one_stock_7_2_commits() {
  let mut store = ContentStore::new(1 << 24, WINDOW).unwrap();
  let mut index = HgIndex::new();
  let one = commit(
    (&mut index, &mut store),
    NULL_NODE,
    &[Change {
      path: b"a",
      after: Some((b"a\n", Flag::Regular)),
      copied_from: None,
    }],
    (b"default", b"one"),
  );
  assert_eq!(one, (node(STOCK[0].0), node(STOCK[0].1)));
  let two = commit(
    (&mut index, &mut store),
    one.0,
    &[
      Change {
        path: b"b",
        after: Some((b"b\n", Flag::Executable)),
        copied_from: None,
      },
      Change {
        path: b"a",
        after: Some((b"a2\n", Flag::Regular)),
        copied_from: None,
      },
    ],
    (b"default", b"two"),
  );
  assert_eq!(two, (node(STOCK[1].0), node(STOCK[1].1)));
  let three = commit(
    (&mut index, &mut store),
    two.0,
    &[Change {
      path: b"a",
      after: None,
      copied_from: None,
    }],
    (b"stable", b"three"),
  );
  assert_eq!(three, (node(STOCK[2].0), node(STOCK[2].1)));
  index.release(&mut store).unwrap();
}

/// A-731. Do: make a changeset over `one` whose only change gives `a` the bytes and flag it has, and one over an unknown
/// parent. Expect the first refused as changing nothing, the second as an unknown parent.
#[test]
fn a_version_that_changes_nothing_or_names_no_parent_makes_none() {
  let mut store = ContentStore::new(1 << 24, WINDOW).unwrap();
  let mut index = HgIndex::new();
  let one = commit(
    (&mut index, &mut store),
    NULL_NODE,
    &[Change {
      path: b"a",
      after: Some((b"a\n", Flag::Regular)),
      copied_from: None,
    }],
    (b"default", b"one"),
  );
  let authored = Authored {
    user: b"T <t@e>",
    seconds: 0,
    branch: b"default",
    message: b"m",
  };
  let held = IndexHeld::new(&index, &store);
  let same = [Change {
    path: b"a",
    after: Some((b"a\n", Flag::Regular)),
    copied_from: None,
  }];
  assert_eq!(
    made(&held, one.0, &same, &authored, &mut room()).err(),
    Some(Unmade::NothingChanged)
  );
  assert_eq!(
    made(&held, [7; 20], &same, &authored, &mut room()).err(),
    Some(Unmade::ParentUnknown)
  );
  index.release(&mut store).unwrap();
}

/// A-729 part 2 (an import's record holds a manifest as the delta the index keeps it as). Do: make a history of 64
/// files and then 15 changesets each changing one, kept in an index (whose manifests are kept as deltas, A-729 part 1);
/// write its record (`record::write`) and the record of the same revisions with every text whole
/// (`record::write_kept`); replay the first into an empty index. Expect the delta record less than half the whole one,
/// and the replayed index the made one row for row, so the record of the next is the same bytes.
#[test]
fn an_imports_record_holds_its_manifests_as_deltas_and_replays_row_for_row() {
  use crate::import::record;
  use crate::push::verify::LogOf;
  let mut store = ContentStore::new(1 << 26, WINDOW).unwrap();
  let mut index = HgIndex::new();
  let paths: Vec<Vec<u8>> = (0..64)
    .map(|file| format!("src/file-{file:02}.rs").into_bytes())
    .collect();
  let added: Vec<Change<'_>> = paths
    .iter()
    .map(|path| Change {
      path,
      after: Some((b"fn main() {}\n", Flag::Regular)),
      copied_from: None,
    })
    .collect();
  let mut head = commit(
    (&mut index, &mut store),
    NULL_NODE,
    &added,
    (b"default", b"add"),
  )
  .0;
  let bodies: Vec<Vec<u8>> = (0..15)
    .map(|at| format!("fn main() {{ {at} }}\n").into_bytes())
    .collect();
  for (at, body) in bodies.iter().enumerate() {
    let change = [Change {
      path: &paths[at],
      after: Some((body, Flag::Regular)),
      copied_from: None,
    }];
    head = commit(
      (&mut index, &mut store),
      head,
      &change,
      (b"default", b"change"),
    )
    .0;
  }
  let deltas = record::write(&index, &store, u64::MAX).unwrap();
  let mut logs = vec![(LogOf::Changelog, 0), (LogOf::Manifests, 0)];
  logs.extend(
    index
      .file_logs(&store)
      .map(|path| (LogOf::File(path.to_vec()), 0)),
  );
  let whole = record::write_kept(&index, &store, &logs, u64::MAX).unwrap();
  assert!(
    deltas.len() * 2 < whole.len(),
    "{} bytes with deltas against {} whole",
    deltas.len(),
    whole.len()
  );
  let mut replay_store = ContentStore::new(1 << 26, WINDOW).unwrap();
  let mut replayed = HgIndex::new();
  let counted = record::replay(&deltas, &mut replayed, &mut replay_store).unwrap();
  assert_eq!(counted.manifests, 16);
  let rows = |index: &mut HgIndex, store: &ContentStore| -> Vec<(Vec<u8>, Vec<u8>)> {
    index
      .rows()
      .iter(store)
      .map(|row| (row.key().to_vec(), row.body().to_vec()))
      .collect()
  };
  assert_eq!(rows(&mut replayed, &replay_store), rows(&mut index, &store));
  assert_eq!(
    record::write(&replayed, &replay_store, u64::MAX).unwrap(),
    deltas
  );
  replayed.release(&mut replay_store).unwrap();
  index.release(&mut store).unwrap();
}

/// Shape: stock 7.2's nodes for `one` (add `a`), `two` (`hg cp a b`) and `three` (`hg cp a c`, then `c` changed to
/// `c2`), committed as the tests above commit (recorded 2026-11-20, macOS arm64, this Mac, mercurial 7.2 from
/// Homebrew): each changeset's node and its manifest's.
const COPIED: [(&str, &str); 3] = [
  (
    "951f20955075c11a8299b5dd1a8970f1eeb80bf5",
    "a0c8bcbbb45c63b90b70ad007bf38961f64f2af0",
  ),
  (
    "d0f53d73335f5ec35939a4529932b829993da8b5",
    "0796472117e33ad9d223297bb640055aca5b0c18",
  ),
  (
    "a726b80c089129ea379db9cb6f4c6fa1c39c9731",
    "db0d8a056f0a17c5f68d3fffb7245b2019926030",
  ),
];

/// A-731 (a copy, A-626 part 4). Do: make `one`, then `two` copying `a` to `b` unchanged, then `three` copying `a` to
/// `c` with other bytes, as stock 7.2 committed them. Expect each changeset's and manifest's node stock 7.2's: the
/// copy's file revision names its source and the source's node (`copy`, `copyrev`) with a null first parent, its
/// bytes the copy's own.
#[test]
fn a_copy_is_made_as_stock_7_2_commits_one() {
  let mut store = ContentStore::new(1 << 24, WINDOW).unwrap();
  let mut index = HgIndex::new();
  let plain = |path: &'static [u8], bytes: &'static [u8], copied_from| Change {
    path,
    after: Some((bytes, Flag::Regular)),
    copied_from,
  };
  let one = commit(
    (&mut index, &mut store),
    NULL_NODE,
    &[plain(b"a", b"a\n", None)],
    (b"default", b"one"),
  );
  assert_eq!(one, (node(COPIED[0].0), node(COPIED[0].1)));
  let two = commit(
    (&mut index, &mut store),
    one.0,
    &[plain(b"b", b"a\n", Some(&b"a"[..]))],
    (b"default", b"two"),
  );
  assert_eq!(two, (node(COPIED[1].0), node(COPIED[1].1)));
  let three = commit(
    (&mut index, &mut store),
    two.0,
    &[plain(b"c", b"c2\n", Some(&b"a"[..]))],
    (b"default", b"three"),
  );
  assert_eq!(three, (node(COPIED[2].0), node(COPIED[2].1)));
  index.release(&mut store).unwrap();
}

/// A-729 part 2 (an import's record holds a file revision as its delta against the one before). Do: make a history of
/// one file grown by a line in each of 40 changesets, kept in an index; write its record (`record::write`) and the record
/// of the same revisions with every text whole (`record::write_kept`); replay the first into an empty index. Expect the
/// delta record less than half the whole one (18,755 bytes against 54,482 when written, the changesets whole), the
/// replayed index the made one row for row, and its record the same bytes.
#[test]
fn an_imports_record_holds_a_files_revisions_as_deltas_and_replays_row_for_row() {
  use crate::import::record;
  use crate::push::verify::LogOf;
  let mut store = ContentStore::new(1 << 26, WINDOW).unwrap();
  let mut index = HgIndex::new();
  let bodies: Vec<Vec<u8>> = (1..=40)
    .map(|lines| {
      (0..lines)
        .map(|line| format!("line {line} of a file that grows a line a changeset\n"))
        .collect::<String>()
        .into_bytes()
    })
    .collect();
  let mut head = NULL_NODE;
  for body in &bodies {
    let change = [Change {
      path: b"grown.txt",
      after: Some((body, Flag::Regular)),
      copied_from: None,
    }];
    head = commit(
      (&mut index, &mut store),
      head,
      &change,
      (b"default", b"grow"),
    )
    .0;
  }
  let deltas = record::write(&index, &store, u64::MAX).unwrap();
  let mut logs = vec![(LogOf::Changelog, 0), (LogOf::Manifests, 0)];
  logs.extend(
    index
      .file_logs(&store)
      .map(|path| (LogOf::File(path.to_vec()), 0)),
  );
  let whole = record::write_kept(&index, &store, &logs, u64::MAX).unwrap();
  assert!(
    deltas.len() * 2 < whole.len(),
    "{} bytes with deltas against {} whole",
    deltas.len(),
    whole.len()
  );
  let mut replay_store = ContentStore::new(1 << 26, WINDOW).unwrap();
  let mut replayed = HgIndex::new();
  let counted = record::replay(&deltas, &mut replayed, &mut replay_store).unwrap();
  assert_eq!(counted.file_revisions, 40);
  let rows = |index: &mut HgIndex, store: &ContentStore| -> Vec<(Vec<u8>, Vec<u8>)> {
    index
      .rows()
      .iter(store)
      .map(|row| (row.key().to_vec(), row.body().to_vec()))
      .collect()
  };
  assert_eq!(rows(&mut replayed, &replay_store), rows(&mut index, &store));
  assert_eq!(
    record::write(&replayed, &replay_store, u64::MAX).unwrap(),
    deltas
  );
  replayed.release(&mut replay_store).unwrap();
  index.release(&mut store).unwrap();
}
