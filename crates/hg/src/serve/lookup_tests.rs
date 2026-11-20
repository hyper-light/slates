//! Tests of `lookup` against stock 7.2's own answers, recorded on 2026-11-19 (macOS arm64, Mac Studio; mercurial 7.2
//! through uvx): a repository of 18 changesets on `default`, then one on branch `feature`, bookmark `mark` at revision
//! 3, each key sent as `lookup` to `hg -R r serve --stdio`.

use super::lookup;
use crate::phases::Phase;
use crate::pushkey::hex_node;
use crate::serve::tests::{Changeset, History, room};
use crate::{NULL_NODE, Node};

/// Shape: the recorded repository's nodes, by revision.
const NODES: [&str; 19] = [
  "102dd6493a1741b0b2b564ec476ded1f76b18ea9",
  "24043c285595f07284da8f8c652e2782dd397f87",
  "80e8d9e60213d494247f247b8fa129d8fee2cbc3",
  "34f79b95d2782ce451d80e72d2b63495ddbe93e7",
  "7e8ad00a1a56e84038721e8a4ff3186ad9d20588",
  "6fd67b0b7f7d2020304298ae22ffdfd30b7d9b0b",
  "d26dd8965aeb4898cc874d12f03ac5812397fcf2",
  "fc7af7bca6975df6c5e3204e8bc5b7c9e621b692",
  "987951173a97d64cfdfab2ab394bf5f050c5ad47",
  "587317a26d879ff913ed44ae54fbe32a7d30c60a",
  "394e1e2c3633183d14e7b18f768f219134f77e92",
  "93bc7d999e36bdadfe174ac44d6766c66c73e57b",
  "28e1de9008a299d69349b27c6a3b82469105459e",
  "26925756d3f43ba2f51dc539676f0ee65be97c2f",
  "e77904ec796fd129befe0c27b80f3bef4c69b3fc",
  "a0b447a3780ed43b2054cc1a6da49911415cbda1",
  "5809f50b9d44fcb9b7a43e42c7f8ff11fedab098",
  "45c4b8af58424225d168559a9a3abbe7195d60cd",
  "3614331e97131eba295ca4960f9b535e109a5cb2",
];

fn node(hex: &str) -> Node {
  hex_node(hex.as_bytes()).unwrap().unwrap()
}

/// The recorded repository as a served view.
fn recorded() -> History {
  let mut history = History {
    publishing: true,
    ..History::default()
  };
  for (rev, hex) in NODES.iter().enumerate() {
    let parent = rev
      .checked_sub(1)
      .map(|parent| u32::try_from(parent).unwrap());
    let branch: &[u8] = if rev == 18 { b"feature" } else { b"default" };
    history.push(Changeset {
      node: node(hex),
      parents: [parent, None],
      branch: branch.to_vec(),
      closes: false,
      phase: Phase::Public,
    });
  }
  history.bookmarks.push((b"mark".to_vec(), node(NODES[3])));
  history
}

fn answer(history: &History, key: &str) -> String {
  let mut out = Vec::new();
  lookup(history, key.as_bytes(), &mut room(), &mut out).unwrap();
  String::from_utf8(out).unwrap()
}

fn found(rev: usize) -> String {
  format!("1 {}\n", NODES[rev])
}

/// Do: look up a full node, a unique prefix, the names 7.2 knows first, revision numbers, a bookmark and two branches
/// in the recorded repository. Expect stock 7.2's answers byte for byte: `2` is revision 2, not a prefix; `-1` the tip;
/// `default` its last head, `feature` its own.
#[test]
fn names_resolve_as_stock_7_2_resolved_them() {
  let history = recorded();
  let null = format!("1 {}\n", "0".repeat(40));
  let cases = [
    (NODES[0], found(0)),
    ("102dd6", found(0)),
    ("2", found(2)),
    ("tip", found(18)),
    ("null", null.clone()),
    ("0", found(0)),
    ("-1", found(18)),
    ("mark", found(3)),
    ("feature", found(18)),
    ("default", found(17)),
  ];
  for (key, expected) in cases {
    assert_eq!(answer(&history, key), expected, "key {key:?}");
  }
  assert_eq!(
    answer(&history, "."),
    null,
    "no working directory: its parent is null"
  );
}

/// Do: look up a revision past the view, a name nothing has, and two prefixes every node begins (the empty one, and
/// `x`, whose leading `x` 7.2 drops). Expect stock 7.2's words: unknown revision, and the changelog's ambiguous
/// identifier naming the prefix it tried.
#[test]
fn unknown_names_and_ambiguous_prefixes_are_answered_in_7_2s_words() {
  let history = recorded();
  let cases = [
    ("19", "0 unknown revision '19'\n"),
    ("nope", "0 unknown revision 'nope'\n"),
    ("x", "0 00changelog@: ambiguous identifier\n"),
    ("", "0 00changelog@: ambiguous identifier\n"),
  ];
  for (key, expected) in cases {
    assert_eq!(answer(&history, key), expected, "key {key:?}");
  }
}

/// Do: look up `tip`, `-1` and two hex prefixes in an empty repository. Expect `tip` the null node, `-1` and the
/// prefix unknown except a prefix of the null node's, which 7.2's radix tree holds.
#[test]
fn an_empty_repository_knows_only_the_null_node() {
  let history = History::default();
  let null = format!("1 {}\n", hex_of(&NULL_NODE));
  assert_eq!(answer(&history, "tip"), null);
  assert_eq!(answer(&history, "00"), null);
  assert_eq!(answer(&history, "-1"), "0 unknown revision '-1'\n");
  assert_eq!(answer(&history, "ab"), "0 unknown revision 'ab'\n");
}

fn hex_of(node: &Node) -> String {
  let mut out = Vec::new();
  crate::pushkey::encode_hex(node, &mut out);
  String::from_utf8(out).unwrap()
}
