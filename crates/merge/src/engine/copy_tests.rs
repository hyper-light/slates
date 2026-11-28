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
use super::{Green, Increment, Judgment, Lineage, Outcome, Reanchored, change_bytes};
use crate::chain_entry;
use crate::checkpoint::{Checkpoint, Tables};
use crate::increment::Base;
use crate::ops_doc::{NODE_EXCLUSIVE, NodeOrigin, NodeRef, Op, OpKind, OpsDoc, REPLACE};
use crate::origin::Origin;
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
      named: Default::default(),
    }
  }
}

/// Submits `work` to `green` at its head as increment `id`, which must be accepted.
fn accept(green: &mut Green, work: &Work, id: u64) -> u64 {
  let base = green.head();
  match green.submit(&work.at(id, base)) {
    Outcome::Accepted { version } | Outcome::Identical { version } => version,
    other => panic!("increment {id} was not accepted: {other:?}"),
  }
}

/// A-655 part 2b (`Green::beneath`: what a Subversion deletion names). Do: over `trunk`, list what stands beneath
/// `trunk`, beneath `trunk/d`, beneath a file and beneath nothing; copy `trunk` to `branch` and list beneath `branch`.
/// Expect: every file and directory beneath each, deepest first, the path itself left out; none beneath a file or an
/// absent path; the copy's graft listed as the source's tree at its new place.
#[test]
fn t_a655_beneath_lists_a_tree_deepest_first_a_copys_graft_among_them() {
  let mut green = trunk();
  let tree = |at: &str| -> Vec<(String, bool)> {
    [("d/b", false), ("d", true), ("c", false), ("a", false)]
      .iter()
      .map(|(path, directory)| (format!("{at}/{path}"), *directory))
      .collect()
  };
  assert_eq!(green.beneath("trunk"), tree("trunk"));
  assert_eq!(
    green.beneath("trunk/d"),
    vec![("trunk/d/b".to_owned(), false)]
  );
  assert_eq!(
    (green.beneath("trunk/a"), green.beneath("gone")),
    (Vec::new(), Vec::new())
  );
  let mut copy = Work::exclusive();
  copy.copy("trunk", "branch", 1);
  accept(&mut green, &copy, 2);
  assert_eq!(green.beneath("branch"), tree("branch"));
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
    Outcome::Accepted { .. }
    | Outcome::Identical { .. }
    | Outcome::BaseUnavailable { .. }
    | Outcome::CopyRefused { .. }
    | Outcome::ContentUnread { .. } => None,
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
  holds_the_trunk_at_branches_x(&green);
  holds_branches_x_from_version_2_only(&green);
  assert_eq!(green.changed_between(1, 2), ["branches/x"]);
  assert_eq!(green.changed_since(1), ["branches/x"]);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  assert_eq!(green.changes_bytes(), green.changes_recounted());
}

/// The trunk's files, directories, modes and properties at `branches/x`'s place, and the trunk still its own.
fn holds_the_trunk_at_branches_x(green: &Green) {
  assert_eq!(green.content_bytes("branches/x/a"), Some(&b"alpha"[..]));
  assert_eq!(green.content_bytes("branches/x/d/b"), Some(&b"beta"[..]));
  assert!(green.is_dir("branches/x") && green.is_dir("branches/x/d"));
  assert_eq!(green.mode("branches/x"), Some(0o755));
  assert_eq!(green.mode("branches/x/a"), Some(0o644));
  assert_eq!(green.xattr("branches/x", PROPS), Some(&b"p1"[..]));
  assert_eq!(green.xattr("branches/x/d/b", PROPS), Some(&b"p2"[..]));
  assert_eq!(green.content_bytes("trunk/a"), Some(&b"alpha"[..]));
}

/// Nothing at `branches/x` at version 1; the copied tree there at version 2.
fn holds_branches_x_from_version_2_only(green: &Green) {
  assert_eq!(green.content_at_bytes("branches/x/a", 1), None);
  assert_eq!(
    green.content_at_bytes("branches/x/a", 2),
    Some(b"alpha".to_vec())
  );
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
    green.content_bytes("dst/7/4039"),
    Some(&4039u32.to_le_bytes()[..])
  );
  assert_eq!(green.files_bytes().count(), 8192);
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
  reads_b_a_by_version(&green);
  assert_eq!(green.content_bytes("trunk/a"), Some(&b"alpha"[..]));
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  assert_eq!(green.changed_between(2, 3), ["b/a"]);
}

/// `b/a` absent at version 1, the copied bytes and mode at 2 (a page and a length read from them), the new ones at 3.
fn reads_b_a_by_version(green: &Green) {
  assert_eq!(green.content_at_bytes("b/a", 1), None);
  assert_eq!(green.content_at_bytes("b/a", 2), Some(b"alpha".to_vec()));
  assert_eq!(green.content_at_bytes("b/a", 3), Some(b"ALpha".to_vec()));
  assert_eq!(
    green.content_page_at_bytes("b/a", 2, (1, 2)),
    Some((b"lp".to_vec(), 5))
  );
  assert_eq!(green.content_len_at("b/a", 2), Some(5));
  assert!(green.base_at(2).modes.contains(&("b/a".to_owned(), 0o644)));
  assert!(green.base_at(3).modes.contains(&("b/a".to_owned(), 0o700)));
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
  edits_landed_on_the_copy(&mut green);
  let applied = applied.expect("an accepted version's effects");
  let increment_bytes = increment.encode();
  let entry = chain_entry::encode(
    chain_entry::held_for_tests(&increment_bytes),
    &increment,
    &applied,
    None,
    None,
    None,
  );
  let decoded = chain_entry::decode(&entry, &increment_bytes).expect("the entry decodes");
  assert_eq!(decoded.applied, applied);
  assert_eq!(
    replica.apply_recorded(&mut decoded.increment.clone(), &decoded.applied),
    2
  );
  replica_reads_alike(&mut replica, &mut green);
  // The file the copy brought and its own increment edited replaced no file: A-612's removals record none, live or
  // replayed.
  assert!(!green.file_removed.contains_key("b/a"));
  assert!(!replica.file_removed.contains_key("b/a"));
}

/// The edits of the copy's own increment on what the copy brought at `b`, and the trunk untouched.
fn edits_landed_on_the_copy(green: &mut Green) {
  assert_eq!(green.content_bytes("b/a"), Some(&b"ALpha"[..]));
  assert_eq!(green.content_bytes("b/new"), Some(&b"new"[..]));
  assert_eq!(green.content_bytes("b/c"), None);
  assert_eq!(green.mode("b/d"), Some(0o700));
  assert_eq!(green.xattr("b/d/b", PROPS), Some(&b"q"[..]));
  assert_eq!(green.content_bytes("trunk/a"), Some(&b"alpha"[..]));
  assert_eq!(green.content_bytes("trunk/c"), Some(&b"gamma"[..]));
  edits_landed_at_their_version(green);
}

/// The copy's own increment's edits read at its version and not before it, and the identity equal to its recount.
fn edits_landed_at_their_version(green: &mut Green) {
  assert_eq!(green.content_at_bytes("b/a", 1), None);
  assert_eq!(green.content_at_bytes("b/a", 2), Some(b"ALpha".to_vec()));
  assert_eq!(green.content_at_bytes("b/c", 2), None);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
}

