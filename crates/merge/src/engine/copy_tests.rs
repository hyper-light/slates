//! The engine's `Copy` (A-586; A-542's second half; §4.17.4 item 3 and piece 3): a tree copied by reference reads as a
//! copy at every version, is recorded once, costs its paths and never its tree, is refused typed where Subversion's
//! commit would not take it, lands with the edits its own increment makes beneath it, replays from its chain entry, is
//! taken away by a branch made before it, and keeps the identity equal to its recount. The oracle holds a green to a
//! reference green that copies key by key, through the engine's plain operations, over generated histories.

use std::cell::RefCell;
use std::collections::BTreeSet;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::copy::{appearance_bytes, in_tree, joined, relative};
use super::{Green, Increment, Lineage, Outcome, Rebased, change_bytes};
use crate::chain_entry;
use crate::increment::Base;
use crate::ops_doc::{NODE_EXCLUSIVE, Op, OpKind, OpsDoc};
use crate::verdict::MergeConflictClass;

/// The property list a Subversion repository green keeps as one attribute of each node (§4.17.4 item 1).
const PROPS: &str = "slates.svn.props";

/// An increment under construction, as the deriver lays one out (paths interned, bytes in the post-state by offset).
#[derive(Clone, Default)]
struct Work {
  doc: OpsDoc,
  post: Vec<u8>,
  flags: u8,
}

impl Work {
  /// A work whose operations are node-exclusive, as a Subversion client's commit is.
  fn exclusive() -> Work {
    Work {
      flags: NODE_EXCLUSIVE,
      ..Work::default()
    }
  }

  fn push(&mut self, kind: OpKind, path: &str, at: u64, len: u64, src: u64) -> &mut Work {
    let path = self.doc.paths.intern(path);
    self.doc.ops.push(Op {
      kind,
      flags: self.flags,
      path,
      at,
      len,
      src,
    });
    self
  }

  fn name(&mut self, text: &str) -> u64 {
    u64::from(self.doc.paths.intern(text))
  }

  fn stash(&mut self, bytes: &[u8]) -> u64 {
    let offset = self.post.len() as u64;
    self.post.extend_from_slice(bytes);
    offset
  }

  fn bytes(&mut self, kind: OpKind, path: &str, at: u64, bytes: &[u8]) -> &mut Work {
    let src = self.stash(bytes);
    self.push(kind, path, at, bytes.len() as u64, src)
  }

  fn create(&mut self, path: &str, bytes: &[u8]) -> &mut Work {
    self.push(OpKind::Create, path, 0, 0, u64::MAX);
    self.bytes(OpKind::Insert, path, 0, bytes)
  }

  fn overwrite(&mut self, path: &str, at: u64, bytes: &[u8]) -> &mut Work {
    self.bytes(OpKind::Overwrite, path, at, bytes)
  }

  fn unlink(&mut self, path: &str) -> &mut Work {
    self.push(OpKind::Unlink, path, 0, 0, u64::MAX)
  }

  fn mkdir(&mut self, path: &str) -> &mut Work {
    self.push(OpKind::Mkdir, path, 0, 0, u64::MAX)
  }

  fn mode(&mut self, path: &str, mode: u32) -> &mut Work {
    self.push(OpKind::SetMode, path, 0, u64::from(mode), u64::MAX)
  }

  fn xattr(&mut self, path: &str, name: &str, value: &[u8]) -> &mut Work {
    let name = self.name(name);
    let src = self.stash(value);
    self.push(OpKind::SetXattr, path, name, value.len() as u64, src)
  }

  fn remove_xattr(&mut self, path: &str, name: &str) -> &mut Work {
    let name = self.name(name);
    self.push(OpKind::RemoveXattr, path, name, 0, u64::MAX)
  }

  fn symlink(&mut self, path: &str, target: &str) -> &mut Work {
    let target = self.name(target);
    self.push(OpKind::Symlink, path, 0, 0, target)
  }

  fn link(&mut self, path: &str, target: &str) -> &mut Work {
    let target = self.name(target);
    self.push(OpKind::Link, path, 0, 0, target)
  }

  fn rename(&mut self, from: &str, to: &str) -> &mut Work {
    let from = self.name(from);
    self.push(OpKind::Rename, to, 0, 0, from)
  }

  /// `from`'s tree at `version` copied to `to`.
  fn copy(&mut self, from: &str, to: &str, version: u64) -> &mut Work {
    let from = self.name(from);
    self.push(OpKind::Copy, to, version, 0, from)
  }

