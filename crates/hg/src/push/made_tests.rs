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
      },
      Change {
        path: b"a",
        after: Some((b"a2\n", Flag::Regular)),
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