/// The replayed `replica` reads and hashes as `green` does.
fn replica_reads_alike(replica: &mut Green, green: &mut Green) {
  assert_eq!(replica.head_identity(), green.head_identity());
  assert_eq!(replica.base_at(2), green.base_at(2));
  assert_eq!(replica.content_bytes("b/a"), Some(&b"ALpha"[..]));
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
  assert_eq!(green.content_bytes("snap/a"), Some(&b"x"[..]));
  assert_eq!(green.content_bytes("snap/d/f"), Some(&b"y"[..]));
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

/// A-664 part 3 (a reader of the change index takes a copy's tree from the head: the index names the copy at its
/// destination alone). Do: copy `trunk` at version 1 to `branches/x`, then ask `copied_after` of the destination, a
/// path beneath it, the source and a path beside it. Expect: the destination and the path beneath it copied after
/// version 1 and not after version 2, the copy's own; the source and the path beside it never.
#[test]
fn a_copy_is_named_for_every_path_of_its_tree_after_the_version_before_it() {
  let mut green = trunk();
  accept(
    &mut green,
    Work::exclusive().copy("trunk", "branches/x", 1),
    2,
  );
  let asked = [
    green.copied_after("branches/x", 1),
    green.copied_after("branches/x/d/b", 1),
    green.copied_after("branches/x", 2),
    green.copied_after("branches/x/d/b", 2),
    green.copied_after("trunk/a", 1),
    green.copied_after("branches/y", 1),
  ];
  assert_eq!(asked, [true, true, false, false, false, false]);
}

/// A-586 and A-626 (the verdict's refusals, each naming the destination by kind). Do: against a green holding a trunk,
/// a hard link under `h`, one under `h2` naming `trunk/c`, a file under `taken`, a trunk changed after version 1 and a
/// path removed after version 3, submit each refused copy: among them a copy of `h2` from before its link's outside node
/// changed (main's ruling: the head no longer holds what that node held), a second copy onto a place the first copy's
/// tree fills, and replaces of a tree changed after the base, of a tree holding a hard link and of a link's target. Expect: each a conflict with exactly
/// its window, and the green unchanged: its head, its identity and the copies it grafted.
#[test]
fn a_copy_is_refused_typed_and_changes_nothing() {
  let mut green = trunk();
  let mut work = Work::default();
  work
    .create("t2/a", b"t2")
    .link("h/l", "trunk/a")
    .link("h2/l", "trunk/c")
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
  use crate::engine::CopyRefusal::{
    LinkAcrossCopy, OutsideNodeChanged, PastHead, Root, SourceEmpty,
  };
  use MergeConflictClass::{CreateCreate, DeleteModify, TypeChanged};
  // Refused before any verdict, typed (main's ruling of 2026-11-20): none is a change a rebase could answer.
  let refused = [
    (one_copy("t2", "", 5), ("", Root)),
    (one_copy("nothing", "y", 5), ("y", SourceEmpty)),
    (one_copy("t2", "y", 6), ("y", PastHead)),
    (one_copy("h2", "y", 2), ("y", OutsideNodeChanged)),
    (moved, ("y/q", LinkAcrossCopy)),
  ];
  for (index, (work, (path, case))) in refused.into_iter().enumerate() {
    let outcome = green.submit(&work.at(200 + index as u64, 5));
    let expected = Outcome::CopyRefused {
      path: path.to_owned(),
      case,
    };
    assert_eq!(outcome, expected, "refused case {index}");
    assert_eq!(green.head(), head, "refused case {index} committed");
  }
  /// A case: the work, the version it is judged at, and the conflicts expected of it.
  type Case<'a> = (Work, u64, Vec<(&'a str, MergeConflictClass)>);
  let cases: Vec<Case<'_>> = vec![
    (one_copy("t2", "taken", 5), 5, vec![("taken", CreateCreate)]),
    (
      one_copy("t2", "trunk/a", 5),
      5,
      vec![("trunk/a", CreateCreate)],
    ),
    (one_copy("t2", "gone", 5), 3, vec![("gone", CreateCreate)]),
    (both, 5, vec![("y", TypeChanged)]),
    (nested, 5, vec![("y/a", CreateCreate)]),
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
  assert_eq!(green.content_bytes("d/x"), Some(&b"Eew!"[..]));
  assert_eq!(green.content_at_bytes("d/x", 1), Some(b"old!".to_vec()));
  assert_eq!(green.content_at_bytes("d/x", 2), None);
  assert_eq!(green.content_at_bytes("d/x", 3), Some(b"new!".to_vec()));
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
        green.content_at_bytes("b/a", version),
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
          green.content_bytes("b/a").map(<[u8]>::to_vec),
          green.is_dir("b/d"),
        )
      })
      .expect("the branch");
    assert_eq!(identity, expected_identity, "the identity at {version}");
    assert_eq!((read, made), (bytes, dir), "the reads at {version}");
    lineage
      .with(branch, |green| {
        assert_eq!(green.content_bytes("trunk/a"), Some(&b"alpha"[..]));
      })
      .expect("the branch");
  }
  assert_eq!(lineage.walked(), walked, "a branch walked a map");
}

/// What a restored green and the green it was restored from answer alike at their head: the head, its base, the bytes
/// of `b/a`, and the identity, which must also equal the restored green's recount.
fn restored_reads(green: &mut Green) -> (u64, Base, Option<Vec<u8>>, [u8; 32], [u8; 32]) {
  (
    green.head(),
    green.base_at(green.head()),
    green.content_bytes("b/a").map(<[u8]>::to_vec),
    green.head_identity(),
    green.head_identity_from_scratch(),
  )
}

/// A-626 part 3 (a checkpoint of a green holding a copy; A-586 had refused one). Do: copy `trunk` to `b`, edit `b/a`
/// once after it, and checkpoint the green at the copy's version and at the edit's; restore each from the origin (none)
/// and give it the versions after. Expect: each checkpoint carries the copy's record and lists the copied keys that hold
/// what the copy gave them (at the copy's version, every key of the tree; at the edit's, every one but `b/a`, which its
/// own change lists); each restored green reads, and hashes, as the green, its identity equal to its recount.
#[test]
fn a_green_holding_a_copy_is_checkpointed_and_restored() {
  let mut green = trunk();
  accept(&mut green, Work::exclusive().copy("trunk", "b", 1), 2);
  accept(&mut green, Work::default().overwrite("b/a", 0, b"A"), 3);
  for (at, copied_files) in [(2, 3), (3, 2)] {
    let checkpoint = green.checkpoint(at).expect("a checkpoint");
    assert_eq!(checkpoint.copies.len(), 1, "the copy's record at {at}");
    assert_eq!(
      checkpoint.copied.files.len(),
      copied_files,
      "the copied files at {at}"
    );
    let mut restored = Green::restore(Origin::default(), &checkpoint);
    if at == 2 {
      let work = Work::default().overwrite("b/a", 0, b"A").at(3, 2);
      assert_eq!(restored.submit(&work), Outcome::Accepted { version: 3 });
    }
    let (read, expected) = (restored_reads(&mut restored), restored_reads(&mut green));
    assert_eq!(read, expected, "restored at {at}");
    assert_eq!(read.3, read.4, "the restored identity is its recount");
  }
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
  assert_eq!(green.content_bytes(&path("a")), Some(&b"alpha"[..]));
  assert_eq!(green.content_bytes(&path("c")), Some(&b"gamma"[..]));
  assert_eq!(green.content_bytes(&path("d/b")), Some(&b"beta"[..]));
  assert!(green.is_dir(at) && green.is_dir(&path("d")));
  assert_eq!(green.mode(at), Some(0o755));
  assert_eq!(green.mode(&path("a")), Some(0o644));
  assert_eq!(green.xattr(at, PROPS), Some(&b"p1"[..]));
  assert_eq!(green.xattr(&path("d/b"), PROPS), Some(&b"p2"[..]));
}