  /// The increment, identified by `id`, based on `base`.
  fn at(&self, id: u64, base: u64) -> Increment {
    let mut identity = [0u8; 32];
    identity[..8].copy_from_slice(&id.to_le_bytes());
    identity[8] = self.flags;
    let mut doc = self.doc.clone();
    doc.canonicalize();
    Increment {
      id: identity,
      base,
      doc,
      post_state: self.post.clone(),
      evidence: Vec::new(),
    }
  }
}

/// Submits `work` to `green` at its head as increment `id`, which must be accepted.
fn accept(green: &mut Green, work: &Work, id: u64) -> u64 {
  let base = green.head();
  match green.submit(&work.at(id, base)) {
    Outcome::Accepted { version } => version,
    other => panic!("increment {id} was not accepted: {other:?}"),
  }
}

/// The paths and classes of a conflict, or none for any other outcome.
fn windows(outcome: &Outcome) -> Option<Vec<(String, MergeConflictClass)>> {
  match outcome {
    Outcome::Conflict { windows } => Some(
      windows
        .iter()
        .map(|window| (window.path.clone(), window.class))
        .collect(),
    ),
    Outcome::Accepted { .. } | Outcome::BaseUnavailable { .. } => None,
  }
}

/// A green at version 1 holding a Subversion-like trunk: two directories with modes and properties and three files.
fn trunk() -> Green {
  let mut green = Green::new();
  let mut work = Work::default();
  work
    .mkdir("trunk")
    .mode("trunk", 0o755)
    .xattr("trunk", PROPS, b"p1")
    .create("trunk/a", b"alpha")
    .mode("trunk/a", 0o644)
    .create("trunk/c", b"gamma")
    .mkdir("trunk/d")
    .create("trunk/d/b", b"beta")
    .xattr("trunk/d/b", PROPS, b"p2");
  assert_eq!(accept(&mut green, &work, 1), 1);
  green
}

/// A-586. Do: copy `trunk` at version 1 to `branches/x`, node-exclusively. Expect: accepted as version 2; every file,
/// directory, mode and property of the trunk at `branches/x`'s place; nothing of it at version 1; the copy walked no
/// map, brought a tree and was refused by no map; the change index names the destination once; the identity equals its
/// recount; and the change index's bytes equal theirs.
#[test]
fn a_copy_shares_its_source_and_reads_as_its_copy() {
  let mut green = trunk();
  let (walked, grafted) = (green.walked(), green.copies_grafted());
  let version = accept(
    &mut green,
    Work::exclusive().copy("trunk", "branches/x", 1),
    2,
  );
  assert_eq!(version, 2);
  assert_eq!(green.walked(), walked, "the copy walked a map");
  assert_eq!(green.copies_grafted(), grafted + 1);
  assert_eq!(green.copies_refused(), 0);
  assert_eq!(green.content("branches/x/a"), Some(&b"alpha"[..]));
  assert_eq!(green.content("branches/x/d/b"), Some(&b"beta"[..]));
  assert!(green.is_dir("branches/x") && green.is_dir("branches/x/d"));
  assert_eq!(green.mode("branches/x"), Some(0o755));
  assert_eq!(green.mode("branches/x/a"), Some(0o644));
  assert_eq!(green.xattr("branches/x", PROPS), Some(&b"p1"[..]));
  assert_eq!(green.xattr("branches/x/d/b", PROPS), Some(&b"p2"[..]));
  assert_eq!(green.content("trunk/a"), Some(&b"alpha"[..]));
  assert_eq!(green.content_at("branches/x/a", 1), None);
  assert_eq!(green.content_at("branches/x/a", 2), Some(b"alpha".to_vec()));
  assert_eq!(green.content_len_at("branches/x/d/b", 2), Some(4));
  let before = green.base_at(1);
  assert!(
    base_paths(&before)
      .iter()
      .all(|path| !path.starts_with("branches"))
  );
  let after = green.base_at(2);
  assert!(after.dirs.contains(&"branches/x/d".to_owned()));
  assert!(after.modes.contains(&("branches/x".to_owned(), 0o755)));
  assert_eq!(green.changed_between(1, 2), ["branches/x"]);
  assert_eq!(green.changed_since(1), ["branches/x"]);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  assert_eq!(green.changes_bytes(), green.changes_recounted());
}

