//! The engine's `Copy` (A-586, A-626; A-542's second half; §4.17.4 item 3 and piece 3): a tree copied by reference reads
//! as a copy at every version, is recorded once, costs its paths and never its tree, is refused typed where Subversion's
//! commit would not take it, lands with the edits its own increment makes beneath it, replaces what its destination
//! holds when it says so, nests with the other copies of its increment, replays from its chain entry, is taken away by a
//! branch made before it, and keeps the identity equal to its recount. The oracle holds a green to a model that states
//! every version's tree by the design's rules, over generated histories.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::copy::{appearance_bytes, in_tree, joined, relative};
use super::{Green, Increment, Lineage, Outcome, Reanchored, Rebased, change_bytes};
use crate::chain_entry;
use crate::increment::Base;
use crate::ops_doc::{NODE_EXCLUSIVE, NodeOrigin, NodeRef, Op, OpKind, OpsDoc, REPLACE};
use crate::range::Range;
use crate::review::OutdatedBy;
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
    self.doc.paths.intern(text)
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

  fn rmdir(&mut self, path: &str) -> &mut Work {
    self.push(OpKind::Rmdir, path, 0, 0, u64::MAX)
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

  /// A hard link at `path` to the node at `target` at the work's base (A-646).
  fn link(&mut self, path: &str, target: &str) -> &mut Work {
    let target = self.name(target);
    let node = self.doc.nodes.intern(NodeRef {
      path: target,
      origin: NodeOrigin::Base,
    });
    self.push(OpKind::Link, path, 0, 0, node)
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

  /// `from`'s tree at `version` copied to `to`, replacing what `to` holds (A-626).
  fn replace(&mut self, from: &str, to: &str, version: u64) -> &mut Work {
    let from = self.name(from);
    let flags = self.flags;
    self.flags = flags | REPLACE;
    self.push(OpKind::Copy, to, version, 0, from);
    self.flags = flags;
    self
  }

  /// A copy as the oracle declares one.
  fn declared(&mut self, copy: &Declared) -> &mut Work {
    if copy.replace {
      self.replace(&copy.src, &copy.dst, copy.version)
    } else {
      self.copy(&copy.src, &copy.dst, copy.version)
    }
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
/// the version's own changes; no removal recorded for an edited copied file (A-612); and the replayed green reads and
/// hashes alike.
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
  // The file the copy brought and its own increment edited replaced no file: A-612's removals record none, live or
  // replayed.
  assert!(!green.file_removed.contains_key("b/a"));
  assert!(!replica.file_removed.contains_key("b/a"));
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

/// A-586 and A-626 (the verdict's refusals, each naming the destination by kind). Do: against a green holding a trunk,
/// a hard link under `h`, a file under `taken`, a trunk changed after version 1 and a path removed after version 3,
/// submit each refused copy: among them a second copy onto a place the first copy's tree fills, and replaces of a tree
/// changed after the base, of a tree holding a hard link and of a link's target. Expect: each a conflict with exactly
/// its window, and the green unchanged: its head, its identity and the copies it grafted.
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
  nested.copy("t2", "y", 5).copy("trunk", "y/a", 5);
  let mut moved = Work::exclusive();
  moved.copy("t2", "y", 4).rename("trunk/a", "y/q");
  use MergeConflictClass::{CreateCreate, DeleteModify, TypeChanged};
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
    (one_copy("t2", "gone", 5), 3, vec![("gone", CreateCreate)]),
    (both, 5, vec![("y", TypeChanged)]),
    (nested, 5, vec![("y/a", CreateCreate)]),
    (moved, 5, vec![("y/q", TypeChanged)]),
    (
      replace_copy("t2", "taken", 5),
      1,
      vec![("taken", DeleteModify)],
    ),
    (replace_copy("t2", "h", 5), 5, vec![("h", TypeChanged)]),
    (
      replace_copy("t2", "trunk/a", 5),
      5,
      vec![("trunk/a", TypeChanged)],
    ),
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

/// A-586 with A-596 (a review comment on a copied file). Do: hold `d/x`, remove it, copy a tree holding `x` to `d`,
/// commit another file, then follow a comment on the copied file's first line from the copy's version to the head, and
/// one on the removed file's line from version 1. Expect: the first moves, its lines unchanged, though the path's own
/// history ends in the removal; the second is outdated by the removal at version 2.
#[test]
fn a_comment_on_a_copied_file_moves_with_it() {
  let mut green = Green::new();
  let mut first = Work::default();
  first.create("d/x", b"old\n").create("t/x", b"one\ntwo\n");
  accept(&mut green, &first, 1);
  accept(&mut green, Work::default().unlink("d/x"), 2);
  accept(&mut green, Work::exclusive().copy("t", "d", 2), 3);
  accept(&mut green, Work::default().create("e", b"e"), 4);
  let line = Range::new(0, 4);
  assert_eq!(
    green.reanchor("d/x", line, (3, 4)),
    Ok(Reanchored::Moved(line))
  );
  assert_eq!(
    green.reanchor("d/x", line, (1, 4)),
    Ok(Reanchored::Outdated {
      at: 2,
      by: OutdatedBy::FileReplaced
    })
  );
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
        range: Range::new(0, 0),
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

/// A green at version 2 holding the trunk and a branch `b` of its own: a file named as the trunk's is, a file and a
/// directory the trunk has none of, the directory's mode, and the branch's property.
fn trunk_and_branch() -> Green {
  let mut green = trunk();
  let mut own = Work::default();
  own
    .mkdir("b")
    .xattr("b", PROPS, b"pb")
    .create("b/a", b"mine")
    .create("b/only", b"only")
    .mkdir("b/sub")
    .mode("b/sub", 0o700);
  assert_eq!(accept(&mut green, &own, 2), 2);
  green
}

/// One node-exclusive copy of `from` at `version` to `to` that replaces what `to` holds (A-626).
fn replace_copy(from: &str, to: &str, version: u64) -> Work {
  let mut work = Work::exclusive();
  work.replace(from, to, version);
  work
}

/// Asserts that `green` holds the trunk's tree of [`trunk`] at `at`.
fn assert_holds_the_trunk_at(green: &Green, at: &str) {
  let path = |rel: &str| joined(at, rel);
  assert_eq!(green.content(&path("a")), Some(&b"alpha"[..]));
  assert_eq!(green.content(&path("c")), Some(&b"gamma"[..]));
  assert_eq!(green.content(&path("d/b")), Some(&b"beta"[..]));
  assert!(green.is_dir(at) && green.is_dir(&path("d")));
  assert_eq!(green.mode(at), Some(0o755));
  assert_eq!(green.mode(&path("a")), Some(0o644));
  assert_eq!(green.xattr(at, PROPS), Some(&b"p1"[..]));
  assert_eq!(green.xattr(&path("d/b"), PROPS), Some(&b"p2"[..]));
}

/// Asserts that `green` read at version 2 holds the branch `b` of [`trunk_and_branch`], byte for byte.
fn assert_holds_the_branch_at_2(green: &Green) {
  assert_eq!(green.content_at("b/a", 2), Some(b"mine".to_vec()));
  assert_eq!(green.content_at("b/only", 2), Some(b"only".to_vec()));
  let before = green.base_at(2);
  assert!(before.dirs.contains(&"b/sub".to_owned()));
  assert!(before.modes.contains(&("b/sub".to_owned(), 0o700)));
  assert!(
    before
      .xattrs
      .contains(&("b".to_owned(), PROPS.to_owned(), b"pb".to_vec()))
  );
  assert!(!before.files.iter().any(|(path, _)| path == "b/c"));
}

/// Replays `increment`'s chain entry (`applied`, encoded and decoded) on `replica`, which holds the version before it,
/// and asserts it reads at every version and hashes as `green`.
fn assert_replays_alike(
  green: &mut Green,
  mut replica: Green,
  increment: &Increment,
  applied: &[chain_entry::Applied],
) {
  let entry = chain_entry::encode(&increment.encode(), increment, applied, None, None, None);
  let decoded = chain_entry::decode(&entry).expect("the entry decodes");
  assert_eq!(decoded.applied, applied);
  assert_eq!(
    replica.apply_recorded(&decoded.increment, &decoded.applied),
    green.head()
  );
  assert_eq!(replica.head_identity(), green.head_identity());
  for version in 0..=green.head() {
    assert_eq!(
      replica.base_at(version),
      green.base_at(version),
      "the replay at {version}"
    );
  }
}

/// A-626 (Subversion's replace with history, §4.17.4 item 3.5: a copy onto a path its own commit removes). Do: replace
/// the branch `b` of [`trunk_and_branch`] with the trunk at version 1, node-exclusively, then replay that version's
/// chain entry on a green holding the version before. Expect: accepted as version 3; `b` holds the trunk's tree and
/// nothing of its own (`b/only`, `b/sub` and its mode gone, `b/a` the trunk's bytes, `b`'s property the trunk's); at
/// version 2 its own tree, byte for byte; the change index names `b` and every path the replace took away; no map
/// refused the copy; the identity equals its recount and the change index's bytes theirs; and the replica reads at
/// every version and hashes alike.
#[test]
fn a_replace_takes_its_destination_away_and_copies_the_source_there() {
  let mut green = trunk_and_branch();
  let increment = replace_copy("trunk", "b", 1).at(3, 2);
  let (outcome, applied) = green.submit_resolved(&increment);
  assert_eq!(outcome, Outcome::Accepted { version: 3 });
  assert_holds_the_trunk_at(&green, "b");
  assert_eq!(green.content("b/only"), None);
  assert!(!green.is_dir("b/sub"));
  assert_eq!(green.mode("b/sub"), None);
  assert_holds_the_branch_at_2(&green);
  assert_eq!(green.changed_between(2, 3), ["b", "b/a", "b/only", "b/sub"]);
  assert_eq!(green.copies_refused(), 0);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  assert_eq!(green.changes_bytes(), green.changes_recounted());
  let applied = applied.expect("an accepted version's effects");
  assert_replays_alike(&mut green, trunk_and_branch(), &increment, &applied);
}

/// A-626 (the histories a replace leaves: a key it took away that the copy gave back, changed later; a directory it took
/// away that the copy did not give back, made again later). Do: replace the branch `b` with the trunk at version 3, then
/// at version 4 overwrite `b/a` and make `b/sub` again with another mode. Expect: `b/a` reads the branch's bytes at 2,
/// the trunk's at 3 and the new ones at 4; `b/sub` stands at 2 with its first mode, not at 3, and again at 4 with its
/// new mode; and the identity equals its recount.
#[test]
fn a_key_a_replace_took_away_reads_at_every_version_once_it_changes_again() {
  let mut green = trunk_and_branch();
  accept(&mut green, &replace_copy("trunk", "b", 1), 3);
  accept(
    &mut green,
    Work::exclusive()
      .overwrite("b/a", 0, b"AL")
      .mkdir("b/sub")
      .mode("b/sub", 0o711),
    4,
  );
  let bytes: Vec<Option<Vec<u8>>> = (2..=4)
    .map(|version| green.content_at("b/a", version))
    .collect();
  assert_eq!(
    bytes,
    [
      Some(b"mine".to_vec()),
      Some(b"alpha".to_vec()),
      Some(b"ALpha".to_vec())
    ]
  );
  let stood: Vec<(bool, Option<u32>)> = (2..=4)
    .map(|version| {
      let base = green.base_at(version);
      let mode = base
        .modes
        .iter()
        .find(|(path, _)| path == "b/sub")
        .map(|(_, mode)| *mode);
      (base.dirs.contains(&"b/sub".to_owned()), mode)
    })
    .collect();
  assert_eq!(
    stood,
    [(true, Some(0o700)), (false, None), (true, Some(0o711))]
  );
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
}

/// A green at version 2 holding the trunk and `t2`, a second tree: a directory holding a file and a directory with a
/// file.
fn trunk_and_t2() -> Green {
  let mut green = trunk();
  let mut work = Work::default();
  work
    .mkdir("t2")
    .create("t2/f", b"f2")
    .mkdir("t2/e")
    .create("t2/e/g", b"g2");
  assert_eq!(accept(&mut green, &work, 2), 2);
  green
}

/// Asserts that `green` holds `t2`'s tree of [`trunk_and_t2`] at `at`.
fn assert_holds_t2_at(green: &Green, at: &str) {
  assert!(green.is_dir(at) && green.is_dir(&joined(at, "e")));
  assert_eq!(green.content(&joined(at, "f")), Some(&b"f2"[..]));
  assert_eq!(green.content(&joined(at, "e/g")), Some(&b"g2"[..]));
}

/// A-626 (copies into a copied tree in one commit: Subversion's second copy into a new branch, and its move inside one, a
/// copy and a removal). Do: in one node-exclusive increment, copy the trunk at version 2 to `b`, `t2` into `b/new` (a
/// place the trunk's tree leaves empty), `t2` over `b/d` replacing it (a place the trunk's tree fills), and move `b/a` to
/// `b/moved` as a stock client does (the trunk's `a` copied there, `b/a` removed); then replay that version's chain entry
/// on a green holding the version before. Expect: accepted as version 3; `b/new` and `b/d` hold `t2`'s tree, and `b/d/b`
/// and its property are gone; `b/moved` holds the trunk's `a`, and `b/a` is gone; the rest of `b` is the trunk's;
/// version 2 holds nothing of `b`; the change index names the four destinations, `b/a`, and what the replace took away
/// (`b/d/b`); no map refused a copy; the identity equals its recount; and the replica reads and hashes alike.
#[test]
fn copies_nest_in_one_increment_and_a_move_inside_a_copy_lands() {
  let mut green = trunk_and_t2();
  let mut work = Work::exclusive();
  work
    .copy("trunk", "b", 2)
    .copy("t2", "b/new", 2)
    .replace("t2", "b/d", 2)
    .copy("trunk/a", "b/moved", 2)
    .unlink("b/a");
  let increment = work.at(3, 2);
  let (outcome, applied) = green.submit_resolved(&increment);
  assert_eq!(outcome, Outcome::Accepted { version: 3 });
  assert_holds_t2_at(&green, "b/new");
  assert_holds_t2_at(&green, "b/d");
  assert_eq!(green.content("b/d/b"), None);
  assert_eq!(green.xattr("b/d/b", PROPS), None);
  assert_eq!(green.content("b/moved"), Some(&b"alpha"[..]));
  assert_eq!(green.content("b/a"), None);
  assert_eq!(green.content("b/c"), Some(&b"gamma"[..]));
  assert_eq!(green.xattr("b", PROPS), Some(&b"p1"[..]));
  assert!(
    base_paths(&green.base_at(2))
      .iter()
      .all(|path| !in_tree(path, "b"))
  );
  assert_eq!(
    green.changed_between(2, 3),
    ["b", "b/a", "b/d", "b/d/b", "b/moved", "b/new"]
  );
  assert_eq!(green.copies_refused(), 0);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  let applied = applied.expect("an accepted version's effects");
  assert_replays_alike(&mut green, trunk_and_t2(), &increment, &applied);
}

/// A-626 (a replace's charge, secured before its verdict, §4.2 all-cost admission): its history and change-index
/// reservations cover what its commit adds. Do: hold a branch of 64 files, reserve for replacing it with the trunk, and
/// for copying the trunk onto an empty path, then commit the replace. Expect: each of the replace's reservations at
/// least what its total grew by, and more than the plain copy's: the replaced tree is charged.
#[test]
fn a_replaces_reservations_cover_what_its_commit_adds() {
  let mut green = trunk();
  let mut work = Work::default();
  for index in 0..64u32 {
    work.create(&format!("b/{index}"), &index.to_le_bytes());
  }
  accept(&mut green, &work, 2);
  let replace = replace_copy("trunk", "b", 1).at(3, 2);
  let plain = one_copy("trunk", "e", 1).at(4, 2);
  let reserved = (
    green.history_reservation(&replace),
    green.changes_reservation(&replace),
  );
  let alone = (
    green.history_reservation(&plain),
    green.changes_reservation(&plain),
  );
  let before = (green.retained_bytes().history, green.changes_bytes());
  assert_eq!(green.submit(&replace), Outcome::Accepted { version: 3 });
  let grown = (
    green.retained_bytes().history - before.0,
    green.changes_bytes() - before.1,
  );
  assert!(
    grown.0 <= reserved.0 && grown.1 <= reserved.1,
    "grew {grown:?}, reserved {reserved:?}"
  );
  assert!(
    reserved.0 > alone.0 && reserved.1 > alone.1,
    "the replace reserved {reserved:?}, a copy alone {alone:?}"
  );
}

/// [`trunk_and_branch`] with the versions of [`a_branch_before_a_replace_holds_the_tree_it_took_away`] up to `head`:
/// the branch replaced with the trunk at 3, and at 4 the trunk copied to `e` with `b/d` copied into it.
fn replaced_up_to(head: u64) -> Green {
  let mut green = if head >= 2 {
    trunk_and_branch()
  } else {
    trunk()
  };
  if head >= 3 {
    accept(&mut green, &replace_copy("trunk", "b", 1), 3);
  }
  if head >= 4 {
    accept(
      &mut green,
      Work::exclusive()
        .copy("trunk", "e", 3)
        .copy("b/d", "e/x", 3),
      4,
    );
  }
  green
}

/// Asserts that a branch of `up_to(head)` at every version from 1 reads as `up_to(version)` reads at its head and hashes
/// as it, and that making it walks no map.
fn assert_branches_read_as_their_versions(up_to: impl Fn(u64) -> Green, head: u64) {
  let expected: Vec<([u8; 32], Base)> = (1..=head)
    .map(|version| {
      let mut prefix = up_to(version);
      (prefix.head_identity(), sorted(prefix.base_at(version)))
    })
    .collect();
  let (mut lineage, source) = Lineage::new(up_to(head));
  for (version, expected) in (1..=head).zip(expected) {
    let walked = lineage.walked();
    let branch = lineage.branch(source, version).expect("a branch");
    assert_eq!(
      lineage.walked(),
      walked,
      "the branch at {version} walked a map"
    );
    let found = lineage
      .with(branch, |green| {
        (green.head_identity(), sorted(green.base_at(0)))
      })
      .expect("the branch");
    assert_eq!(found, expected, "the branch at {version}");
  }
}

/// A-626 (a green made at another's version, before and after a replace and copies nested in one increment). Do: replace
/// the branch `b` at version 3, copy the trunk to `e` with `b/d` copied into it at 4, and branch the green at every
/// version. Expect: each branch reads as a green that applied the same versions reads at its head, and hashes as it; no
/// branch walks a map.
#[test]
fn a_branch_before_a_replace_holds_the_tree_it_took_away() {
  assert_branches_read_as_their_versions(replaced_up_to, 4);
}

/// A-626, found by its oracle (A-586's branch left a key of a cut copy's tree cut when the copy brought it back with the
/// bytes it held before). Do: hold `d/x` and `t/x` with the same bytes, remove `d/x`, copy `t` to `d`, and branch the
/// green at every version. Expect: each branch reads as a green that applied the same versions (`d/x` there at 1, gone
/// at 2, there at 3) and hashes as it.
#[test]
fn a_branch_before_a_copy_sets_back_a_key_it_brought_back_unchanged() {
  let up_to = |head: u64| {
    let mut green = Green::new();
    accept(
      &mut green,
      Work::default()
        .create("d/x", b"same")
        .create("t/x", b"same"),
      1,
    );
    if head >= 2 {
      accept(&mut green, Work::default().unlink("d/x"), 2);
    }
    if head >= 3 {
      accept(&mut green, &one_copy("t", "d", 2), 3);
    }
    green
  };
  assert_branches_read_as_their_versions(up_to, 3);
}

/// A-626, found while fixing the above (A-586's branch read a key at its version from the key's own history alone,
/// before the copy that gave it its value, and gave back no copy's entry for a key whose history ended before the copy).
/// Do: make `d` with a mode, remove it (its mode with it, A-618), copy `t` (another mode) to `d`, then set a property
/// on `d`, and branch the green at every version. Expect: each branch reads as a green that applied the same versions
/// (the branch at 3 holds `d` with the copy's mode) and hashes as it.
#[test]
fn a_branch_reads_a_key_through_the_copy_that_gave_it_its_value() {
  let up_to = |head: u64| {
    let mut green = Green::new();
    accept(
      &mut green,
      Work::default()
        .mkdir("d")
        .mode("d", 0o700)
        .mkdir("t")
        .mode("t", 0o755)
        .create("t/f", b"f"),
      1,
    );
    if head >= 2 {
      accept(&mut green, Work::default().rmdir("d"), 2);
    }
    if head >= 3 {
      accept(&mut green, &one_copy("t", "d", 2), 3);
    }
    if head >= 4 {
      accept(&mut green, Work::default().xattr("d", PROPS, b"p"), 4);
    }
    green
  };
  assert_branches_read_as_their_versions(up_to, 4);
}

/// A green at version 3 whose trunk changed after version 1 in every dimension it holds: `a` overwritten and its mode
/// changed, `c` removed, `new` made and `d/b`'s property changed at 2; `d` given a property and `e` made at 3.
fn trunk_changed_after_1() -> Green {
  let mut green = trunk();
  let mut second = Work::default();
  second
    .overwrite("trunk/a", 0, b"AL")
    .unlink("trunk/c")
    .create("trunk/new", b"new")
    .xattr("trunk/d/b", PROPS, b"q2")
    .mode("trunk/a", 0o600);
  assert_eq!(accept(&mut green, &second, 2), 2);
  let mut third = Work::default();
  third.xattr("trunk/d", PROPS, b"pd").mkdir("trunk/e");
  assert_eq!(accept(&mut green, &third, 3), 3);
  green
}

/// What `base` holds of `root`'s tree, every path moved to its place in `to`'s, in [`sorted`]'s order.
fn tree_moved(base: &Base, root: &str, to: &str) -> Base {
  let place = |path: &str| relative(path, root).map(|rel| joined(to, rel));
  sorted(Base {
    files: base
      .files
      .iter()
      .filter_map(|(path, len)| Some((place(path)?, *len)))
      .collect(),
    specials: Vec::new(),
    dirs: base.dirs.iter().filter_map(|path| place(path)).collect(),
    modes: base
      .modes
      .iter()
      .filter_map(|(path, mode)| Some((place(path)?, *mode)))
      .collect(),
    symlinks: base
      .symlinks
      .iter()
      .filter_map(|(path, target)| Some((place(path)?, target.clone())))
      .collect(),
    xattrs: base
      .xattrs
      .iter()
      .filter_map(|(path, name, value)| Some((place(path)?, name.clone(), value.clone())))
      .collect(),
    hardlinks: Vec::new(),
  })
}

/// Submits `work` at `green`'s head as increment `id`, which must be accepted: the increment and what its entry
/// records.
fn submit_recorded(
  green: &mut Green,
  work: &Work,
  id: u64,
) -> (Increment, Vec<chain_entry::Applied>) {
  let increment = work.at(id, green.head());
  let (outcome, applied) = green.submit_resolved(&increment);
  assert!(
    matches!(outcome, Outcome::Accepted { .. }),
    "increment {id}: {outcome:?}"
  );
  (increment, applied.expect("an accepted version's effects"))
}

/// Replays `entries`, each encoded and decoded, on `replica`, and asserts it reads at every version and hashes as
/// `green`.
fn assert_replay_reads_alike(
  green: &mut Green,
  mut replica: Green,
  entries: &[(Increment, Vec<chain_entry::Applied>)],
) {
  for (increment, applied) in entries {
    let entry = chain_entry::encode(&increment.encode(), increment, applied, None, None, None);
    let decoded = chain_entry::decode(&entry).expect("the entry decodes");
    assert_eq!(&decoded.applied, applied);
    replica.apply_recorded(&decoded.increment, &decoded.applied);
  }
  assert_eq!(replica.head(), green.head());
  for version in 0..=green.head() {
    assert_eq!(
      replica.base_at(version),
      green.base_at(version),
      "the replay at {version}"
    );
  }
  assert_eq!(replica.head_identity(), green.head_identity());
}

/// Asserts that `applied` records a fix, that `room` (the green's, derived before the verdict) is its size, and that
/// the encoded entry fits the increment's own bound plus `room`.
fn assert_fits_its_room(increment: &Increment, applied: &[chain_entry::Applied], room: usize) {
  let fixes: usize = applied
    .iter()
    .filter_map(|applied| match applied {
      chain_entry::Applied::CopyTree { fix: Some(fix), .. } => Some(chain_entry::fix_bytes(fix)),
      _ => None,
    })
    .sum();
  assert!(fixes > 0, "no fix recorded");
  assert_eq!(room, fixes);
  let bytes = increment.encode();
  let entry = chain_entry::encode(&bytes, increment, applied, None, None, None);
  let bound = chain_entry::entry_bound(bytes.len(), increment) + room;
  assert!(entry.len() <= bound, "{} bytes past {bound}", entry.len());
}

/// A-626 part 2 (a copy from an older version of a source that changed since, as FSFS accepts it). Do: copy the trunk
/// at version 1 to `b` after versions 2 and 3 changed it in every dimension, and replay that version's chain entry on
/// a green holding the version before. Expect: accepted as version 4; `b` holds the trunk as it stood at version 1 (the
/// trunk's base at 1 moved to `b`) and the trunk keeps its head; the change index names `b` alone; no map refused the
/// copy; the identity equals its recount; the entry records the fix within the room the green derived for it before
/// the verdict, and a copy of an unchanged source needs none; and the replica reads and hashes alike.
#[test]
fn a_copy_from_an_older_version_brings_the_source_as_it_stood() {
  let mut green = trunk_changed_after_1();
  let increment = one_copy("trunk", "b", 1).at(4, 3);
  let room = green.copy_entry_room(&increment);
  let (outcome, applied) = green.submit_resolved(&increment);
  assert_eq!(outcome, Outcome::Accepted { version: 4 });
  assert_holds_the_trunk_at(&green, "b");
  assert_eq!(
    tree_moved(&green.base_at(1), "trunk", "b"),
    tree_moved(&green.base_at(4), "b", "b")
  );
  assert_eq!(green.content("trunk/a"), Some(&b"ALpha"[..]));
  assert_eq!(green.changed_between(3, 4), ["b"]);
  assert_eq!(green.copies_refused(), 0);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  let applied = applied.expect("an accepted version's effects");
  assert_fits_its_room(&increment, &applied, room);
  assert_eq!(
    green.copy_entry_room(&one_copy("trunk", "z", 3).at(9, 4)),
    0
  );
  assert_replays_alike(&mut green, trunk_changed_after_1(), &increment, &applied);
}

/// A-626 part 2 (copies from before a copy inside the source and from before one over it). Do: on [`trunk_and_t2`],
/// copy `t2` to `trunk/t` (version 3), the trunk at version 2 to `b` (4), replace the trunk with `t2` (5), and copy the
/// trunk at version 4 to `c` (6); replay every one of those entries on a green holding version 2. Expect: each copy
/// holds the trunk as the green reads it at the copy's version (`b` without `t`, the inner copy cut; `c` the trunk
/// before its replace, `t` among it); the identity equals its recount; the replica reads at every version and hashes
/// alike.
#[test]
fn copies_from_before_a_copy_inside_or_over_the_source_bring_it_as_it_stood() {
  let mut green = trunk_and_t2();
  let entries = vec![
    submit_recorded(&mut green, &one_copy("t2", "trunk/t", 2), 3),
    submit_recorded(&mut green, &one_copy("trunk", "b", 2), 4),
    submit_recorded(&mut green, &replace_copy("t2", "trunk", 2), 5),
    submit_recorded(&mut green, &one_copy("trunk", "c", 4), 6),
  ];
  assert_eq!(
    tree_moved(&green.base_at(2), "trunk", "b"),
    tree_moved(&green.base_at(6), "b", "b")
  );
  assert_eq!(
    tree_moved(&green.base_at(4), "trunk", "c"),
    tree_moved(&green.base_at(6), "c", "c")
  );
  assert_eq!(green.content("c/t/f"), Some(&b"f2"[..]));
  assert_eq!(green.content("b/t/f"), None);
  assert_eq!(green.copies_refused(), 0);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  assert_replay_reads_alike(&mut green, trunk_and_t2(), &entries);
}

/// A-626 part 2 (an edit beneath a copy from an older version in the copy's own increment: a working copy's commit of
/// `svn cp -r 1 trunk b` with an edit). Do: in one increment copy the trunk at version 1 to `b` and overwrite `b/a` at
/// its fourth byte, after version 2 changed `trunk/a`'s first two; replay the entry on a green holding the version
/// before. Expect: `b/a` holds the edit spliced into the bytes `trunk/a` held at 1 (`alpHA`, not the head's `ALpHA`),
/// the identity equals its recount, and the replica reads and hashes alike.
#[test]
fn an_edit_beneath_a_copy_from_an_older_version_lands_on_the_bytes_it_brings() {
  let mut green = trunk_changed_after_1();
  let mut work = Work::exclusive();
  work.copy("trunk", "b", 1).overwrite("b/a", 3, b"HA");
  let increment = work.at(4, 3);
  let (outcome, applied) = green.submit_resolved(&increment);
  assert_eq!(outcome, Outcome::Accepted { version: 4 });
  assert_eq!(green.content("b/a"), Some(&b"alpHA"[..]));
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  let applied = applied.expect("an accepted version's effects");
  assert_replays_alike(&mut green, trunk_changed_after_1(), &increment, &applied);
}

/// A-626 part 2 (a copy from a version whose values are folded away). Do: fold the green's histories below version 3,
/// then copy the trunk at version 1 (changed at 2, so its values then are needed) and at version 3 (unchanged since).
/// Expect: the first a type conflict naming its destination, the green unchanged; the second accepted.
#[test]
fn a_copy_from_a_folded_version_is_refused_typed() {
  let mut green = trunk_changed_after_1();
  green.fold_history_before(3);
  let head = green.head();
  let stale = green.submit(&one_copy("trunk", "b", 1).at(4, head));
  assert_eq!(
    windows(&stale),
    Some(vec![("b".to_owned(), MergeConflictClass::TypeChanged)])
  );
  assert_eq!(green.head(), head);
  assert_eq!(
    green.submit(&one_copy("trunk", "c", 3).at(5, head)),
    Outcome::Accepted { version: 4 }
  );
}

/// The attribute every file of the oracle's histories carries.
const NAME: &str = "n";

/// The paths the oracle's histories name: the root (as a copy's source only) first, then paths that are prefixes of one
/// another, siblings sharing a first byte, and paths often empty, so copies land and later steps change their trees.
const PATHS: [&str; 11] = [
  "", "a", "a/x", "a/x/y", "a/b", "b", "b/x", "ab", "c", "c/x", "d",
];

/// Where beneath a copy's destination a second copy of its increment lands (A-626): names the paths' trees hold, and
/// one they never hold.
const BENEATH: [&str; 4] = ["x", "x/y", "b", "m"];

/// Any path of [`PATHS`], the root among them: a copy's source or destination.
fn any_path(index: usize) -> String {
  PATHS
    .get(index % PATHS.len())
    .copied()
    .unwrap_or_default()
    .to_owned()
}

/// A path of [`PATHS`] other than the root: where a file or a directory is put.
fn named_path(index: usize) -> String {
  PATHS
    .get(1 + index % (PATHS.len() - 1))
    .copied()
    .unwrap_or_default()
    .to_owned()
}

/// A place of [`BENEATH`].
fn beneath(index: usize) -> &'static str {
  BENEATH
    .get(index % BENEATH.len())
    .copied()
    .unwrap_or_default()
}

/// One generated step of the oracle (module docs).
#[derive(Clone, Debug)]
enum Step {
  /// A one-byte file and its attribute at a path (an overwrite where a file stands).
  Put { path: usize, byte: u8 },
  /// A file and its attribute taken away.
  Take { path: usize },
  /// A directory with its mode.
  Mkdir { path: usize, mode: u8 },
  /// A tree copied, from the head or a version or two before it, replacing what its destination holds or not (A-626).
  Copy {
    from: usize,
    to: usize,
    back: u8,
    replace: bool,
  },
  /// Two copies in one increment, the second landing beneath the first's destination, each replacing or not (A-626).
  Nested {
    from: usize,
    to: usize,
    inner: usize,
    at: usize,
    replace: (bool, bool),
  },
  /// A tree copied with one of the files it brings moved within it as a stock client moves one: the file's source copied
  /// to its new place, and the file removed (A-626).
  Move {
    from: usize,
    to: usize,
    file: usize,
    at: usize,
  },
}

/// The steps, weighted by a selector over plain tuples (`prop_oneof!`'s strategy is shared-ownership backed, R2).
fn step() -> impl Strategy<Value = Step> {
  (
    0u8..16,
    (
      any::<usize>(),
      any::<usize>(),
      any::<usize>(),
      any::<usize>(),
    ),
    (any::<u8>(), any::<bool>(), any::<bool>()),
  )
    .prop_map(
      |(selector, (first, second, third, fourth), (byte, flag, other))| match selector {
        0..4 => Step::Put { path: first, byte },
        4 => Step::Take { path: first },
        5 => Step::Mkdir {
          path: first,
          mode: byte,
        },
        6..11 => Step::Copy {
          from: first,
          to: second,
          back: byte,
          replace: flag,
        },
        11..14 => Step::Nested {
          from: first,
          to: second,
          inner: third,
          at: fourth,
          replace: (flag, other),
        },
        _ => Step::Move {
          from: first,
          to: second,
          file: third,
          at: fourth,
        },
      },
    )
}

/// What the generated histories met, so the oracle shows it compared real copies.
#[derive(Clone, Copy, Debug, Default)]
struct Census {
  accepted: usize,
  refused: usize,
  from_root: usize,
  from_before: usize,
  into_themselves: usize,
  /// Copies accepted from a version their source changed after (A-626 part 2).
  from_changed: usize,
  /// Puts and takes of a path a copy's tree holds: a key's change after its copy.
  changed_beneath: usize,
  /// Replaces accepted over a destination that held something (A-626).
  replaced: usize,
  /// Increments of two nested copies accepted (A-626).
  nested: usize,
  /// Of those, ones whose inner copy replaced what the outer brought there.
  nested_replaced: usize,
  /// Moves accepted inside a copied tree (A-626).
  moved: usize,
}

impl Census {
  /// Counts one single copy's outcome: `held`, whether its destination held anything; `changed`, whether its source's
  /// tree changed after its version.
  fn count(&mut self, copy: &Declared, accepted: bool, (head, held, changed): (u64, bool, bool)) {
    if !accepted {
      self.refused += 1;
      return;
    }
    self.from_changed += usize::from(changed);
    self.accepted += 1;
    self.from_root += usize::from(copy.src.is_empty());
    self.from_before += usize::from(copy.version < head);
    self.into_themselves += usize::from(in_tree(&copy.dst, &copy.src));
    self.replaced += usize::from(copy.replace && held);
  }
}

/// A copy one step declares: its destination, its source, the source's version, and whether it replaces (A-626).
#[derive(Clone, Debug)]
struct Declared {
  dst: String,
  src: String,
  version: u64,
  replace: bool,
}

/// One version's tree in the oracle's model: what a green reads at it.
#[derive(Clone, Debug, Default)]
struct State {
  files: BTreeMap<String, Vec<u8>>,
  dirs: BTreeSet<String>,
  modes: BTreeMap<String, u32>,
  xattrs: BTreeMap<(String, String), Vec<u8>>,
}

impl State {
  /// Every path a key is at.
  fn paths(&self) -> BTreeSet<String> {
    self
      .files
      .keys()
      .chain(&self.dirs)
      .chain(self.modes.keys())
      .chain(self.xattrs.keys().map(|(path, _)| path))
      .cloned()
      .collect()
  }

  /// Whether a key lies in `root`'s tree.
  fn holds_tree(&self, root: &str) -> bool {
    self.paths().iter().any(|path| in_tree(path, root))
  }

  /// `root`'s tree, every key moved to its place in `to`'s.
  fn tree_at(&self, root: &str, to: &str) -> State {
    let place = |path: &str| relative(path, root).map(|rel| joined(to, rel));
    State {
      files: self
        .files
        .iter()
        .filter_map(|(path, bytes)| Some((place(path)?, bytes.clone())))
        .collect(),
      dirs: self.dirs.iter().filter_map(|path| place(path)).collect(),
      modes: self
        .modes
        .iter()
        .filter_map(|(path, mode)| Some((place(path)?, *mode)))
        .collect(),
      xattrs: self
        .xattrs
        .iter()
        .filter_map(|((path, name), value)| Some(((place(path)?, name.clone()), value.clone())))
        .collect(),
    }
  }

  /// Takes `root`'s tree away; the paths it took.
  fn remove_tree(&mut self, root: &str) -> BTreeSet<String> {
    let taken: BTreeSet<String> = self
      .paths()
      .into_iter()
      .filter(|path| in_tree(path, root))
      .collect();
    self.files.retain(|path, _| !in_tree(path, root));
    self.dirs.retain(|path| !in_tree(path, root));
    self.modes.retain(|path, _| !in_tree(path, root));
    self.xattrs.retain(|(path, _), _| !in_tree(path, root));
    taken
  }

  /// Takes the file at `path` away, with its mode and attributes once nothing stands there (A-618).
  fn unlink(&mut self, path: &str) {
    self.files.remove(path);
    if !self.dirs.contains(path) {
      self.modes.remove(path);
      self.xattrs.retain(|(at, _), _| at != path);
    }
  }

  /// Adds `tree`'s keys.
  fn graft(&mut self, tree: State) {
    self.files.extend(tree.files);
    self.dirs.extend(tree.dirs);
    self.modes.extend(tree.modes);
    self.xattrs.extend(tree.xattrs);
  }

  /// The base a green answers for this tree, in [`sorted`]'s order.
  fn base(&self) -> Base {
    Base {
      files: self
        .files
        .iter()
        .map(|(path, bytes)| (path.clone(), bytes.len() as u64))
        .collect(),
      specials: Vec::new(),
      dirs: self.dirs.iter().cloned().collect(),
      modes: self
        .modes
        .iter()
        .map(|(path, mode)| (path.clone(), *mode))
        .collect(),
      symlinks: Vec::new(),
      xattrs: self
        .xattrs
        .iter()
        .map(|((path, name), value)| (path.clone(), name.clone(), value.clone()))
        .collect(),
      hardlinks: Vec::new(),
    }
  }
}

/// `base` with every list in key order, so a green's and the model's compare whatever order each lists them in.
fn sorted(mut base: Base) -> Base {
  base.files.sort();
  base.specials.sort_by(|a, b| a.0.cmp(&b.0));
  base.dirs.sort();
  base.modes.sort();
  base.symlinks.sort();
  base.xattrs.sort();
  base.hardlinks.sort();
  base
}

/// The copy among `copies` whose tree holds `dst` (another's destination) with the longest destination: the one it
/// lands in.
fn outer_copy<'a>(copies: &'a [Declared], dst: &str) -> Option<&'a Declared> {
  copies
    .iter()
    .filter(|copy| copy.dst != dst && in_tree(dst, &copy.dst))
    .max_by_key(|copy| copy.dst.len())
}

/// What a version lands in the model: its tree, the paths it names in the change index, the destinations it copied to.
type Landed = (State, BTreeSet<String>, BTreeSet<String>);

/// The oracle's model (module docs): every version's tree, the paths each version names in the change index, and the
/// destinations of the copies each made. It states the design's rules, never the engine's code: a copy brings its
/// source's tree as the version it names held it (A-626 part 2), a replace takes its destination's tree away first, copies land in
/// their destinations' order, and a copy stands where Subversion's commit would take it.
struct Model {
  states: Vec<State>,
  named: Vec<BTreeSet<String>>,
  copied: Vec<BTreeSet<String>>,
}

impl Default for Model {
  fn default() -> Model {
    Model {
      states: vec![State::default()],
      named: vec![BTreeSet::new()],
      copied: vec![BTreeSet::new()],
    }
  }
}

impl Model {
  fn head(&self) -> u64 {
    u64::try_from(self.states.len().saturating_sub(1)).unwrap_or(u64::MAX)
  }

  fn state(&self, version: u64) -> Option<&State> {
    self.states.get(usize::try_from(version).ok()?)
  }

  fn current(&self) -> State {
    self.states.last().cloned().unwrap_or_default()
  }

  fn push(&mut self, (state, named, copied): Landed) {
    self.states.push(state);
    self.named.push(named);
    self.copied.push(copied);
  }

  /// The paths the versions in `(from, to]` name.
  fn named_between(&self, from: u64, to: u64) -> BTreeSet<String> {
    let index = |version: u64| usize::try_from(version).unwrap_or(usize::MAX);
    self
      .named
      .iter()
      .take(index(to).saturating_add(1))
      .skip(index(from).saturating_add(1))
      .flatten()
      .cloned()
      .collect()
  }

  /// Whether `root`'s tree changed after `version`: a later version named a path in it, or copied onto it or above it.
  fn changed_after(&self, root: &str, version: u64) -> bool {
    let after = usize::try_from(version).map_or(usize::MAX, |version| version.saturating_add(1));
    let named = self
      .named
      .iter()
      .skip(after)
      .flatten()
      .any(|path| in_tree(path, root));
    let replaced = self
      .copied
      .iter()
      .skip(after)
      .flatten()
      .any(|dst| in_tree(root, dst));
    named || replaced
  }

  /// Whether every copy of one increment stands (based on the head): each destination other than the root holds nothing
  /// or is replaced, a destination beneath another copy of the increment judged against what that copy brings there;
  /// each source holds something and did not change after its version.
  fn copies_stand(&self, copies: &[Declared]) -> bool {
    let head = self.current();
    let held_at = |version: u64, root: &str| {
      self
        .state(version)
        .is_some_and(|state| state.holds_tree(root))
    };
    copies.iter().all(|copy| {
      let filled = match outer_copy(copies, &copy.dst) {
        None => head.holds_tree(&copy.dst),
        Some(outer) => {
          let rel = relative(&copy.dst, &outer.dst).unwrap_or_default();
          held_at(outer.version, &joined(&outer.src, rel))
        }
      };
      let destination = !copy.dst.is_empty() && (copy.replace || !filled);
      destination && held_at(copy.version, &copy.src)
    })
  }

  /// What one increment of `copies` and the removals `unlinked` in their trees lands: every source's tree read at its
  /// version first (FSFS copies from any committed revision, A-626 part 2), then in their destinations' order each replaced destination's tree taken away and each source's tree put
  /// there, then the removals.
  fn after_copies(&self, copies: &[Declared], unlinked: &[String]) -> Landed {
    let mut next = self.current();
    let mut order: Vec<&Declared> = copies.iter().collect();
    order.sort_by(|a, b| a.dst.cmp(&b.dst));
    let sources: Vec<State> = order
      .iter()
      .map(|copy| {
        self
          .state(copy.version)
          .map(|state| state.tree_at(&copy.src, &copy.dst))
          .unwrap_or_default()
      })
      .collect();
    let mut named: BTreeSet<String> = order.iter().map(|copy| copy.dst.clone()).collect();
    let copied = named.clone();
    for (copy, source) in order.into_iter().zip(sources) {
      if copy.replace {
        named.extend(next.remove_tree(&copy.dst));
      }
      next.graft(source);
    }
    for path in unlinked {
      next.unlink(path);
      named.insert(path.clone());
    }
    (next, named, copied)
  }
}

/// The green under test, the model it is held to, and what each accepted version applied.
#[derive(Default)]
struct Oracle {
  green: Green,
  model: Model,
  entries: Vec<(Increment, Vec<chain_entry::Applied>)>,
  next: u64,
}

impl Oracle {
  /// Applies one step to the green, which must decide it as the model does.
  fn apply(&mut self, step: &Step, census: &mut Census) -> Result<(), TestCaseError> {
    match *step {
      Step::Put { path, byte } => self.put(&named_path(path), byte, census),
      Step::Take { path } => self.take(&named_path(path), census),
      Step::Mkdir { path, mode } => self.mkdir(&named_path(path), u32::from(mode)),
      Step::Copy {
        from,
        to,
        back,
        replace,
      } => self.copy_step((from, to), back, replace, census),
      Step::Nested {
        from,
        to,
        inner,
        at,
        replace,
      } => self.nested_step((from, to), (inner, at), replace, census),
      Step::Move { from, to, file, at } => self.move_step((from, to), (file, at), census),
    }
  }

  /// Submits `work` at the head, which the model expects accepted with what it lands (`Some`) or refused; the green must
  /// agree, and an accepted version's entry is kept for the replay. Whether it was accepted.
  fn submit(&mut self, work: &Work, expected: Option<Landed>) -> Result<bool, TestCaseError> {
    self.next += 1;
    let increment = work.at(self.next, self.green.head());
    let (outcome, applied) = self.green.submit_resolved(&increment);
    let accepted = matches!(outcome, Outcome::Accepted { .. });
    prop_assert_eq!(accepted, expected.is_some(), "{:?}", outcome);
    if let (Some(applied), Some(landed)) = (applied, expected) {
      self.entries.push((increment, applied));
      self.model.push(landed);
    }
    Ok(accepted)
  }

  /// A one-byte file at `path` and its attribute: an overwrite where a file stands, refused where a directory does.
  fn put(&mut self, path: &str, byte: u8, census: &mut Census) -> Result<(), TestCaseError> {
    census.changed_beneath += usize::from(self.green.copy_above(path).is_some());
    let mut next = self.model.current();
    let stands = next.files.contains_key(path) || !next.dirs.contains(path);
    let mut work = Work::default();
    let bytes = match next.files.get(path) {
      Some(old) => {
        work.overwrite(path, 0, &[byte]);
        let mut bytes = old.clone();
        bytes.splice(..1.min(bytes.len()), [byte]);
        bytes
      }
      None => {
        work.create(path, &[byte]);
        vec![byte]
      }
    };
    work.xattr(path, NAME, &[byte]);
    next.files.insert(path.to_owned(), bytes);
    next
      .xattrs
      .insert((path.to_owned(), NAME.to_owned()), vec![byte]);
    let named = BTreeSet::from([path.to_owned()]);
    self
      .submit(&work, stands.then_some((next, named, BTreeSet::new())))
      .map(drop)
  }

  /// The file at `path` and its attribute taken away, where a file stands.
  fn take(&mut self, path: &str, census: &mut Census) -> Result<(), TestCaseError> {
    let mut next = self.model.current();
    if !next.files.contains_key(path) {
      return Ok(());
    }
    census.changed_beneath += usize::from(self.green.copy_above(path).is_some());
    let mut work = Work::default();
    work.unlink(path);
    if next
      .xattrs
      .contains_key(&(path.to_owned(), NAME.to_owned()))
    {
      work.remove_xattr(path, NAME);
    }
    next.unlink(path);
    let named = BTreeSet::from([path.to_owned()]);
    self
      .submit(&work, Some((next, named, BTreeSet::new())))
      .map(drop)
  }

  /// A directory at `path` with `mode`, refused where a file stands.
  fn mkdir(&mut self, path: &str, mode: u32) -> Result<(), TestCaseError> {
    let mut next = self.model.current();
    let stands = !next.files.contains_key(path);
    next.dirs.insert(path.to_owned());
    next.modes.insert(path.to_owned(), mode);
    let mut work = Work::default();
    work.mkdir(path).mode(path, mode);
    let named = BTreeSet::from([path.to_owned()]);
    self
      .submit(&work, stands.then_some((next, named, BTreeSet::new())))
      .map(drop)
  }

  /// One increment of `copies` and the removals `unlinked` in their trees, node-exclusively: the model decides and lands
  /// it, and the green must agree; an increment that replaces nothing walks no map. Whether it was accepted.
  fn copies(&mut self, copies: &[Declared], unlinked: &[String]) -> Result<bool, TestCaseError> {
    let mut work = Work::exclusive();
    for copy in copies {
      work.declared(copy);
    }
    for path in unlinked {
      work.unlink(path).remove_xattr(path, NAME);
    }
    let expected = self
      .model
      .copies_stand(copies)
      .then(|| self.model.after_copies(copies, unlinked));
    let walked = self.green.walked();
    let accepted = self.submit(&work, expected)?;
    if accepted && copies.iter().all(|copy| !copy.replace) {
      prop_assert_eq!(self.green.walked(), walked, "a copy walked a map");
    }
    Ok(accepted)
  }

  /// A copy of `from` at the head less `back % 3` to `to`, replacing or not.
  fn copy_step(
    &mut self,
    (from, to): (usize, usize),
    back: u8,
    replace: bool,
    census: &mut Census,
  ) -> Result<(), TestCaseError> {
    let head = self.model.head();
    let copy = Declared {
      dst: any_path(to),
      src: any_path(from),
      version: head.saturating_sub(u64::from(back % 3)),
      replace,
    };
    let held = self.model.current().holds_tree(&copy.dst);
    let changed = self.model.changed_after(&copy.src, copy.version);
    let accepted = self.copies(std::slice::from_ref(&copy), &[])?;
    census.count(&copy, accepted, (head, held, changed));
    Ok(())
  }

  /// A copy of `from` to `to` with a second copy, of `inner`, beneath it at `at`, both of the head's version.
  fn nested_step(
    &mut self,
    (from, to): (usize, usize),
    (inner, at): (usize, usize),
    replace: (bool, bool),
    census: &mut Census,
  ) -> Result<(), TestCaseError> {
    let head = self.model.head();
    let outer = Declared {
      dst: any_path(to),
      src: any_path(from),
      version: head,
      replace: replace.0,
    };
    let place = beneath(at);
    let filled = self.model.current().holds_tree(&joined(&outer.src, place));
    let inner = Declared {
      dst: joined(&outer.dst, place),
      src: any_path(inner),
      version: head,
      replace: replace.1,
    };
    let accepted = self.copies(&[outer, inner], &[])?;
    census.nested += usize::from(accepted);
    census.nested_replaced += usize::from(accepted && filled && replace.1);
    Ok(())
  }

  /// A copy of `from` to `to` with one of the files it brings moved to `at` beneath it, as a stock client moves one: the
  /// file's source copied there, the file removed. Nothing where `from`'s tree holds no file.
  fn move_step(
    &mut self,
    (from, to): (usize, usize),
    (file, at): (usize, usize),
    census: &mut Census,
  ) -> Result<(), TestCaseError> {
    let head = self.model.head();
    let (src, dst) = (any_path(from), any_path(to));
    let state = self.model.current();
    let files: Vec<&String> = state
      .files
      .keys()
      .filter(|path| in_tree(path, &src))
      .collect();
    let Some(moved) = files.get(file % files.len().max(1)).copied() else {
      return Ok(());
    };
    let unlinked = joined(&dst, relative(moved, &src).unwrap_or_default());
    let copies = [
      Declared {
        dst: dst.clone(),
        src: src.clone(),
        version: head,
        replace: false,
      },
      Declared {
        dst: joined(&dst, beneath(at)),
        src: moved.clone(),
        version: head,
        replace: false,
      },
    ];
    let accepted = self.copies(&copies, &[unlinked])?;
    census.moved += usize::from(accepted);
    Ok(())
  }

  /// Holds the green to the model at every version (each base, each file's bytes, the paths named between it and the
  /// head), its identity to its recount, its change index's bytes to theirs, and no copy refused by a map its verdict
  /// passed.
  fn check(&mut self) -> Result<(), TestCaseError> {
    let head = self.green.head();
    prop_assert_eq!(head, self.model.head());
    for version in 0..=head {
      self.check_version(version, head)?;
    }
    prop_assert_eq!(
      self.green.head_identity(),
      self.green.head_identity_from_scratch()
    );
    prop_assert_eq!(self.green.changes_bytes(), self.green.changes_recounted());
    prop_assert_eq!(self.green.copies_refused(), 0);
    Ok(())
  }

  /// [`Oracle::check`] at one version.
  fn check_version(&self, version: u64, head: u64) -> Result<(), TestCaseError> {
    let state = self
      .model
      .state(version)
      .ok_or_else(|| TestCaseError::fail("a version the model lacks"))?;
    prop_assert_eq!(
      sorted(self.green.base_at(version)),
      state.base(),
      "the base at {}",
      version
    );
    for (path, bytes) in &state.files {
      prop_assert_eq!(
        self.green.content_at(path, version),
        Some(bytes.clone()),
        "{} at {}",
        path,
        version
      );
    }
    let named: BTreeSet<String> = self
      .green
      .changed_between(version, head)
      .into_iter()
      .collect();
    prop_assert_eq!(
      named,
      self.model.named_between(version, head),
      "the paths named since {}",
      version
    );
    Ok(())
  }

  /// Every accepted version's chain entry, encoded and decoded, replayed on a fresh green; with the replica's identity at
  /// each version.
  fn replay(&self) -> Result<(Green, Vec<[u8; 32]>), TestCaseError> {
    let mut replica = Green::new();
    let mut identities = vec![replica.head_identity()];
    for (increment, applied) in &self.entries {
      let entry = chain_entry::encode(&increment.encode(), increment, applied, None, None, None);
      let decoded =
        chain_entry::decode(&entry).map_err(|error| TestCaseError::fail(format!("{error:?}")))?;
      replica.apply_recorded(&decoded.increment, &decoded.applied);
      identities.push(replica.head_identity());
    }
    Ok((replica, identities))
  }

  /// The replay must read as the green at every version and hash as it; then a branch of the green at every version
  /// must read as the model's tree there and hash as the replay did, and making it may walk no map.
  fn replay_and_branch(mut self) -> Result<(), TestCaseError> {
    let (replica, identities) = self.replay()?;
    let head = self.green.head();
    for version in 0..=head {
      prop_assert_eq!(
        replica.base_at(version),
        self.green.base_at(version),
        "the replay at {}",
        version
      );
    }
    prop_assert_eq!(identities.last().copied(), Some(self.green.head_identity()));
    let Oracle { green, model, .. } = self;
    let (mut lineage, source) = Lineage::new(green);
    let refused = |refused: super::LineageRefused| TestCaseError::fail(format!("{refused:?}"));
    for (version, identity) in (0..=head).zip(identities) {
      // The branch alone walks nothing; reading its base after is a walk by design.
      let walked = lineage.walked();
      let branch = lineage.branch(source, version).map_err(refused)?;
      prop_assert_eq!(
        lineage.walked(),
        walked,
        "the branch at {} walked a map",
        version
      );
      let (base, hashed) = lineage
        .with(branch, |green| (green.base_at(0), green.head_identity()))
        .map_err(refused)?;
      let state = model
        .state(version)
        .ok_or_else(|| TestCaseError::fail("a version the model lacks"))?;
      prop_assert_eq!(sorted(base), state.base(), "the branch at {}", version);
      prop_assert_eq!(hashed, identity, "the branch's identity at {}", version);
    }
    Ok(())
  }
}

/// Runs one history, checking after every step, then replays the green's chain and branches it at every version.
fn run(steps: &[Step], census: &mut Census) -> Result<(), TestCaseError> {
  let mut oracle = Oracle::default();
  for step in steps {
    oracle.apply(step, census)?;
    oracle.check()?;
  }
  oracle.replay_and_branch()
}

/// A-586 and A-626 (the oracle). Do: generated histories of files and their attributes put and taken, directories made
/// with modes, single copies of any path (the root among them) at the head or a version or two before it, its tree
/// changed since or not (A-626 part 2), to any path (into the source's own tree among them), replacing what the destination holds or not, two copies nested in one
/// increment, and moves inside a copied tree as a stock client makes them, on a green and on a model that states every
/// version's tree. Expect: the green decides every step as the model does; after every step it reads as the model at
/// every version (each base, each file's bytes, the paths the change index names since), its identity equals its
/// recount and its change index's bytes theirs, no map refused a copy its verdict passed, and a copy that replaces
/// nothing walked no map; at the end its chain, replayed from its entries, reads and hashes as it does, and a branch at
/// every version reads as the model and hashes as the replay, walking no map. The census shows each kind of copy met.
#[test]
fn every_copy_reads_as_its_model_over_every_history() {
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
    census.from_changed,
    census.changed_beneath,
    census.replaced,
    census.nested,
    census.nested_replaced,
    census.moved,
  ];
  assert!(
    met.iter().all(|&count| count > 0),
    "a case never met: {census:?}"
  );
}