/// Asserts that `green` read at version 2 holds the branch `b` of [`trunk_and_branch`], byte for byte.
fn assert_holds_the_branch_at_2(green: &Green) {
  assert_eq!(green.content_at_bytes("b/a", 2), Some(b"mine".to_vec()));
  assert_eq!(green.content_at_bytes("b/only", 2), Some(b"only".to_vec()));
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
  let increment_bytes = increment.encode();
  let entry = chain_entry::encode(
    chain_entry::held_for_tests(&increment_bytes),
    increment,
    applied,
    None,
    None,
    None,
  );
  let decoded = chain_entry::decode(&entry, &increment_bytes).expect("the entry decodes");
  assert_eq!(decoded.applied, applied);
  assert_eq!(
    replica.apply_recorded(&mut decoded.increment.clone(), &decoded.applied),
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
  assert_eq!(green.content_bytes("b/only"), None);
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
    .map(|version| green.content_at_bytes("b/a", version))
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
  assert_eq!(green.content_bytes(&joined(at, "f")), Some(&b"f2"[..]));
  assert_eq!(green.content_bytes(&joined(at, "e/g")), Some(&b"g2"[..]));
}

/// What the nested copies leave at `b`: `t2`'s tree at `b/new` and over `b/d`, `b/d/b` and its property gone, the
/// trunk's `a` moved to `b/moved`, and the rest the trunk's.
fn b_holds_its_nested_copies(green: &Green) {
  assert_holds_t2_at(green, "b/new");
  assert_holds_t2_at(green, "b/d");
  assert_eq!(green.content_bytes("b/d/b"), None);
  assert_eq!(green.xattr("b/d/b", PROPS), None);
  assert_eq!(green.content_bytes("b/moved"), Some(&b"alpha"[..]));
  assert_eq!(green.content_bytes("b/a"), None);
  assert_eq!(green.content_bytes("b/c"), Some(&b"gamma"[..]));
  assert_eq!(green.xattr("b", PROPS), Some(&b"p1"[..]));
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
  b_holds_its_nested_copies(&green);
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
    matches!(
      outcome,
      Outcome::Accepted { .. } | Outcome::Identical { .. }
    ),
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
    let increment_bytes = increment.encode();
    let entry = chain_entry::encode(
      chain_entry::held_for_tests(&increment_bytes),
      increment,
      applied,
      None,
      None,
      None,
    );
    let decoded = chain_entry::decode(&entry, &increment_bytes).expect("the entry decodes");
    assert_eq!(&decoded.applied, applied);
    replica.apply_recorded(&mut decoded.increment.clone(), &decoded.applied);
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
  let entry = chain_entry::encode(
    chain_entry::held_for_tests(&bytes),
    increment,
    applied,
    None,
    None,
    None,
  );
  let bound = chain_entry::entry_bound(increment) + room;
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
  assert_eq!(green.content_bytes("trunk/a"), Some(&b"ALpha"[..]));
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
  assert_eq!(green.content_bytes("c/t/f"), Some(&b"f2"[..]));
  assert_eq!(green.content_bytes("b/t/f"), None);
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
  assert_eq!(green.content_bytes("b/a"), Some(&b"alpHA"[..]));
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  let applied = applied.expect("an accepted version's effects");
  assert_replays_alike(&mut green, trunk_changed_after_1(), &increment, &applied);
}

/// A-626 part 2 (a copy from a version whose values are folded away). Do: fold the green's histories below version 3,
/// then copy the trunk at version 1 (changed at 2, so its values then are needed) and at version 3 (unchanged since).
/// Expect: the first refused typed, `VersionFolded`, naming its destination (main's ruling of 2026-11-20: no conflict),
/// the green unchanged; the second accepted.
#[test]
fn a_copy_from_a_folded_version_is_refused_typed() {
  let mut green = trunk_changed_after_1();
  green.fold_history_before(3);
  let head = green.head();
  let stale = green.submit(&one_copy("trunk", "b", 1).at(4, head));
  assert_eq!(
    stale,
    Outcome::CopyRefused {
      path: "b".to_owned(),
      case: crate::engine::CopyRefusal::VersionFolded,
    }
  );
  assert_eq!(green.head(), head);
  assert_eq!(
    green.submit(&one_copy("trunk", "c", 3).at(5, head)),
    Outcome::Accepted { version: 4 }
  );
}

/// Submits `increment` to `green`, which must accept it: what its version applied, as its chain entry records it.
fn accepted_effects(green: &mut Green, increment: &Increment) -> Vec<chain_entry::Applied> {
  let (outcome, applied) = green.submit_resolved(increment);
  assert!(
    matches!(
      outcome,
      Outcome::Accepted { .. } | Outcome::Identical { .. }
    ),
    "not accepted: {outcome:?}"
  );
  applied.expect("an accepted version's effects")
}

/// Replays `increment`'s chain entry (`applied`, encoded and decoded) on `replica`, reading nothing of its identity: the
/// new head.
fn replay_entry(
  replica: &mut Green,
  increment: &Increment,
  applied: &[chain_entry::Applied],
) -> u64 {
  let increment_bytes = increment.encode();
  let entry = chain_entry::encode(
    chain_entry::held_for_tests(&increment_bytes),
    increment,
    applied,
    None,
    None,
    None,
  );
  let decoded = chain_entry::decode(&entry, &increment_bytes).expect("the entry decodes");
  replica.apply_recorded(&mut decoded.increment.clone(), &decoded.applied)
}

/// A-626 part 3 (the replica's half of the copy oracle's seed `cc ef9d3557…`, found by reading): a green restored at a
/// checkpoint taken after its trunk changed and before a copy of the trunk from earlier replays the copy, then a change
/// to a file the copy brought, its identity read only after both. Do: checkpoint [`trunk_changed_after_1`] at 3, copy
/// the trunk at 1 to `b` as version 4 and overwrite `b/a` as 5; restore the checkpoint and replay 4 and 5, each entry
/// encoded and decoded. Expect: the replica hashes as its owner at 5, its identity equals its recount, its identity
/// moved to 3 is its owner's, and so is its checkpoint at 3 (which carries no sum since A-626 part 3, continued (1)).
/// Before, each read what the copy gave `b/a` from the trunk's history at 1, which the restored green does not hold: it
/// read nothing there, so the replica hashed otherwise than its owner and moved another sum to 3.
#[test]
fn a_replica_restored_after_a_copys_source_version_hashes_as_its_owner() {
  let mut green = trunk_changed_after_1();
  let checkpoint = green.checkpoint(3).expect("a checkpoint at the head");
  let mut copy = Work::exclusive();
  copy.copy("trunk", "b", 1);
  let copy = copy.at(4, 3);
  let copied = accepted_effects(&mut green, &copy);
  let mut edit = Work::default();
  edit.overwrite("b/a", 0, b"Z");
  let edit = edit.at(5, 4);
  let edited = accepted_effects(&mut green, &edit);
  let mut replica = Green::restore(Origin::default(), &checkpoint);
  assert_eq!(replay_entry(&mut replica, &copy, &copied), 4);
  assert_eq!(replay_entry(&mut replica, &edit, &edited), 5);
  assert_eq!(
    replica.head_identity(),
    green.head_identity(),
    "the replica's identity at 5"
  );
  assert_eq!(
    replica.head_identity(),
    replica.head_identity_from_scratch()
  );
  // Below the copy, the identity moved to 3 is its owner's (`Green::sum_at`, what a branch there carries), and so is the
  // whole checkpoint at 3, which carries no sum since no copy stands at or before it (A-626 part 3, continued (1)).
  let moved = |green: &Green| green.sum_at(3);
  assert!(
    moved(&replica) == moved(&green),
    "the replica moves its identity to 3 otherwise than its owner"
  );
  let encoded = |green: &Green| green.checkpoint(3).map(|checkpoint| checkpoint.encode());
  assert_eq!(
    encoded(&replica),
    encoded(&green),
    "the replica's checkpoint at 3"
  );
}

/// A-626 part 3 (found by reading): as above with the edit in the copy's own increment, whose value for `b/a` stood at
/// no version, so only the copy's graft holds it. Do: checkpoint [`trunk_changed_after_1`] at 3, then in one increment
/// copy the trunk at 1 to `b` and overwrite `b/a` at its fourth byte as version 4; restore the checkpoint and replay 4.
/// Expect: the replica hashes as its owner, and its identity equals its recount. Before, both read the trunk's history
/// at 1, which the restored green does not hold.
#[test]
fn a_replica_replaying_a_copy_edited_in_its_own_version_hashes_as_its_owner() {
  let mut green = trunk_changed_after_1();
  let checkpoint = green.checkpoint(3).expect("a checkpoint at the head");
  let mut work = Work::exclusive();
  work.copy("trunk", "b", 1).overwrite("b/a", 3, b"HA");
  let increment = work.at(4, 3);
  let applied = accepted_effects(&mut green, &increment);
  let mut replica = Green::restore(Origin::default(), &checkpoint);
  assert_eq!(replay_entry(&mut replica, &increment, &applied), 4);
  assert_eq!(
    replica.head_identity(),
    green.head_identity(),
    "the replica's identity"
  );
  assert_eq!(
    replica.head_identity(),
    replica.head_identity_from_scratch()
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
  /// Checkpoints taken at or after a copy, restored and replayed (A-626 part 3).
  checkpointed_copies: usize,
  /// Of those, ones listing copied keys a restore sets with no history.
  copied_keys: usize,
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

  /// Whether every case was met at least once.
  fn met(&self) -> bool {
    [
      self.accepted,
      self.refused,
      self.from_root,
      self.from_before,
      self.into_themselves,
      self.from_changed,
      self.changed_beneath,
      self.replaced,
      self.nested,
      self.nested_replaced,
      self.moved,
      self.checkpointed_copies,
      self.copied_keys,
    ]
    .iter()
    .all(|&count| count > 0)
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

/// A step a proptest case cannot go on from, as the case's failure.
fn failed(error: impl std::fmt::Debug) -> TestCaseError {
  TestCaseError::fail(format!("{error:?}"))
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
    let head = self.green.head();
    // The paths the view is held to, from the model (a walk of either green would move the walk count the copies are
    // held to): every path of the head's tree and of the tree the model expects next, and every directory above one.
    let mut universe = self.model.current().paths();
    if let Some((next, _, _)) = &expected {
      universe.extend(next.paths());
    }
    let universe = with_parents(universe);
    let viewed = self.view_of(&increment, &universe)?;
    let (outcome, applied) = self.green.submit_resolved(&increment);
    // A-805: the view of the held verdict read as the green reads once it commits.
    if let Some((changed, reads)) = viewed
      && self.green.head() == head + 1
    {
      prop_assert_eq!(&changed, &self.green.changed_between(head, head + 1));
      for path in universe.iter().chain(&changed) {
        let committed = reads_at(&self.green, path, head);
        prop_assert_eq!(reads.get(path), Some(&committed), "path {}", path);
      }
    }
    let accepted = matches!(
      outcome,
      Outcome::Accepted { .. } | Outcome::Identical { .. }
    );
    prop_assert_eq!(accepted, expected.is_some(), "{:?}", outcome);
    if let (Some(applied), Some(landed)) = (applied, expected) {
      self.entries.push((increment, applied));
      self.model.push(landed);
    }
    Ok(accepted)
  }

  /// A-805: the green lent to a lineage, the increment judged there and, when held accepted, viewed as it would commit
  /// (`Lineage::preview`): the paths it changes and every path's reads, with the fork's numbering (the head its 0).
  /// The fork is retired and released, and the green taken back, judged but unchanged.
  fn view_of(
    &mut self,
    increment: &Increment,
    universe: &BTreeSet<String>,
  ) -> Result<Option<Viewed>, TestCaseError> {
    let (mut lineage, handle) = Lineage::new(std::mem::take(&mut self.green));
    let judgment = lineage
      .with(handle, |green| green.judge(increment))
      .map_err(failed)?;
    let viewed = match judgment {
      Judgment::Accepted(judged) => {
        let fork = lineage.preview(handle, judged).map_err(failed)?;
        let viewed = lineage
          .with(fork, |view| {
            let changed = view.changed_between(0, 1);
            let reads = universe
              .iter()
              .chain(&changed)
              .map(|path| (path.clone(), reads_at(view, path, 0)))
              .collect();
            (changed, reads)
          })
          .map_err(failed)?;
        let mut release = lineage.retire(fork).map_err(failed)?;
        while !lineage
          .release_step(&mut release, &mut crate::reaped::Reaped::default())
          .map_err(failed)?
        {}
        Some(viewed)
      }
      _ => None,
    };
    self.green = lineage.take(handle).map_err(failed)?;
    Ok(viewed)
  }

  /// A one-byte file at `path` and its attribute: an overwrite where a file stands, refused where a directory does.
  fn put(&mut self, path: &str, byte: u8, census: &mut Census) -> Result<(), TestCaseError> {
    census.changed_beneath += usize::from(self.green.copy_above(path, self.green.head()).is_some());
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
    census.changed_beneath += usize::from(self.green.copy_above(path, self.green.head()).is_some());
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
      self.check_version(&self.green, version, head)?;
    }
    prop_assert_eq!(
      self.green.head_identity(),
      self.green.head_identity_from_scratch()
    );
    prop_assert_eq!(self.green.changes_bytes(), self.green.changes_recounted());
    prop_assert_eq!(self.green.copies_refused(), 0);
    Ok(())
  }

  /// [`Oracle::check`] of `green` (the green under test, or one restored from its checkpoint) at one version.
  fn check_version(&self, green: &Green, version: u64, head: u64) -> Result<(), TestCaseError> {
    let state = self
      .model
      .state(version)
      .ok_or_else(|| TestCaseError::fail("a version the model lacks"))?;
    prop_assert_eq!(
      sorted(green.base_at(version)),
      state.base(),
      "the base at {}",
      version
    );
    for (path, bytes) in &state.files {
      prop_assert_eq!(
        green.content_at_bytes(path, version),
        Some(bytes.clone()),
        "{} at {}",
        path,
        version
      );
    }
    let named: BTreeSet<String> = green.changed_between(version, head).into_iter().collect();
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
      let increment_bytes = increment.encode();
      let entry = chain_entry::encode(
        chain_entry::held_for_tests(&increment_bytes),
        increment,
        applied,
        None,
        None,
        None,
      );
      let decoded = chain_entry::decode(&entry, &increment_bytes)
        .map_err(|error| TestCaseError::fail(format!("{error:?}")))?;
      replica.apply_recorded(&mut decoded.increment.clone(), &decoded.applied);
      identities.push(replica.head_identity());
    }
    Ok((replica, identities))
  }

  /// A-626 part 3: the green checkpointed at every version, each checkpoint encoded and decoded, restored from the
  /// origin (none here) and given every later version's chain entry ([`Oracle::restored_from`]); the restored green must
  /// read as the model at every version from the checkpoint on (each base, each file's bytes, the paths named since).
  fn restore_at_every_version(&self, census: &mut Census) -> Result<(), TestCaseError> {
    let (_, identities) = self.replay()?;
    let head = self.green.head();
    for at in 0..=head {
      let checkpoint = self
        .green
        .checkpoint(at)
        .ok_or_else(|| TestCaseError::fail(format!("no checkpoint at {at}")))?;
      census.checkpointed_copies += usize::from(!checkpoint.copies.is_empty());
      census.copied_keys += usize::from(lists_copied(&checkpoint.copied));
      let decoded = Checkpoint::decode(&checkpoint.encode())
        .map_err(|error| TestCaseError::fail(format!("{error:?}")))?;
      prop_assert_eq!(&decoded, &checkpoint, "the checkpoint at {} round trip", at);
      let restored = self.restored_from(&decoded, &identities)?;
      for version in at..=head {
        self.check_version(&restored, version, head)?;
      }
    }
    Ok(())
  }

  /// The green restored from `checkpoint` and given every chain entry after it, encoded and decoded: it must hash as the
  /// replay (`identities`, by version) at its restore and at every version it reaches, and end with its identity equal
  /// to its recount and its change index's bytes to theirs.
  fn restored_from(
    &self,
    checkpoint: &Checkpoint,
    identities: &[[u8; 32]],
  ) -> Result<Green, TestCaseError> {
    let mut restored = Green::restore(Origin::default(), checkpoint);
    let at = usize::try_from(checkpoint.version).unwrap_or(usize::MAX);
    let expected = |version: usize| identities.get(version).copied();
    prop_assert_eq!(
      Some(restored.head_identity()),
      expected(at),
      "restored at {}",
      at
    );
    for (version, (increment, applied)) in self.entries.iter().enumerate().skip(at) {
      let increment_bytes = increment.encode();
      let entry = chain_entry::encode(
        chain_entry::held_for_tests(&increment_bytes),
        increment,
        applied,
        None,
        None,
        None,
      );
      let decoded = chain_entry::decode(&entry, &increment_bytes)
        .map_err(|error| TestCaseError::fail(format!("{error:?}")))?;
      restored.apply_recorded(&mut decoded.increment.clone(), &decoded.applied);
      prop_assert_eq!(
        Some(restored.head_identity()),
        expected(version + 1),
        "restored at {} replayed to {}",
        at,
        version + 1
      );
    }
    prop_assert_eq!(
      restored.head_identity(),
      restored.head_identity_from_scratch()
    );
    prop_assert_eq!(restored.changes_bytes(), restored.changes_recounted());
    Ok(restored)
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

/// Whether a checkpoint's copied tables list any key (A-626 part 3).
fn lists_copied(tables: &Tables) -> bool {
  !(tables.files.is_empty()
    && tables.dirs.is_empty()
    && tables.modes.is_empty()
    && tables.xattrs.is_empty()
    && tables.specials.is_empty()
    && tables.symlinks.is_empty()
    && tables.hardlinks.is_empty())
}

/// Runs one history, checking after every step, then restores it from a checkpoint at every version, replays its chain
/// and branches it at every version.
fn run(steps: &[Step], census: &mut Census) -> Result<(), TestCaseError> {
  let mut oracle = Oracle::default();
  for step in steps {
    oracle.apply(step, census)?;
    oracle.check()?;
  }
  oracle.restore_at_every_version(census)?;
  oracle.replay_and_branch()
}

/// One history meeting every case the census counts, so each count is met on every run whatever the generator's seed,
/// the generated histories adding to them (A-672, after A-670's rule for the restatement oracle). On the generated
/// histories alone, an inner copy replacing what its outer copy brought was met in about one nested pair in thirty, by
/// none of 30 in one run and 3 of 37 in the next (jupiter, 2026-11-19), so a run could fail by the seed's luck. The
/// steps, by path (puts index [`PATHS`] through [`named_path`], copies through [`any_path`]): `a/x` put twice; `a`
/// copied to `c` as the version before the head held it, its tree changed since; `c/x` put beneath that copy; the root
/// copied into its own tree at `d`; `a` copied over `c`, replacing it, then again without replacing, refused; `a`
/// copied to `b` with `a/x` moved to `b/m` inside it; and `a` copied to `ab` with `b` copied to `ab/x`, replacing the
/// file the outer copy brought there. Every checkpoint from the first copy on carries its record and lists its
/// tree's keys (A-626 part 3).
fn fixed_history() -> Vec<Step> {
  vec![
    Step::Put { path: 1, byte: 1 },
    Step::Put { path: 1, byte: 2 },
    Step::Copy {
      from: 1,
      to: 8,
      back: 1,
      replace: false,
    },
    Step::Put { path: 8, byte: 3 },
    Step::Copy {
      from: 0,
      to: 10,
      back: 0,
      replace: false,
    },
    Step::Copy {
      from: 1,
      to: 8,
      back: 0,
      replace: true,
    },
    Step::Copy {
      from: 1,
      to: 8,
      back: 0,
      replace: false,
    },
    Step::Move {
      from: 1,
      to: 5,
      file: 0,
      at: 3,
    },
    Step::Nested {
      from: 1,
      to: 7,
      inner: 5,
      at: 0,
      replace: (false, true),
    },
  ]
}

/// A-586 and A-626 (the oracle). Do: generated histories of files and their attributes put and taken, directories made
/// with modes, single copies of any path (the root among them) at the head or a version or two before it, its tree
/// changed since or not (A-626 part 2), to any path (into the source's own tree among them), replacing what the destination holds or not, two copies nested in one
/// increment, and moves inside a copied tree as a stock client makes them, on a green and on a model that states every
/// version's tree. Expect: the green decides every step as the model does; after every step it reads as the model at
/// every version (each base, each file's bytes, the paths the change index names since), its identity equals its
/// recount and its change index's bytes theirs, no map refused a copy its verdict passed, and a copy that replaces
/// nothing walked no map; at the end, restored from its checkpoint at every version (encoded and decoded) and given every
/// later chain entry, it hashes as the replay at every version it reaches, reads as the model at every version from the
/// checkpoint on, and its identity equals its recount (A-626 part 3); its chain, replayed from its entries, reads and
/// hashes as it does, and a branch at every version reads as the model and hashes as the replay, walking no map. A fixed
/// history, run first, meets every case the census counts whatever the seed (A-672): each kind of copy, and
/// checkpoints carrying copies and listing copied keys (A-626 part 3); the generated histories add to the counts.
#[test]
fn every_copy_reads_as_its_model_over_every_history() {
  let mut fixed = Census::default();
  if let Err(failure) = run(&fixed_history(), &mut fixed) {
    panic!("the fixed history: {failure}");
  }
  assert!(
    fixed.met(),
    "the fixed history no longer meets every case: {fixed:?}"
  );
  eprintln!("copy oracle census, the fixed history: {fixed:?}");
  let census = RefCell::new(fixed);
  let config = Config {
    cases: slates_test_seeds::cases(128),
    ..Config::default()
  };
  // The reviewed seeds replay first: A-626 part 3's restore at every version met a copy from before the checkpoint.
  let seeds = include_str!("copy_tests.proptest-regressions");
  let mut runner = TestRunner::new(slates_test_seeds::seeded(config, seeds).unwrap());
  let result = runner.run(&prop::collection::vec(step(), 1..24), |steps| {
    run(&steps, &mut census.borrow_mut())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  // The figures the evidence quotes (`-- --nocapture`): the fixed history's counts and the generated histories'.
  eprintln!("copy oracle census: {:?}", census.into_inner());
}

/// A-658 (a work's copies judged again once the green moved). Do: compute, on a copy of a trunk at version 1, a work
/// copying `trunk` at version 1 to `b`; land on the owner an overwrite of `trunk/a` as version 2; hand the owner the
/// computed verdict. Expect: the copies' unit judged again, the verdict held at version 2 equal to the one the owner
/// computes there itself (the copy fixed to bring `trunk/a` as it stood at version 1); committed, `b/a` holds `alpha`.
#[test]
fn a_copy_computed_before_the_green_moved_is_judged_again_by_position() {
  let copy = trunk();
  let mut owner = trunk();
  let work = one_copy("trunk", "b", 1).at(3, 1);
  let computed = copy.compute(&work).expect("computed at version 1");
  accept(
    &mut owner,
    Work::default().overwrite("trunk/a", 0, b"ALPHA"),
    2,
  );
  let again = owner.judged_again();
  let own = owner.compute(&work).expect("computed at version 2");
  let Ok(Judgment::Accepted(held)) = owner.judge_computed(&work, computed) else {
    panic!("not accepted by position");
  };
  let judged = (
    owner.judged_again() - again,
    held.at(),
    Some(held.applied()),
  );
  assert_eq!(judged, (1, 2, own.applied()));
  owner.commit_judged(held).expect("committed at its head");
  assert_eq!(owner.content_bytes("b/a"), Some(&b"alpha"[..]));
}

/// What a view answers (A-805, [`Oracle::view_of`]): the paths its version changed, and each path's reads.
type Viewed = (Vec<String>, BTreeMap<String, Reads>);

/// One path's reads as a Subversion revision is made of them (`vcs_store::svn_made`): its bytes, its property list, its
/// kind, its link target, its IPC node, its hard link's primary, and whether a copy after `since` brought it.
type Reads = (
  Option<Vec<u8>>,
  Option<Vec<u8>>,
  bool,
  Option<String>,
  bool,
  Option<String>,
  bool,
);

/// `path`'s reads in `green`, a copy counted after `since` ([`Reads`]).
fn reads_at(green: &Green, path: &str, since: u64) -> Reads {
  (
    green.content_bytes(path).map(<[u8]>::to_vec),
    green.xattr(path, PROPS).map(<[u8]>::to_vec),
    green.is_dir(path),
    green.symlink(path).map(str::to_owned),
    green.special(path).is_some(),
    green.hardlink(path).map(str::to_owned),
    green.copied_after(path, since),
  )
}

/// `paths` and every directory above one.
fn with_parents(paths: BTreeSet<String>) -> BTreeSet<String> {
  let mut all = paths.clone();
  for path in &paths {
    let mut at = path.as_str();
    while let Some(cut) = at.rfind('/') {
      at = at.get(..cut).unwrap_or_default();
      all.insert(at.to_owned());
    }
  }
  all
}

/// A green at version 2 holding `outside/f` and a trunk with two hard links: `trunk/l` naming `trunk/a`, a node inside
/// the trunk, and `trunk/o` naming `outside/f`, a node outside it.
fn linked_trunk() -> Green {
  let mut green = trunk();
  let mut work = Work::default();
  work
    .create("outside/f", b"far")
    .mode("outside/f", 0o600)
    .link("trunk/l", "trunk/a");
  accept(&mut green, &work, 2);
  accept(&mut green, Work::default().link("trunk/o", "outside/f"), 3);
  green
}

/// What a copy of `linked_trunk`'s trunk to `b` leaves, read alike from any green: `b/l`'s node, `b/o`'s node, the bytes
/// and mode `b/o` holds, `b/l`'s bytes, and the source's links.
type Linked = (
  Option<String>,
  Option<String>,
  Option<Vec<u8>>,
  Option<u32>,
  Option<Vec<u8>>,
  (Option<String>, Option<String>),
);

/// [`Linked`] of `green`.
fn linked_reads(green: &Green) -> Linked {
  let read = |path: &str| {
    green
      .content_bytes(green.hardlink(path).unwrap_or(path))
      .map(<[u8]>::to_vec)
  };
  (
    green.hardlink("b/l").map(str::to_owned),
    green.hardlink("b/o").map(str::to_owned),
    green.content_bytes("b/o").map(<[u8]>::to_vec),
    green.mode("b/o"),
    read("b/l"),
    (
      green.hardlink("trunk/l").map(str::to_owned),
      green.hardlink("trunk/o").map(str::to_owned),
    ),
  )
}

/// Main's ruling of 2026-11-20 (a hard link in a copied tree; A-586 had refused one, answering `TypeChanged`). Do: over a
/// trunk whose `l` names its own `a` and whose `o` names `outside/f`, copy the trunk to `b` at the head; replay the
/// version on a replica; checkpoint the green at the copy's version and restore it. Expect: accepted; `b/l` names
/// `b/a`, the copy's node, and reads its bytes; `b/o` names no node, a file of its own holding `outside/f`'s bytes and
/// mode; the trunk's links unchanged; the identity equal to its recount; the replica and the restored green read and
/// hash alike.
#[test]
fn a_copied_trees_links_share_the_copys_nodes_and_one_naming_a_node_outside_owns_one() {
  let mut green = linked_trunk();
  let mut replica = linked_trunk();
  let increment = Work::exclusive().copy("trunk", "b", 3).at(4, 3);
  let (outcome, applied) = green.submit_resolved(&increment);
  assert_eq!(outcome, Outcome::Accepted { version: 4 });
  let expected: Linked = (
    Some("b/a".to_owned()),
    None,
    Some(b"far".to_vec()),
    Some(0o600),
    Some(b"alpha".to_vec()),
    (Some("trunk/a".to_owned()), Some("outside/f".to_owned())),
  );
  assert_eq!(linked_reads(&green), expected);
  assert_eq!(green.head_identity(), green.head_identity_from_scratch());
  let applied = applied.expect("an accepted version's effects");
  let increment_bytes = increment.encode();
  let entry = chain_entry::encode(
    chain_entry::held_for_tests(&increment_bytes),
    &increment,
    &applied,
    None,
    None,
    None,
  );
  let decoded = chain_entry::decode(&entry, &increment_bytes).expect("the entry decodes");
  assert_eq!(
    replica.apply_recorded(&mut decoded.increment.clone(), &decoded.applied),
    4
  );
  assert_eq!(linked_reads(&replica), expected);
  assert_eq!(replica.head_identity(), green.head_identity());
  let checkpoint = green.checkpoint(4).expect("a checkpoint");
  let mut restored = Green::restore(Origin::default(), &checkpoint);
  assert_eq!(linked_reads(&restored), expected);
  assert_eq!(restored.head_identity(), green.head_identity());
}

/// A-579's partial folds, found reading `sum_at` (2026-11-21): a checkpoint's sum at the head, taken while the identity's
/// backlog is part folded, must be the sum the whole fold reaches. Do: on the trunk, copy it to `branches/x`, then
/// change three files; fold one key alone (`fold_identity_step(1)`); change the three again; take the sum at the head;
/// fold the rest. Expect: the
/// sum taken equals the folded identity's. Before the fix `sum_at` moved every key from `identity_version`, and the key
/// folded alone was moved from `identity_version`, not from the version it was folded at.
#[test]
fn a_checkpoints_sum_taken_mid_fold_is_the_whole_folds() {
  let mut green = trunk();
  accept(
    &mut green,
    Work::exclusive().copy("trunk", "branches/x", 1),
    2,
  );
  let mut work = Work::default();
  work
    .overwrite("trunk/a", 0, b"ALPHA")
    .overwrite("trunk/c", 0, b"GAMMA")
    .overwrite("trunk/d/b", 0, b"BETA");
  accept(&mut green, &work, 3);
  assert!(
    green.identity_backlog() > 1,
    "too few keys to fold one alone"
  );
  assert!(!green.fold_identity_step(1));
  // The key folded alone changes again: dirty once more, its entry in the sum at the version it was folded at.
  let mut again = Work::default();
  again
    .overwrite("trunk/a", 0, b"alpha")
    .overwrite("trunk/c", 0, b"gamma")
    .overwrite("trunk/d/b", 0, b"beta");
  accept(&mut green, &again, 4);
  let head = green.head();
  let mid_fold = green.sum_at(head);
  green.fold_identity();
  assert_eq!(mid_fold.bytes(), green.sum_at(head).bytes());
}

/// Every key of `root`'s tree as a walk of every key the green holds filters them: the reference [`Green::every_key`]
/// walked before A-920 8a, kept as its oracle.
fn every_key_by_a_whole_walk(green: &Green, root: &str) -> Vec<super::IdentityKey> {
  use super::{IdentityKey, union_keys};
  let inside = |path: &String| in_tree(path, root);
  let mut keys = Vec::new();
  let files = union_keys(green.content.keys(), green.content_history.keys());
  keys.extend(files.filter(inside).map(IdentityKey::File));
  let specials = union_keys(green.specials.keys(), green.special_history.keys().cloned());
  keys.extend(specials.filter(inside).map(IdentityKey::Special));
  let dirs = union_keys(green.dirs.keys(), green.dir_history.keys().cloned());
  keys.extend(dirs.filter(inside).map(IdentityKey::Dir));
  let modes = union_keys(green.modes.keys(), green.mode_history.keys().cloned());
  keys.extend(modes.filter(inside).map(IdentityKey::Mode));
  let symlinks = union_keys(green.symlinks.keys(), green.symlink_history.keys().cloned());
  keys.extend(symlinks.filter(inside).map(IdentityKey::Symlink));
  let hardlinks = union_keys(
    green.hardlinks.keys(),
    green.hardlink_history.keys().cloned(),
  );
  keys.extend(hardlinks.filter(inside).map(IdentityKey::Hardlink));
  let xattrs = union_keys(
    green.xattrs.iter().map(|(key, _)| key),
    green.xattr_history.keys().cloned(),
  );
  keys.extend(
    xattrs
      .filter(|(path, _)| inside(path))
      .map(|(path, name)| IdentityKey::Xattr(path, name)),
  );
  keys
}

/// A green holding `trunk`, a sibling `trunk-x` whose name begins with the trunk's, a file of the trunk made and unlinked
/// (held in its history alone), and a copy of the trunk at `b`.
fn trunk_beside_a_prefixed_sibling() -> Green {
  let mut green = trunk();
  let mut sibling = Work::default();
  sibling
    .mkdir("trunk-x")
    .create("trunk-x/a", b"sibling")
    .mode("trunk-x/a", 0o600)
    .xattr("trunk-x", PROPS, b"px")
    .create("trunk/gone", b"soon gone");
  accept(&mut green, &sibling, 2);
  accept(&mut green, Work::default().unlink("trunk/gone"), 3);
  accept(&mut green, Work::exclusive().copy("trunk", "b", 3), 4);
  green
}

/// A-920 8a. Do: over [`trunk_beside_a_prefixed_sibling`], list every key of `trunk`, `trunk-x`, `b`, `trunk/d`, a file
/// and the root (`Green::every_key`, which walks each map from the tree's root alone). Expect: each the keys a walk of
/// every key the green holds finds in that tree, in the same order: the sibling's never the trunk's, the unlinked
/// file's history among the trunk's.
#[test]
fn every_key_walks_its_tree_alone_and_finds_what_a_whole_walk_finds() {
  let green = trunk_beside_a_prefixed_sibling();
  for root in ["trunk", "trunk-x", "b", "trunk/d", "trunk/a", ""] {
    assert_eq!(
      green.every_key(root),
      every_key_by_a_whole_walk(&green, root),
      "the keys of {root:?}"
    );
  }
  let trunk = green.every_key("trunk");
  assert!(
    trunk.iter().all(|key| !key.path().starts_with("trunk-x")),
    "no sibling key among the trunk's: {trunk:?}"
  );
  assert!(
    trunk.contains(&super::IdentityKey::File("trunk/gone".to_owned())),
    "the unlinked file's history among the trunk's: {trunk:?}"
  );
}

/// A-920 8a. Do: over [`trunk_beside_a_prefixed_sibling`], take the checkpoint at the copy's version whole; then begin
/// one there, take a unit, accept versions past it (a file made and a copied file unlinked inside the copy, a file made
/// outside it), and build the rest a unit at a time (`Green::checkpoint_unit`). Expect: the same checkpoint, each copied
/// key read at the checkpoint's version, not the head's, and a unit for each copied key beside the tree's gathering and
/// the rest.
#[test]
fn a_checkpoint_built_a_unit_at_a_time_across_later_versions_is_the_one_built_whole() {
  let mut green = trunk_beside_a_prefixed_sibling();
  let whole = green.checkpoint(4).expect("a checkpoint at the copy");
  let mut build = green.checkpoint_begin(4).expect("a checkpoint begun");
  let keys = u32::try_from(green.every_key("b").len()).expect("a count");
  let mut units = 0u32;
  let mut step = |green: &Green, build: &mut super::CheckpointBuild| {
    units += 1;
    green.checkpoint_unit(build).expect("still available")
  };
  assert!(matches!(
    step(&green, &mut build),
    super::CheckpointStep::More
  ));
  accept(
    &mut green,
    Work::default().create("b/new", b"edited").unlink("b/c"),
    5,
  );
  accept(
    &mut green,
    Work::default().create("trunk/new", b"edited"),
    6,
  );
  let built = loop {
    if let super::CheckpointStep::Built(built) = step(&green, &mut build) {
      break *built;
    }
  };
  assert_eq!(built, whole, "the checkpoint built across versions 5 and 6");
  assert!(
    units >= keys + 2,
    "{units} unit(s) for {keys} copied key(s)"
  );
}

/// The copy entry room `inc` asks of `green` as it was taken before A-920 12a, the whole increment resolved for its
/// copies' fixes: the oracle `Green::copy_entry_room` is held to.
fn copy_entry_room_by_a_whole_resolve(green: &Green, inc: &Increment) -> usize {
  let resolved = Green::resolve(inc);
  green
    .copy_fixes(&resolved)
    .values()
    .filter_map(|fix| fix.as_ref().ok().and_then(Option::as_ref))
    .map(chain_entry::fix_bytes)
    .fold(0, usize::saturating_add)
}

/// A-920 12a. Do: over `trunk`, with `trunk/a` changed at version 2, ask the copy entry room of increments based at the
/// head: a copy of the trunk at version 1 (before the change, so it carries a fix) with a file made and one changed
/// beneath it; a copy of the head with an edit beneath; two copies of the trunk at version 1, one inside the other's
/// tree, each with a fix; and no copy. Expect: each the room the whole increment resolved gives, a copy from before the
/// change a room of its own, and two such copies more than one.
#[test]
fn the_copy_entry_room_from_the_copy_ops_alone_is_the_whole_resolutions() {
  let mut green = trunk();
  accept(&mut green, Work::default().overwrite("trunk/a", 0, b"A"), 2);
  let mut older = Work::exclusive();
  older
    .copy("trunk", "b", 1)
    .create("b/new", b"new")
    .overwrite("b/c", 0, b"G");
  let mut head = Work::exclusive();
  head.copy("trunk", "c", 2).overwrite("c/a", 0, b"Z");
  let mut nested = Work::exclusive();
  nested.copy("trunk", "d", 1).copy("trunk", "d/e", 1);
  let mut none = Work::default();
  none.create("x", b"x");
  let base = green.head();
  let rooms: Vec<(usize, usize)> = [older, head, nested, none]
    .iter()
    .zip(10u64..)
    .map(|(work, id)| {
      let inc = work.at(id, base);
      (
        green.copy_entry_room(&inc),
        copy_entry_room_by_a_whole_resolve(&green, &inc),
      )
    })
    .collect();
  for (from_copies, whole) in &rooms {
    assert_eq!(from_copies, whole, "the copy entry rooms: {rooms:?}");
  }
  assert!(
    rooms[0].0 > 0,
    "the copy from before the change carries no fix: {rooms:?}"
  );
  assert_eq!(rooms[3].0, 0, "an increment with no copy asks no room");
}

/// Shape: the files a wide work with a copy makes inside the copy's tree, and as many outside it.
const SPLIT_FILES: usize = 300;

/// A-920 (a resolution's end an entry a unit). Do: on `trunk`, compute, whole and a unit a step, one work copying `trunk`
/// to `b` and making [`SPLIT_FILES`] files under `b` and as many outside it. Expect both accepted with the same effects
/// recorded, and a unit a step, the resolution's end one entry a unit (`Green::resolution_ends`). Before, the end split
/// every container of the resolution whole in one call (5.86 ms debug at 9,000 paths), each path tested against every
/// copy's destination.
#[test]
fn a_resolutions_end_with_a_copy_is_taken_an_entry_a_unit() {
  let green = trunk();
  let mut work = Work::exclusive();
  work.copy("trunk", "b", 1);
  for at in 0..SPLIT_FILES {
    work.create(&format!("b/n{at}"), b"in");
    work.create(&format!("o{at}"), b"out");
  }
  let inc = work.at(2, 1);
  let whole = green.compute(&inc).expect("computed whole");
  let before = green.resolution_ends();
  let mut computing = green.begin_computing(&inc);
  let sliced = loop {
    if let Some(computed) = green.computing_step(&inc, &mut computing, &mut || true) {
      break computed.expect("computed a unit a step");
    }
  };
  let after = green.resolution_ends();
  let (entries, units) = (after.0 - before.0, after.1 - before.1);
  let recorded = whole.applied().expect("accepted whole");
  assert!(
    recorded.len() > 2 * SPLIT_FILES,
    "{} effects",
    recorded.len()
  );
  assert_eq!(sliced.applied(), Some(recorded));
  assert!(
    entries > 2 * SPLIT_FILES as u64 && units == entries,
    "{entries} entries in {units} units"
  );
}

/// The identity a reader reads of `green` at its head, folding what is owed, with the fold's bookkeeping after it.
fn identity_read(green: &mut Green) -> String {
  let identity = green.head_identity();
  format!("{} {identity:?} {:?}", green.head(), green.identity_folded)
}

/// A-752 part 3c U7 (the fold's copy reads bounded by the head; main 01a20182's ruling of 2026-11-28 03:33). Do: on a
/// serial twin and a staged green, make `src/a` and `dst/a` (left unfolded on the staged one); at version 2 replace
/// `dst` with `src`, staged one unit a step, a reader folding the identity between every two steps from the first, the
/// second and so on (fresh twins each time); then published, the serial twin given it whole. Expect every read between
/// the steps the serial twin's at version 1, and after the publish its at version 2. Red with the head bound taken off:
/// the fold of `dst/a`, which the copy brings `src/a`'s value, read the stage's copy record above the head
/// (`copies_above_after`, `copy_entries_after`).
#[test]
fn a_readers_fold_between_a_staged_copys_turns_reads_no_copy_above_the_head() {
  for from in 0u64.. {
    let (mut serial, mut staged) = (Green::new(), Green::new());
    let mut made = Work::default();
    made.create("src/a", b"source").create("dst/a", b"replaced");
    for green in [&mut serial, &mut staged] {
      accept(green, &made, 1);
    }
    let at_head = identity_read(&mut serial);
    let mut replace = Work::exclusive();
    replace.replace("src", "dst", 1);
    let inc = replace.at(2, staged.head());
    let Judgment::Accepted(held) = staged.judge(&inc) else {
      panic!("the replace was not accepted");
    };
    let mut staging = staged.begin_staging(held);
    let mut read = false;
    loop {
      let stepped = staged.staging_step(&mut staging, &mut || true);
      if staging.units() > from {
        read = true;
        assert_eq!(
          identity_read(&mut staged),
          at_head,
          "a reader's fold saw the stage at unit {}",
          staging.units()
        );
      }
      if let super::StagingStep::Staged = stepped {
        break;
      }
    }
    assert!(staged.publish(staging).is_some(), "published");
    accept(&mut serial, &replace, 2);
    assert_eq!(identity_read(&mut staged), identity_read(&mut serial));
    if !read {
      break;
    }
  }
}

/// Every path of the staged-commit tests ([`super::fold_tests::PATHS`]) made as an eight-byte file.
fn made_eight() -> Work {
  let mut work = Work::default();
  for path in super::fold_tests::PATHS {
    work.create(path, b"made0123");
  }
  work
}

/// One byte of each named file overwritten in place (`changes`: the path's place, the byte's offset, the byte): the
/// change the history's layered re-anchor takes (A-803).
fn overwrites(changes: &[(usize, u8, u8)]) -> Work {
  let mut work = Work::default();
  for (index, at, byte) in changes {
    let path = super::fold_tests::PATHS[*index % super::fold_tests::PATHS.len()];
    work.overwrite(path, u64::from(*at % 8), &[*byte]);
  }
  work
}

/// The overwrite works [`proptest`] generates: one to six, each of one to three bytes.
fn overwrite_works() -> impl Strategy<Value = Vec<Vec<(usize, u8, u8)>>> {
  prop::collection::vec(
    prop::collection::vec((0usize..5, any::<u8>(), any::<u8>()), 1..4),
    1..7,
  )
}

/// A green given every path ([`made_eight`]) and then `works` whole.
fn overwritten(works: &[Vec<(usize, u8, u8)>]) -> Green {
  let mut green = Green::new();
  accept(&mut green, &made_eight(), 1);
  for (id, changes) in (2u64..).zip(works) {
    accept(&mut green, &overwrites(changes), id);
  }
  green
}

/// Gives up the stage of `works`' last work after `turns` steps on a green given the others: whether it finished within
/// its turns, refused where the content history, the content map's live blocks and nodes or any chunk's holds were left
/// otherwise than before.
fn overwrite_given_up_after(works: &[Vec<(usize, u8, u8)>], turns: u64) -> Result<bool, String> {
  let Some((last, before)) = works.split_last() else {
    return Ok(true);
  };
  let mut green = overwritten(before);
  let inc = overwrites(last).at(u64::MAX, green.head());
  let Judgment::Accepted(held) = green.judge(&inc) else {
    return Ok(true);
  };
  let history = super::publish_tests::content_history_held(&green);
  let (live, holds) = (green.content.live(), green.chunks.holds());
  let mut staging = green.begin_staging(held);
  for _ in 0..turns {
    if let super::StagingStep::Staged = green.staging_step(&mut staging, &mut || true) {
      return Ok(true);
    }
  }
  green.abandon_staging(staging);
  if super::publish_tests::content_history_held(&green) != history {
    return Err(format!(
      "a stage given up after {turns} turns left the content history changed"
    ));
  }
  let left = green.content.live();
  if left != live {
    return Err(format!(
      "a stage given up after {turns} turns left (blocks, nodes) {left:?}, from {live:?}"
    ));
  }
  // Every chunk's holds as before: a recount from the values' pieces leaves out a layered anchor's layers, which hold
  // the overwritten bytes of the history the serial commits made (A-803).
  let left = green.chunks.holds();
  if left != holds {
    return Err(format!(
      "a stage given up after {turns} turns left chunk holds {left:?}, from {holds:?}"
    ));
  }
  Ok(false)
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-752 part 3c U8 (a staged overwrite re-anchors its own copies in full; main 01a20182's rulings of 2026-11-28 01:29
  /// and 04:01). Do: on a serial twin and a staged green, every path made; then generated works overwriting a byte of a
  /// standing file in place, the change the history's layered re-anchor takes; each staged one unit a step on the
  /// staged green, the published content history and every value's bytes read through the chunks between every two
  /// steps, then published, and given whole to the serial twin. Expect the published history held as before between
  /// the steps, every version's bytes the twin's after each publish, and the chunk guard. Red with a stage allowed the
  /// layered route: it repoints the path's shared anchor chunk, which the published values name.
  #[test]
  fn a_staged_overwrite_re_anchors_its_own_copies_and_leaves_the_published_anchor(steps in overwrite_works()) {
    let (mut serial, mut staged) = (overwritten(&[]), overwritten(&[]));
    for (id, changes) in (2u64..).zip(&steps) {
      let inc = overwrites(changes).at(id, staged.head());
      let version = super::publish_tests::content_history_unseen(&mut staged, &inc).map_err(TestCaseError::fail)?;
      let expected = accept(&mut serial, &overwrites(changes), id);
      prop_assert_eq!(version, Some(expected));
      prop_assert_eq!(
        super::publish_tests::content_bytes_read(&staged),
        super::publish_tests::content_bytes_read(&serial)
      );
    }
  }

  /// A-752 part 3c U8 (a staged overwrite given up at every turn). Expect the content history, the content map's live
  /// blocks and nodes, and every chunk's holds as before.
  #[test]
  fn a_staged_overwrite_given_up_at_any_turn_leaves_the_content_history_as_it_was(steps in overwrite_works()) {
    for end in 1..=steps.len() {
      let prefix = steps.get(..end).unwrap_or_default();
      for turns in 0u64.. {
        if overwrite_given_up_after(prefix, turns).map_err(TestCaseError::fail)? {
          break;
        }
      }
    }
  }
}