/// A-586 (O(the paths), never the tree). Do: copy a tree of 4,096 files. Expect: no map walked, the histories' bytes
/// unchanged (a copy records no value), and the change index grown by exactly one entry for the destination, its last
/// change, and the copy's record.
#[test]
fn a_copy_costs_its_paths_whatever_its_tree_holds() {
  let mut green = Green::new();
  let mut work = Work::default();
  for index in 0..4096u32 {
    work.create(&format!("src/{}/{index}", index % 64), &index.to_le_bytes());
  }
  accept(&mut green, &work, 1);
  // The retained figures walk the current files for their sizes, so they are read before the walk counter.
  let (history, changes) = (green.retained_bytes().history, green.changes_bytes());
  let walked = green.walked();
  accept(&mut green, Work::exclusive().copy("src", "dst", 1), 2);
  assert_eq!(green.walked(), walked, "the copy walked a map");
  assert_eq!(
    green.retained_bytes().history,
    history,
    "the copy recorded values"
  );
  let grown = 2 * change_bytes("dst") + appearance_bytes("dst", "src");
  assert_eq!(green.changes_bytes() - changes, grown);
  assert_eq!(green.changes_bytes(), green.changes_recounted());
  assert_eq!(
    green.content("dst/7/4039"),
    Some(&4039u32.to_le_bytes()[..])
  );
  assert_eq!(green.files().count(), 8192);
}

/// A-586 (a key's first change after a copy records, at the copy's version, the value the copy gave it). Do: copy the
/// trunk at version 2, then at version 3 overwrite `b/a`'s first two bytes and set its mode. Expect: `b/a` absent at 1,
/// the copied bytes and mode at 2, the new ones at 3; a page and a length read at 2 from the copied bytes; the trunk's
/// file unchanged; and the identity equal to its recount after each version.
#[test]
fn a_keys_first_change_after_a_copy_records_the_value_the_copy_gave_it() {
  let mut green = trunk();
  accept(&mut green, Work::exclusive().copy("trunk", "b", 1), 2);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  accept(
    &mut green,
    Work::exclusive()
      .overwrite("b/a", 0, b"AL")
      .mode("b/a", 0o700),
    3,
  );
  assert_eq!(green.content_at("b/a", 1), None);
  assert_eq!(green.content_at("b/a", 2), Some(b"alpha".to_vec()));
  assert_eq!(green.content_at("b/a", 3), Some(b"ALpha".to_vec()));
  assert_eq!(
    green.content_page_at("b/a", 2, (1, 2)),
    Some((b"lp".to_vec(), 5))
  );
  assert_eq!(green.content_len_at("b/a", 2), Some(5));
  assert!(green.base_at(2).modes.contains(&("b/a".to_owned(), 0o644)));
  assert!(green.base_at(3).modes.contains(&("b/a".to_owned(), 0o700)));
  assert_eq!(green.content("trunk/a"), Some(&b"alpha"[..]));
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  assert_eq!(green.changed_between(2, 3), ["b/a"]);
}

/// A-586 (a commit that copies a directory and edits beneath it). Do: in one node-exclusive increment, copy the trunk
/// to `b`, overwrite `b/a`, create `b/new`, unlink `b/c`, set `b/d`'s mode and `b/d/b`'s property; then replay that
/// version's chain entry, encoded and decoded, on a green holding the version before it. Expect: accepted; the edits
/// land on what the copy brought, the trunk untouched; at the version before, none of it; no copied value recorded for
/// the version's own changes; and the replayed green reads and hashes alike.
#[test]
fn edits_beneath_a_copy_in_its_own_increment_land_on_what_it_brought() {
  let mut green = trunk();
  let mut replica = trunk();
  let mut work = Work::exclusive();
  work
    .copy("trunk", "b", 1)
    .overwrite("b/a", 0, b"AL")
    .create("b/new", b"new")
    .unlink("b/c")
    .mode("b/d", 0o700)
    .xattr("b/d/b", PROPS, b"q");
  let increment = work.at(2, 1);
  let (outcome, applied) = green.submit_resolved(&increment);
  assert_eq!(outcome, Outcome::Accepted { version: 2 });
  assert_eq!(green.content("b/a"), Some(&b"ALpha"[..]));
  assert_eq!(green.content("b/new"), Some(&b"new"[..]));
  assert_eq!(green.content("b/c"), None);
  assert_eq!(green.mode("b/d"), Some(0o700));
  assert_eq!(green.xattr("b/d/b", PROPS), Some(&b"q"[..]));
  assert_eq!(green.content("trunk/a"), Some(&b"alpha"[..]));
  assert_eq!(green.content("trunk/c"), Some(&b"gamma"[..]));
  assert_eq!(green.content_at("b/a", 1), None);
  assert_eq!(green.content_at("b/a", 2), Some(b"ALpha".to_vec()));
  assert_eq!(green.content_at("b/c", 2), None);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  let applied = applied.expect("an accepted version's effects");
  let entry = chain_entry::encode(&increment.encode(), &increment, &applied, None, None, None);
  let decoded = chain_entry::decode(&entry).expect("the entry decodes");
  assert_eq!(decoded.applied, applied);
  assert_eq!(
    replica.apply_recorded(&decoded.increment, &decoded.applied),
    2
  );
  assert_eq!(replica.head_identity(), green.head_identity());
  assert_eq!(replica.base_at(2), green.base_at(2));
  assert_eq!(replica.content("b/a"), Some(&b"ALpha"[..]));
}

/// A-586 (the root's copy: `svn copy ^/ ^/snap`, which a stock client sends). Do: give the root a property and copy it,
/// with every file beneath it, to `snap`. Expect: every path beneath `snap`, the root's property on `snap`, no path
/// ending in the separator, no map walked, and the identity equal to its recount.
#[test]
fn a_copy_of_the_root_brings_every_path_beneath_its_destination() {
  let mut green = Green::new();
  let mut work = Work::default();
  work
    .mkdir("")
    .xattr("", PROPS, b"root")
    .create("a", b"x")
    .mkdir("d")
    .create("d/f", b"y");
  accept(&mut green, &work, 1);
  let walked = green.walked();
  accept(&mut green, Work::exclusive().copy("", "snap", 1), 2);
  assert_eq!(green.walked(), walked, "the root's copy walked a map");
  assert_eq!(green.content("snap/a"), Some(&b"x"[..]));
  assert_eq!(green.content("snap/d/f"), Some(&b"y"[..]));
  assert!(green.is_dir("snap") && green.is_dir("snap/d"));
  assert_eq!(green.xattr("snap", PROPS), Some(&b"root"[..]));
  assert_eq!(green.xattr("", PROPS), Some(&b"root"[..]));
  assert!(
    base_paths(&green.base_at(2))
      .iter()
      .all(|path| !path.ends_with('/'))
  );
  assert_eq!(green.changed_between(1, 2), ["snap"]);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
}

/// A-586 (the verdict's refusals, each naming the destination by kind). Do: against a green holding a trunk, a hard
/// link under `h`, a file under `taken`, a trunk changed after version 1 and a path removed after version 3, submit
/// each refused copy. Expect: each a conflict with exactly its window, and the green unchanged: its head, its identity
/// and the copies it grafted.
#[test]
fn a_copy_is_refused_typed_and_changes_nothing() {
  let mut green = trunk();
  let mut work = Work::default();
  work
    .create("t2/a", b"t2")
    .link("h/l", "trunk/a")
    .create("taken/x", b"x");
  accept(&mut green, &work, 2);
  accept(&mut green, Work::default().create("gone/x", b"g"), 3);
  accept(&mut green, Work::default().unlink("gone/x"), 4);
  accept(&mut green, Work::default().overwrite("trunk/c", 0, b"G"), 5);
  let (head, identity, grafted) = (green.head(), green.head_identity(), green.copies_grafted());
  let mut both = Work::exclusive();
  both.copy("trunk", "y", 4).copy("t2", "y", 4);
  let mut nested = Work::exclusive();
  nested.copy("t2", "y", 4).copy("t2", "y/z", 4);
  let mut moved = Work::exclusive();
  moved.copy("t2", "y", 4).rename("trunk/a", "y/q");
  use MergeConflictClass::{CreateCreate, RenameRename, TypeChanged};
  let cases: Vec<(Work, u64, Vec<(&str, MergeConflictClass)>)> = vec![
    (one_copy("t2", "taken", 5), 5, vec![("taken", CreateCreate)]),
    (
      one_copy("t2", "trunk/a", 5),
      5,
      vec![("trunk/a", CreateCreate)],
    ),
    (one_copy("t2", "", 5), 5, vec![("", TypeChanged)]),
    (one_copy("nothing", "y", 5), 5, vec![("y", TypeChanged)]),
    (one_copy("t2", "y", 6), 5, vec![("y", TypeChanged)]),
    (one_copy("h", "y", 5), 5, vec![("y", TypeChanged)]),
    (one_copy("trunk", "y", 4), 5, vec![("y", RenameRename)]),
    (one_copy("t2", "gone", 5), 3, vec![("gone", CreateCreate)]),
    (both, 5, vec![("y", TypeChanged)]),
    (nested, 5, vec![("y/z", TypeChanged)]),
    (moved, 5, vec![("y/q", TypeChanged)]),
  ];
  for (index, (work, base, expected)) in cases.into_iter().enumerate() {
    let outcome = green.submit(&work.at(100 + index as u64, base));
    let expected: Vec<(String, MergeConflictClass)> = expected
      .into_iter()
      .map(|(path, class)| (path.to_owned(), class))
      .collect();
    let found: Vec<(String, MergeConflictClass)> = windows(&outcome).expect("a conflict");
    for window in &expected {
      assert!(
        found.contains(window),
        "case {index}: {window:?} not in {found:?}"
      );
    }
    assert_eq!(green.head(), head, "case {index} committed");
  }
  assert_eq!(green.head_identity(), identity);
  assert_eq!(green.copies_grafted(), grafted);
}

/// One node-exclusive copy of `from` at `version` to `to`.
fn one_copy(from: &str, to: &str, version: u64) -> Work {
  let mut work = Work::exclusive();
  work.copy(from, to, version);
  work
}

/// A-586 (an edit declared before a copy that replaced its file). Do: hold `d/x`, remove it, then copy a tree holding
/// `x` to `d`; submit a plain overwrite of `d/x` based before the removal, and another based on the copy. Expect: the
/// first a delete/modify conflict on `d/x` (the bytes it was declared against were replaced whole, which no delta
/// records), the second accepted onto the copied bytes.
#[test]
fn an_edit_declared_before_a_copy_replaced_its_file_conflicts() {
  let mut green = Green::new();
  accept(
    &mut green,
    Work::default()
      .create("d/x", b"old!")
      .create("t/x", b"new!"),
    1,
  );
  accept(&mut green, Work::default().unlink("d/x"), 2);
  accept(&mut green, Work::exclusive().copy("t", "d", 2), 3);
  let stale = green.submit(&Work::default().overwrite("d/x", 0, b"E").at(10, 1));
  assert_eq!(
    windows(&stale),
    Some(vec![("d/x".to_owned(), MergeConflictClass::DeleteModify)])
  );
  let fresh = green.submit(&Work::default().overwrite("d/x", 0, b"E").at(11, 3));
  assert_eq!(fresh, Outcome::Accepted { version: 4 });
  assert_eq!(green.content("d/x"), Some(&b"Eew!"[..]));
  assert_eq!(green.content_at("d/x", 1), Some(b"old!".to_vec()));
  assert_eq!(green.content_at("d/x", 2), None);
  assert_eq!(green.content_at("d/x", 3), Some(b"new!".to_vec()));
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
}

/// A-586 (a green made at another's version, before and after a copy). Do: copy the trunk at version 2, edit a copied
/// file at 3, and branch the green at 1, 2 and 3. Expect: each branch reads as the green read at its version (no copied
/// tree at 1, the copy's bytes at 2, the edit at 3) and hashes as a green that applied the same versions up to it; no
/// branch walks a map.
#[test]
fn a_branch_before_a_copy_holds_none_of_it_and_one_after_holds_it() {
  let mut green = trunk();
  let mut replayed = vec![trunk().head_identity()];
  accept(&mut green, Work::exclusive().copy("trunk", "b", 1), 2);
  {
    let mut reference = trunk();
    accept(&mut reference, Work::exclusive().copy("trunk", "b", 1), 2);
    replayed.push(reference.head_identity());
  }
  accept(&mut green, Work::exclusive().overwrite("b/a", 0, b"AL"), 3);
  replayed.push(green.head_identity());
  let reads: Vec<(Option<Vec<u8>>, bool)> = (1..=3)
    .map(|version| {
      (
        green.content_at("b/a", version),
        green.base_at(version).dirs.contains(&"b/d".to_owned()),
      )
    })
    .collect();
  let (mut lineage, source) = Lineage::new(green);
  let walked = lineage.walked();
  for (version, (expected_identity, (bytes, dir))) in (1..=3).zip(replayed.into_iter().zip(reads)) {
    let branch = lineage.branch(source, version).expect("a branch");
    let (identity, read, made) = lineage
      .with(branch, |green| {
        (
          green.head_identity(),
          green.content("b/a").map(<[u8]>::to_vec),
          green.is_dir("b/d"),
        )
      })
      .expect("the branch");
    assert_eq!(identity, expected_identity, "the identity at {version}");
    assert_eq!((read, made), (bytes, dir), "the reads at {version}");
    lineage
      .with(branch, |green| {
        assert_eq!(green.content("trunk/a"), Some(&b"alpha"[..]));
      })
      .expect("the branch");
  }
  assert_eq!(lineage.walked(), walked, "a branch walked a map");
}

/// A-586 (owed: a checkpoint listing a copy's tree and carrying the identity's sum). Do: checkpoint a green at the
/// version before its copy and at the copy's. Expect: the first taken, the second refused (`None`), which the service
/// answers by keeping the chain whole.
#[test]
fn a_green_is_not_checkpointed_from_its_first_copys_version() {
  let mut green = trunk();
  accept(&mut green, Work::default().create("e", b"e"), 2);
  accept(&mut green, Work::exclusive().copy("trunk", "b", 2), 3);
  assert!(green.checkpoint(2).is_some());
  assert!(green.checkpoint(3).is_none());
}

/// A-586 (owed: a rebase restating a copy). Do: rebase a work holding a copy. Expect: a type conflict naming its
/// destination, and the green untouched.
#[test]
fn a_rebase_of_a_work_with_a_copy_is_refused_typed() {
  let mut green = trunk();
  let identity = green.head_identity();
  let rebased = green.rebase(&one_copy("trunk", "b", 1).at(2, 1));
  assert_eq!(
    rebased,
    Rebased::Conflict {
      windows: vec![super::ConflictWindow {
        path: "b".to_owned(),
        range: crate::range::Range::new(0, 0),
        class: MergeConflictClass::TypeChanged,
      }],
    }
  );
  assert_eq!((green.head(), green.head_identity()), (1, identity));
}

/// Every path a base names, in any dimension.
fn base_paths(base: &Base) -> BTreeSet<String> {
  base
    .files
    .iter()
    .map(|(path, _)| path.clone())
    .chain(base.dirs.iter().cloned())
    .chain(base.modes.iter().map(|(path, _)| path.clone()))
    .chain(base.symlinks.iter().map(|(path, _)| path.clone()))
    .chain(base.xattrs.iter().map(|(path, _, _)| path.clone()))
    .chain(base.hardlinks.iter().map(|(path, _)| path.clone()))
    .chain(base.specials.iter().map(|(path, _)| path.clone()))
    .collect()
}

/// The paths the oracle's histories name: the root (as a copy's source only) first, then paths that are prefixes of one
/// another, siblings sharing a first byte, and paths often empty, so copies land and later steps change their trees.
const PATHS: [&str; 11] = [
  "", "a", "a/x", "a/x/y", "a/b", "b", "b/x", "ab", "c", "c/x", "d",
];

/// One generated step of the oracle (module docs).
#[derive(Clone, Debug)]
enum Step {
  /// A one-byte file and its property at a path (an overwrite where a file stands).
  Put { path: usize, byte: u8 },
  /// A file and its property taken away.
  Take { path: usize },
  /// A directory with its mode.
  Mkdir { path: usize, mode: u8 },
  /// A tree copied, from the head or a version or two before it.
  Copy { from: usize, to: usize, back: u8 },
}

fn step() -> impl Strategy<Value = Step> {
  prop_oneof![
    3 => (any::<usize>(), any::<u8>()).prop_map(|(path, byte)| Step::Put { path, byte }),
    1 => any::<usize>().prop_map(|path| Step::Take { path }),
    1 => (any::<usize>(), any::<u8>()).prop_map(|(path, mode)| Step::Mkdir { path, mode }),
    4 => (any::<usize>(), any::<usize>(), any::<u8>()).prop_map(|(from, to, back)| Step::Copy { from, to, back }),
  ]
}

/// What the generated histories met, so the oracle shows it compared real copies.
#[derive(Clone, Copy, Debug, Default)]
struct Census {
  accepted: usize,
  refused: usize,
  from_root: usize,
  from_before: usize,
  into_themselves: usize,
  changed_after: usize,
  /// Puts and takes of a path a copy's tree holds: a key's change after its copy.
  changed_beneath: usize,
}

/// A green that copies by reference, the reference that copies key by key, and what each accepted version applied.
#[derive(Default)]
struct Pair {
  green: Green,
  reference: Green,
  entries: Vec<(Increment, Vec<chain_entry::Applied>)>,
  next: u64,
}

impl Pair {
  /// Submits `work` to the green at its head; the outcome, and the entry recorded when it was accepted.
  fn submit(&mut self, work: &Work) -> bool {
    self.next += 1;
    let increment = work.at(self.next, self.green.head());
    let (outcome, applied) = self.green.submit_resolved(&increment);
    if let Some(applied) = applied {
      self.entries.push((increment, applied));
    }
    matches!(outcome, Outcome::Accepted { .. })
  }

  /// Submits `work` to the reference at its head; whether it was accepted.
  fn submit_reference(&mut self, work: &Work) -> bool {
    self.next += 1;
    let increment = work.at(self.next, self.reference.head());
    matches!(self.reference.submit(&increment), Outcome::Accepted { .. })
  }

  /// Applies one step to both; the green's and the reference's outcomes must agree.
  fn apply(&mut self, step: &Step, census: &mut Census) -> Result<(), TestCaseError> {
    let at = |index: usize| PATHS[1 + index % (PATHS.len() - 1)];
    if let Step::Put { path, .. } | Step::Take { path } = *step {
      census.changed_beneath += usize::from(self.green.copy_above(at(path)).is_some());
    }
    let work = match *step {
      Step::Put { path, byte } => self.put(at(path), byte),
      Step::Take { path } => self.take(at(path)),
      Step::Mkdir { path, mode } => {
        let mut work = Work::default();
        work.mkdir(at(path)).mode(at(path), u32::from(mode));
        Some(work)
      }
      Step::Copy { from, to, back } => {
        return self.copy(
          PATHS[from % PATHS.len()],
          PATHS[to % PATHS.len()],
          back,
          census,
        );
      }
    };
    if let Some(work) = work {
      let (green, reference) = (self.submit(&work), self.submit_reference(&work));
      prop_assert_eq!(green, reference, "{:?}", step);
    }
    Ok(())
  }

  /// A one-byte file at `path` and its property: an overwrite where the reference holds a file.
  fn put(&self, path: &str, byte: u8) -> Option<Work> {
    let mut work = Work::default();
    if self.reference.content(path).is_some() {
      work.overwrite(path, 0, &[byte]);
    } else {
      work.create(path, &[byte]);
    }
    work.xattr(path, "n", &[byte]);
    Some(work)
  }

  /// The file at `path` and its property taken away, where the reference holds one.
  fn take(&self, path: &str) -> Option<Work> {
    self.reference.content(path)?;
    let mut work = Work::default();
    work.unlink(path).remove_xattr(path, "n");
    Some(work)
  }

  /// A copy of `from` at the head less `back % 3` to `to`: by reference in the green, key by key in the reference when
  /// the reference's own state says the copy stands (module docs of `copy`); both must decide alike, and the copy must
  /// walk no map.
  fn copy(
    &mut self,
    from: &str,
    to: &str,
    back: u8,
    census: &mut Census,
  ) -> Result<(), TestCaseError> {
    let head = self.reference.head();
    let version = head.saturating_sub(u64::from(back % 3));
    let stands = self.copy_stands(from, to, version);
    let walked = self.green.walked();
    let accepted = self.submit(&one_copy(from, to, version));
    prop_assert_eq!(
      accepted,
      stands,
      "copy {:?} at {} to {:?}",
      from,
      version,
      to
    );
    if !accepted {
      census.refused += 1;
      census.changed_after += usize::from(
        self
          .reference
          .changed_between(version, head)
          .iter()
          .any(|path| in_tree(path, from)),
      );
      return Ok(());
    }
    prop_assert_eq!(self.green.walked(), walked, "the copy walked a map");
    census.accepted += 1;
    census.from_root += usize::from(from.is_empty());
    census.from_before += usize::from(version < head);
    census.into_themselves += usize::from(in_tree(to, from));
    let work = copied_key_by_key(&self.reference, from, to, version);
    prop_assert!(
      self.submit_reference(&work),
      "the reference refused its copy"
    );
    Ok(())
  }

  /// Whether the reference's own state lets a copy of `from` at `version` to `to` stand: the destination is not the root
  /// and nothing is in its tree at the head, the source's tree did not change after `version`, and it holds something
  /// then.
  fn copy_stands(&self, from: &str, to: &str, version: u64) -> bool {
    let head = self.reference.head();
    let occupied = base_paths(&self.reference.base_at(head))
      .iter()
      .any(|path| in_tree(path, to));
    let changed = self
      .reference
      .changed_between(version, head)
      .iter()
      .any(|path| in_tree(path, from));
    let held = base_paths(&self.reference.base_at(version))
      .iter()
      .any(|path| in_tree(path, from));
    !to.is_empty() && !occupied && !changed && held
  }

  /// Holds the green to the reference at every version (module docs): bases, bytes, the change index with each copy's
  /// destination standing for its tree; and the green's identity to its recount and its change index's bytes to theirs.
  fn check(&mut self) -> Result<(), TestCaseError> {
    let head = self.green.head();
    prop_assert_eq!(head, self.reference.head());
    for version in 0..=head {
      let base = self.green.base_at(version);
      prop_assert_eq!(
        &base,
        &self.reference.base_at(version),
        "the base at {}",
        version
      );
      for (path, _) in &base.files {
        prop_assert_eq!(
          self.green.content_at(path, version),
          self.reference.content_at(path, version)
        );
      }
      self.check_changes(version, head)?;
    }
    prop_assert_eq!(
      self.green.head_identity(),
      self.green.head_identity_from_scratch()
    );
    prop_assert_eq!(self.green.changes_bytes(), self.green.changes_recounted());
    prop_assert_eq!(self.green.copies_refused(), 0);
    Ok(())
  }

  /// The green's change index from `version` to `head`, each copy's destination opened into the paths of its tree the
  /// reference holds at either end, against the reference's with those destinations.
  fn check_changes(&self, version: u64, head: u64) -> Result<(), TestCaseError> {
    let ends: BTreeSet<String> = base_paths(&self.reference.base_at(version))
      .union(&base_paths(&self.reference.base_at(head)))
      .cloned()
      .collect();
    let copied = |path: &str| {
      self
        .green
        .copies
        .get(path)
        .is_some_and(|records| records.iter().any(|record| record.version > version))
    };
    let mut opened: BTreeSet<String> = BTreeSet::new();
    let mut destinations: BTreeSet<String> = BTreeSet::new();
    for path in self.green.changed_between(version, head) {
      if copied(&path) {
        opened.extend(ends.iter().filter(|end| in_tree(end, &path)).cloned());
        destinations.insert(path.clone());
      }
      opened.insert(path);
    }
    let mut expected: BTreeSet<String> = self
      .reference
      .changed_between(version, head)
      .into_iter()
      .collect();
    expected.extend(destinations);
    prop_assert_eq!(opened, expected, "the changes since {}", version);
    Ok(())
  }

  /// Replays every accepted version's chain entry, encoded and decoded, on a fresh green: it must read and hash as the
  /// green does.
  fn replay(&mut self) -> Result<(), TestCaseError> {
    let mut replica = Green::new();
    for (increment, applied) in &self.entries {
      let entry = chain_entry::encode(&increment.encode(), increment, applied, None, None, None);
      let decoded = chain_entry::decode(&entry).expect("an entry the engine recorded decodes");
      replica.apply_recorded(&decoded.increment, &decoded.applied);
    }
    let head = self.green.head();
    prop_assert_eq!(replica.head(), head);
    prop_assert_eq!(replica.base_at(head), self.green.base_at(head));
    prop_assert_eq!(replica.head_identity(), self.green.head_identity());
    Ok(())
  }
}

/// The reference's increment for a copy of `from`'s tree at `version` to `to`, key by key: each file created with its
/// bytes, each directory made, each mode, property and symbolic link set, read from the reference itself at that
/// version.
fn copied_key_by_key(reference: &Green, from: &str, to: &str, version: u64) -> Work {
  let base = reference.base_at(version);
  let place = |path: &str| relative(path, from).map(|rel| joined(to, rel));
  let mut work = Work::default();
  for (path, _) in &base.files {
    if let Some(at) = place(path) {
      let bytes = reference
        .content_at(path, version)
        .expect("a file of the base");
      work.create(&at, &bytes);
    }
  }
  for path in &base.dirs {
    if let Some(at) = place(path) {
      work.mkdir(&at);
    }
  }
  for (path, mode) in &base.modes {
    if let Some(at) = place(path) {
      work.mode(&at, *mode);
    }
  }
  for (path, target) in &base.symlinks {
    if let Some(at) = place(path) {
      work.symlink(&at, target);
    }
  }
  for (path, name, value) in &base.xattrs {
    if let Some(at) = place(path) {
      work.xattr(&at, name, value);
    }
  }
  work
}

/// Runs one history on a pair, checking after every step, then replays the green's chain.
fn run(steps: &[Step], census: &mut Census) -> Result<(), TestCaseError> {
  let mut pair = Pair::default();
  for step in steps {
    pair.apply(step, census)?;
    pair.check()?;
  }
  pair.replay()
}

/// A-586 (the oracle). Do: generated histories of files and their properties put and taken, directories made with
/// modes, and copies of any path (the root among them) at the head or a version or two before it to any path (into the
/// source's own tree among them), on a green and on a reference green that makes each copy key by key through the
/// engine's plain operations. Expect: the two decide every copy alike; after every step they read alike at every
/// version (each base, each file's bytes), their change indexes agree with each copy's destination standing for its
/// tree, every copy walked no map, the identity equals its recount and the change index's bytes theirs, no map refused
/// a copy its verdict passed; and the green's chain, replayed from its entries, reads and hashes as it does. The census
/// shows copies accepted and refused, from the root, from a version before the head, into their own source's tree,
/// refused for a source changed since, and keys put or taken beneath a copy.
#[test]
fn every_copy_reads_as_its_key_by_key_reference_over_every_history() {
  let census = RefCell::new(Census::default());
  let config = Config {
    cases: slates_test_seeds::cases(128),
    ..Config::default()
  };
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(config));
  let result = runner.run(&prop::collection::vec(step(), 1..24), |steps| {
    run(&steps, &mut census.borrow_mut())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  // The figures the evidence quotes (`-- --nocapture`).
  eprintln!("copy oracle census: {census:?}");
  let met = [
    census.accepted,
    census.refused,
    census.from_root,
    census.from_before,
    census.into_themselves,
    census.changed_after,
    census.changed_beneath,
  ];
  assert!(
    met.iter().all(|&count| count > 0),
    "a case never met: {census:?}"
  );
}
