//! The Subversion index's tests (A-553, §4.17.4 item 2 and piece 4): FSFS's rules case by case, each citing the line
//! of `structure`, `tree.c` or `dag.c` it holds the index to (Subversion `1.14.x` at `d434dc14`); every refusal, each
//! leaving the index as it was; and the oracle: a naive filesystem that keeps every revision's whole tree and applies
//! FSFS's operations to it literally, which the index must equal on generated histories (created revisions,
//! histories, relations, identities, copy roots, predecessors and mergeinfo counts), and which must equal itself when
//! each revision's rows are applied in random drive orders instead of path order.

use super::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::ops::Bound;

/// All the room there is, for the tests that are not about the bound.
pub(crate) const ALL_ROOM: usize = usize::MAX;

/// A transaction name of the form §4.17.4 item 3.1 gives: the base revision, `-`, and a work's id in base 36.
pub(crate) const TRANSACTION: &str = "0-1z";

/// A text named by one byte.
fn text(mark: u8) -> Text {
  Text {
    identity: [mark; 32],
    md5: [mark; 16],
    sha1: [mark; 20],
    size: u64::from(mark),
  }
}

/// A property list named by one byte, holding `svn:mergeinfo` or not.
fn listed(mark: u8, mergeinfo: bool) -> PropertyList {
  PropertyList::Held {
    identity: [mark; 32],
    has_mergeinfo: mergeinfo,
  }
}

/// A row with nothing set but its path, action and kind.
fn bare(path: &str, action: Action, kind: NodeKind) -> ChangedPath {
  ChangedPath {
    path: path.to_owned(),
    action,
    kind,
    copy_from: None,
    text: None,
    properties: None,
    mergeinfo_modified: false,
  }
}

/// `add-dir` without history.
fn add_dir(path: &str) -> ChangedPath {
  bare(path, Action::Add, NodeKind::Directory)
}

/// `add-file` without history, with a text.
fn add_file(path: &str, mark: u8) -> ChangedPath {
  ChangedPath {
    text: Some(text(mark)),
    ..bare(path, Action::Add, NodeKind::File)
  }
}

/// An addition by a copy of `from` in `revision`.
fn copied(path: &str, kind: NodeKind, from: &str, revision: Revision) -> ChangedPath {
  ChangedPath {
    copy_from: Some(CopySource {
      path: from.to_owned(),
      revision,
    }),
    ..bare(path, Action::Add, kind)
  }
}

/// A replacement by a copy of `from` in `revision`.
fn replaced_by_copy(path: &str, kind: NodeKind, from: &str, revision: Revision) -> ChangedPath {
  ChangedPath {
    action: Action::Replace,
    ..copied(path, kind, from, revision)
  }
}

/// A file's text changed.
fn edit(path: &str, mark: u8) -> ChangedPath {
  ChangedPath {
    text: Some(text(mark)),
    ..bare(path, Action::Modify, NodeKind::File)
  }
}

/// A node's property list changed, the mergeinfo flag set as a stock client's commit sets it when `svn:mergeinfo` is
/// in play.
fn set_properties(
  path: &str,
  kind: NodeKind,
  list: PropertyList,
  mergeinfo_modified: bool,
) -> ChangedPath {
  ChangedPath {
    properties: Some(list),
    mergeinfo_modified,
    ..bare(path, Action::Modify, kind)
  }
}

/// A deletion.
fn deleted(path: &str, kind: NodeKind) -> ChangedPath {
  bare(path, Action::Delete, kind)
}

/// Commits one revision and answers its number.
fn commit(index: &mut Index, changes: Vec<ChangedPath>) -> Revision {
  index
    .append(
      RevisionRecord {
        transaction: TRANSACTION.to_owned(),
        changes,
      },
      ALL_ROOM,
    )
    .unwrap()
}

/// The refusal of one revision's record.
fn refused(index: &mut Index, changes: Vec<ChangedPath>) -> IndexRefusal {
  index
    .append(
      RevisionRecord {
        transaction: TRANSACTION.to_owned(),
        changes,
      },
      ALL_ROOM,
    )
    .unwrap_err()
}

/// The node at a place that holds one.
fn at<'index>(index: &'index Index, path: &str, revision: Revision) -> Node<'index> {
  index.node(path, revision).unwrap().unwrap()
}

/// The node-revision, node identity and copy identity at a place that holds a node, owned, to compare with what later
/// revisions hold.
fn identities(index: &Index, path: &str, revision: Revision) -> (NodeRevisionId, NodeId, CopyId) {
  let node = at(index, path, revision);
  (node.id(), node.node_id(), node.copy_id())
}

/// The created path and revision of a node-revision: FSFS's way of naming a copy root.
fn named(index: &Index, id: NodeRevisionId) -> (String, Revision) {
  let node = index.node_revision(id).unwrap();
  (node.created_path().to_owned(), node.created_revision())
}

/// The revisions of a history list.
fn revisions(history: &[HistoryEntry]) -> Vec<Revision> {
  history.iter().map(|entry| entry.revision).collect()
}

/// Revision 1 of the fixtures: `/trunk` with `lib/a.c` and `README`, and an empty `/branches`.
fn trunk() -> Index {
  let mut index = Index::new();
  commit(
    &mut index,
    vec![
      add_dir("/trunk"),
      add_dir("/trunk/lib"),
      add_file("/trunk/lib/a.c", 1),
      add_file("/trunk/README", 2),
      add_dir("/branches"),
    ],
  );
  index
}

/// A-553 (`structure`, "Copy-IDs and copy roots"): revision 0 is an empty root directory, the first line of history,
/// never copied, its own copy root; its history is its own creation; it had no transaction.
#[test]
fn revision_zero_is_an_empty_root_that_is_its_own_copy_root() {
  let index = Index::new();
  assert_eq!(index.youngest(), 0);
  let root = at(&index, "/", 0);
  assert_an_empty_root_that_is_its_own_copy_root(&root);
  assert_eq!(
    index.history("/").unwrap(),
    &[HistoryEntry {
      revision: 0,
      event: HistoryEvent::Created(root.id())
    }]
  );
  assert!(index.node("/trunk", 0).unwrap().is_none());
  assert_eq!(index.transaction(0).unwrap(), None);
  assert!(index.changes(0).unwrap().is_empty());
}

/// A-553 (`transaction.c` `create_new_txn_noderev_from_rev`): every revision makes a new node-revision of the root,
/// the last root its predecessor, even a revision that changes nothing, as svnsync's and `svnadmin load`'s empty
/// revisions do; everything else stays the node-revision it was.
#[test]
fn every_revision_makes_a_new_root_whose_predecessor_is_the_last_even_when_it_changes_nothing() {
  let mut index = trunk();
  let empty = commit(&mut index, Vec::new());
  assert_eq!(empty, 2);
  let root = at(&index, "/", 2);
  assert_eq!(root.created_revision(), 2);
  assert_eq!(root.predecessor(), Some(at(&index, "/", 1).id()));
  assert_eq!(root.predecessor_count(), 2);
  assert_eq!(
    (root.node_id(), root.copy_id(), root.copy_root()),
    (ROOT_NODE, ROOT_COPY, FIRST_ROOT)
  );
  assert_eq!(
    index.relation(("/", 1), ("/", 2)).unwrap(),
    NodeRelation::CommonAncestor
  );
  assert_eq!(
    index.relation(("/trunk", 1), ("/trunk", 2)).unwrap(),
    NodeRelation::Unchanged
  );
  assert_eq!(revisions(index.history("/").unwrap()), [0, 1, 2]);
  assert_eq!(index.transaction(2).unwrap(), Some(TRANSACTION));
}

/// A-553 (notes §5, "Bubble-up"; `tree.c` `make_path_mutable`): a change makes a new node-revision of every directory
/// above it and of none beside it, so a directory's created revision is its subtree's last change and its history lists
/// every revision that changed anything beneath it.
#[test]
fn a_change_makes_a_new_node_revision_of_every_directory_above_it_and_none_beside_it() {
  let mut index = trunk();
  let before = at(&index, "/trunk/lib", 1).id();
  commit(&mut index, vec![edit("/trunk/lib/a.c", 3)]);
  assert_created_in(
    &index,
    &["/", "/trunk", "/trunk/lib", "/trunk/lib/a.c"],
    2,
    2,
  );
  assert_created_in(&index, &["/branches", "/trunk/README"], 2, 1);
  let lib = at(&index, "/trunk/lib", 2);
  assert_eq!(
    (lib.predecessor(), lib.predecessor_count()),
    (Some(before), 1)
  );
  assert_eq!(lib.node_id(), at(&index, "/trunk/lib", 1).node_id());
  assert_eq!(at(&index, "/trunk/lib/a.c", 2).text(), Some(text(3)));
  assert_eq!(revisions(index.history("/trunk/lib").unwrap()), [1, 2]);
  assert_eq!(revisions(index.history("/branches").unwrap()), [1]);
  assert_eq!(index.changes(2).unwrap(), &[edit("/trunk/lib/a.c", 3)]);
}

/// A-553 (`structure` "Copy-IDs and copy roots": a modification "through a copied ancestor directory (i.e. we are
/// performing a "lazy copy")"): a path beneath a copy is its source's node-revision, created where the source was,
/// until it changes beneath the copy; then it is a new node-revision of the same line, with the copy's copy identity
/// and the copy as its copy root.
#[test]
fn a_path_beneath_a_copy_is_its_sources_node_revision_until_it_changes_there() {
  let mut index = trunk();
  commit(
    &mut index,
    vec![copied("/branches/b1", NodeKind::Directory, "/trunk", 1)],
  );
  let (source_id, source_line, _) = identities(&index, "/trunk/lib/a.c", 1);
  let shared = at(&index, "/branches/b1/lib/a.c", 2);
  assert_eq!(shared.id(), source_id);
  assert_eq!(
    (shared.created_path(), shared.created_revision()),
    ("/trunk/lib/a.c", 1)
  );
  assert_eq!(
    index
      .relation(("/branches/b1/lib/a.c", 2), ("/trunk/lib/a.c", 1))
      .unwrap(),
    NodeRelation::Unchanged
  );
  let (branch_id, _, branch_copy) = identities(&index, "/branches/b1", 2);
  commit(&mut index, vec![edit("/branches/b1/lib/a.c", 4)]);
  assert_a_new_node_revision_on_the_branch(
    &index,
    (source_id, source_line),
    (branch_id, branch_copy),
  );
  assert_eq!(
    index
      .relation(("/branches/b1/lib/a.c", 3), ("/trunk/lib/a.c", 1))
      .unwrap(),
    NodeRelation::CommonAncestor
  );
  assert_eq!(
    index.created_revision("/branches/b1/README", 3).unwrap(),
    Some(1)
  );
  assert_eq!(
    index.created_revision("/trunk/lib/a.c", 3).unwrap(),
    Some(1)
  );
}

/// A-553 (`structure` "Copy-IDs and copy roots": "A node-rev resulting from a creation operation (with no copy history)
/// receives the copy-ID of its parent directory"; `dag.c` `make_entry`): a node made without history is a new line,
/// with its parent's copy identity and copy root and no predecessor.
#[test]
fn a_node_made_without_history_takes_its_parents_copy_identity_and_copy_root() {
  let mut index = trunk();
  commit(
    &mut index,
    vec![copied("/branches/b1", NodeKind::Directory, "/trunk", 1)],
  );
  commit(
    &mut index,
    vec![
      add_file("/branches/b1/new.c", 5),
      add_file("/trunk/new.c", 6),
    ],
  );
  let branch = at(&index, "/branches/b1", 3);
  let on_branch = at(&index, "/branches/b1/new.c", 3);
  assert_eq!(
    (on_branch.copy_id(), on_branch.copy_root()),
    (branch.copy_id(), branch.copy_root())
  );
  assert_eq!(
    named(&index, on_branch.copy_root()),
    ("/branches/b1".to_owned(), 2)
  );
  assert_eq!(
    (
      on_branch.predecessor(),
      on_branch.predecessor_count(),
      on_branch.copy_from()
    ),
    (None, 0, None)
  );
  let on_trunk = at(&index, "/trunk/new.c", 3);
  assert_eq!(on_trunk.copy_id(), ROOT_COPY);
  assert_eq!(named(&index, on_trunk.copy_root()), ("/".to_owned(), 0));
  let lines = BTreeSet::from([
    on_branch.node_id(),
    on_trunk.node_id(),
    at(&index, "/trunk/lib/a.c", 3).node_id(),
  ]);
  assert_eq!(lines.len(), 3);
  assert_eq!(
    index
      .relation(("/branches/b1/new.c", 3), ("/trunk/new.c", 3))
      .unwrap(),
    NodeRelation::Unrelated
  );
}

/// A-553 (`structure`: "A node-rev resulting from a copy operation receives a fresh copy-ID"; `dag.c`
/// `svn_fs_fs__dag_copy`): a copy keeps its source's line, takes a fresh copy identity, is its own copy root, has its
/// source as its predecessor and its source's text and properties; a copy of a copy goes on counting predecessors.
#[test]
fn a_copy_keeps_its_sources_line_takes_a_fresh_copy_identity_and_is_its_own_copy_root() {
  let mut index = trunk();
  commit(
    &mut index,
    vec![copied("/trunk/b.c", NodeKind::File, "/trunk/lib/a.c", 1)],
  );
  commit(
    &mut index,
    vec![copied("/trunk/c.c", NodeKind::File, "/trunk/b.c", 2)],
  );
  let source = at(&index, "/trunk/lib/a.c", 3);
  let first = at(&index, "/trunk/b.c", 3);
  let second = at(&index, "/trunk/c.c", 3);
  assert_eq!(first.node_id(), source.node_id());
  assert_eq!(second.node_id(), source.node_id());
  assert_eq!(
    BTreeSet::from([source.copy_id(), first.copy_id(), second.copy_id()]).len(),
    3
  );
  assert_eq!(
    (first.copy_root(), second.copy_root()),
    (first.id(), second.id())
  );
  assert_eq!(
    (first.predecessor(), first.predecessor_count()),
    (Some(source.id()), 1)
  );
  assert_eq!(
    (second.predecessor(), second.predecessor_count()),
    (Some(first.id()), 2)
  );
  assert_eq!(
    (first.copy_from(), second.copy_from()),
    (Some(("/trunk/lib/a.c", 1)), Some(("/trunk/b.c", 2)))
  );
  assert_eq!(
    (second.text(), second.properties()),
    (source.text(), source.properties())
  );
  assert_eq!(
    index
      .relation(("/trunk/c.c", 3), ("/trunk/lib/a.c", 1))
      .unwrap(),
    NodeRelation::CommonAncestor
  );
}

/// A-553 (`structure`: "If the predecessor does not derive from a copy, the new node-rev receives the copy-ID of its
/// parent directory. If the node-rev is being modified through its created-path, this will be the same copy-ID as the
/// predecessor node-rev has; however, if the node-rev is being modified through a copied ancestor directory ... this
/// will be a different copy-ID").
#[test]
fn a_node_never_copied_keeps_its_copy_identity_on_its_own_path_and_takes_the_copys_beneath_a_copy()
{
  let mut index = trunk();
  commit(&mut index, vec![edit("/trunk/lib/a.c", 3)]);
  assert_eq!(at(&index, "/trunk/lib/a.c", 2).copy_id(), ROOT_COPY);
  commit(
    &mut index,
    vec![copied("/branches/b1", NodeKind::Directory, "/trunk", 2)],
  );
  commit(&mut index, vec![edit("/branches/b1/lib/a.c", 4)]);
  let branch = at(&index, "/branches/b1", 4);
  let changed = at(&index, "/branches/b1/lib/a.c", 4);
  assert_ne!(changed.copy_id(), ROOT_COPY);
  assert_eq!(changed.copy_id(), branch.copy_id());
  assert_eq!(
    changed.predecessor(),
    Some(at(&index, "/trunk/lib/a.c", 2).id())
  );
}

/// A-553 (`structure`: "If the predecessor derives from a copy and the node-rev is being modified through its
/// created-path, the new node-rev receives the copy-ID of the predecessor. If the predecessor derives from a copy and
/// the node-rev is not being modified through its created path, the new node-rev receives a fresh copy-ID. This is
/// called a "soft copy""; `tree.c` `get_copy_inheritance`, `copy_id_inherit_new`): a copy changed on its own path keeps
/// its copy identity; reached through a copy of its parent it takes a fresh one, keeps its copy root, and what changes
/// beneath it takes the fresh one too.
#[test]
fn a_copy_keeps_its_copy_identity_on_its_path_and_takes_a_fresh_one_through_another_copy() {
  let mut index = trunk();
  commit(
    &mut index,
    vec![copied("/branches/b1", NodeKind::Directory, "/trunk", 1)],
  );
  let (branch_id, _, branch_copy) = identities(&index, "/branches/b1", 2);
  commit(
    &mut index,
    vec![set_properties(
      "/branches/b1",
      NodeKind::Directory,
      listed(7, false),
      false,
    )],
  );
  let kept = at(&index, "/branches/b1", 3);
  assert_eq!(
    (kept.copy_id(), kept.copy_root(), kept.created_revision()),
    (branch_copy, branch_id, 3)
  );
  let kept = kept.id();
  commit(
    &mut index,
    vec![copied("/old", NodeKind::Directory, "/branches", 3)],
  );
  let (_, _, outer_copy) = identities(&index, "/old", 4);
  assert_eq!(at(&index, "/old/b1", 4).id(), kept);
  commit(&mut index, vec![edit("/old/b1/lib/a.c", 8)]);
  let soft = at(&index, "/old/b1", 5);
  assert!(![branch_copy, outer_copy, ROOT_COPY].contains(&soft.copy_id()));
  assert_eq!(
    (soft.copy_root(), soft.predecessor()),
    (branch_id, Some(kept))
  );
  assert_eq!(soft.copy_from(), None);
  assert_eq!(at(&index, "/old/b1/lib/a.c", 5).copy_id(), soft.copy_id());
  assert_eq!(at(&index, "/old/b1/lib", 5).copy_id(), soft.copy_id());
}

/// A-553 (`structure`: `minfo-cnt`, "The number of nodes under (and including) this node which have svn:mergeinfo";
/// `tree.c` `increment_mergeinfo_up_tree`, `copy_helper`, `fs_delete_node`): every directory counts the nodes holding
/// `svn:mergeinfo` at and beneath it, through a set, a copy, a deletion, a replacement and a removal of the property.
#[test]
fn mergeinfo_counts_every_node_holding_it_at_and_beneath_a_directory() {
  let mut index = trunk();
  let counts = |index: &Index, revision: Revision, paths: &[&str]| -> Vec<u64> {
    paths
      .iter()
      .map(|path| at(index, path, revision).mergeinfo_count())
      .collect()
  };
  commit(
    &mut index,
    vec![set_properties(
      "/trunk/lib/a.c",
      NodeKind::File,
      listed(9, true),
      true,
    )],
  );
  assert!(at(&index, "/trunk/lib/a.c", 2).has_mergeinfo());
  assert_eq!(
    counts(
      &index,
      2,
      &["/trunk/lib/a.c", "/trunk/lib", "/trunk", "/", "/branches"]
    ),
    [1, 1, 1, 1, 0]
  );
  commit(
    &mut index,
    vec![set_properties(
      "/trunk",
      NodeKind::Directory,
      listed(10, true),
      true,
    )],
  );
  assert_eq!(counts(&index, 3, &["/trunk", "/"]), [2, 2]);
  commit(
    &mut index,
    vec![copied("/branches/b1", NodeKind::Directory, "/trunk", 3)],
  );
  assert_eq!(
    counts(&index, 4, &["/branches/b1", "/branches", "/"]),
    [2, 2, 4]
  );
  commit(&mut index, vec![deleted("/trunk/lib", NodeKind::Directory)]);
  assert_eq!(counts(&index, 5, &["/trunk", "/"]), [1, 3]);
  commit(
    &mut index,
    vec![ChangedPath {
      action: Action::Replace,
      ..add_dir("/branches/b1")
    }],
  );
  assert_eq!(
    counts(&index, 6, &["/branches/b1", "/branches", "/"]),
    [0, 0, 1]
  );
  commit(
    &mut index,
    vec![set_properties(
      "/trunk",
      NodeKind::Directory,
      PropertyList::Empty,
      true,
    )],
  );
  assert_eq!(counts(&index, 7, &["/trunk", "/"]), [0, 0]);
}

/// After an edit on branch b1 in revision 3: its `lib/a.c` is a new node-revision of the source's line (`source`: the
/// source's node-revision and node identities), with b1's copy identity and b1 as its copy root (`branch`: b1's
/// node-revision and copy identities), and so is its `lib` directory.
fn assert_a_new_node_revision_on_the_branch(
  index: &Index,
  (source_id, source_line): (NodeRevisionId, NodeId),
  (branch_id, branch_copy): (NodeRevisionId, CopyId),
) {
  let changed = at(index, "/branches/b1/lib/a.c", 3);
  assert_eq!(
    (changed.created_path(), changed.created_revision()),
    ("/branches/b1/lib/a.c", 3)
  );
  assert_eq!(
    (changed.node_id(), changed.predecessor()),
    (source_line, Some(source_id))
  );
  assert_eq!(changed.copy_id(), branch_copy);
  assert_eq!(changed.copy_root(), branch_id);
  let lib = at(index, "/branches/b1/lib", 3);
  assert_eq!(
    (lib.created_path(), lib.copy_id(), lib.copy_root()),
    ("/branches/b1/lib", branch_copy, branch_id)
  );
}

/// `/d/f` replaced in revision 5 by a copy of its revision 2 shares an ancestor with revision 2, while the plain re-add
/// of revision 4 is unrelated to it.
fn assert_the_replacement_by_copy_relates_and_the_re_add_does_not(index: &Index) {
  assert_eq!(at(index, "/d/f", 5).copy_from(), Some(("/d/f", 2)));
  assert_eq!(
    index.relation(("/d/f", 5), ("/d/f", 2)).unwrap(),
    NodeRelation::CommonAncestor
  );
  assert_eq!(
    index.relation(("/d/f", 4), ("/d/f", 2)).unwrap(),
    NodeRelation::Unrelated
  );
}

/// Revision zero's root as FSFS makes it: a directory created at `/` in revision 0, its own copy root, with no
/// predecessor, the root's node and copy identities, and no text, properties or mergeinfo.
fn assert_an_empty_root_that_is_its_own_copy_root(root: &Node<'_>) {
  assert_eq!(root.kind(), NodeKind::Directory);
  assert_eq!((root.created_path(), root.created_revision()), ("/", 0));
  assert_eq!(root.copy_root(), root.id());
  assert_eq!((root.predecessor(), root.predecessor_count()), (None, 0));
  assert_eq!((root.node_id(), root.copy_id()), (ROOT_NODE, ROOT_COPY));
  assert_eq!(
    (root.text(), root.properties(), root.mergeinfo_count()),
    (None, PropertyList::Empty, 0)
  );
}

/// The revision each of `paths` was last created in, as of `revision`, is `created`.
fn assert_created_in(index: &Index, paths: &[&str], revision: Revision, created: Revision) {
  for path in paths {
    assert_eq!(
      index.created_revision(path, revision).unwrap(),
      Some(created),
      "{path}"
    );
  }
}

/// A-553 (§4.17.4 item 2, "History"): a path's history lists every node-revision created at it and every deletion of
/// it, a replacement once; a directory's lists every revision that changed anything beneath it.
#[test]
fn a_history_list_holds_every_creation_and_deletion_at_its_path() {
  let mut index = Index::new();
  commit(&mut index, vec![add_dir("/d"), add_file("/d/f", 1)]);
  commit(&mut index, vec![edit("/d/f", 2)]);
  let second = at(&index, "/d/f", 2).id();
  commit(&mut index, vec![deleted("/d/f", NodeKind::File)]);
  commit(&mut index, vec![add_file("/d/f", 3)]);
  commit(
    &mut index,
    vec![replaced_by_copy("/d/f", NodeKind::File, "/d/f", 2)],
  );
  let history = index.history("/d/f").unwrap();
  assert_eq!(revisions(history), [1, 2, 3, 4, 5]);
  let events: Vec<bool> = history
    .iter()
    .map(|entry| matches!(entry.event, HistoryEvent::Created(_)))
    .collect();
  assert_eq!(events, [true, true, false, true, true]);
  assert_eq!(history.get(2).unwrap().event, HistoryEvent::Deleted(second));
  assert_eq!(revisions(index.history("/d").unwrap()), [1, 2, 3, 4, 5]);
  assert_eq!(revisions(index.history("/").unwrap()), [0, 1, 2, 3, 4, 5]);
  assert_eq!(index.created_revision("/d/f", 3).unwrap(), None);
  assert_eq!(index.created_revision("/d/f", 4).unwrap(), Some(4));
  assert_the_replacement_by_copy_relates_and_the_re_add_does_not(&index);
  assert!(index.history("/never").unwrap().is_empty());
}

/// A-652 (`tree.c` `copy_helper`, `dag.c` `svn_fs_fs__dag_copy`): do replace `/trunk/README` by a copy of the very
/// node-revision it holds, then `/trunk` by a copy of its own, a revision each, the deletion first as `svnadmin load` and
/// a client's `delete-entry` then `add-file` make it. Expect each a copy, as FSFS makes one once its destination holds
/// no node: its source's line, a fresh copy identity, its own copy root, the node-revision it replaced its predecessor
/// and one more predecessor, created in its revision, related to what it replaced, and what stood beneath the directory
/// standing beneath it again as the same node-revisions.
#[test]
fn a_replacement_by_a_copy_of_the_node_it_replaces_is_a_copy() {
  let mut index = trunk();
  let file = at(&index, "/trunk/README", 1);
  let (file_id, file_line, file_copy, file_count) = (
    file.id(),
    file.node_id(),
    file.copy_id(),
    file.predecessor_count(),
  );
  let second = commit(
    &mut index,
    vec![replaced_by_copy(
      "/trunk/README",
      NodeKind::File,
      "/trunk/README",
      1,
    )],
  );
  assert_a_copy_of(
    &index,
    "/trunk/README",
    second,
    (file_id, file_line, file_copy, file_count),
  );
  assert_eq!(
    index
      .relation(("/trunk/README", second), ("/trunk/README", 1))
      .unwrap(),
    NodeRelation::CommonAncestor
  );
  let directory = at(&index, "/trunk", second);
  let replaced = (
    directory.id(),
    directory.node_id(),
    directory.copy_id(),
    directory.predecessor_count(),
  );
  let a_c = at(&index, "/trunk/lib/a.c", second).id();
  let third = commit(
    &mut index,
    vec![replaced_by_copy(
      "/trunk",
      NodeKind::Directory,
      "/trunk",
      second,
    )],
  );
  assert_a_copy_of(&index, "/trunk", third, replaced);
  assert_eq!(at(&index, "/trunk/lib/a.c", third).id(), a_c);
  assert_eq!(
    revisions(index.history("/trunk/README").unwrap()),
    [1, second]
  );
}

/// The node at `path` in `revision` is a copy made in that revision of the node-revision `replaced` names (its id, node
/// identity, copy identity and predecessor count), copied from its own path in the revision before.
fn assert_a_copy_of(
  index: &Index,
  path: &str,
  revision: Revision,
  (id, line, copy, count): (NodeRevisionId, NodeId, CopyId, u64),
) {
  let node = at(index, path, revision);
  assert_eq!((node.node_id(), node.predecessor()), (line, Some(id)));
  assert_ne!(node.copy_id(), copy);
  assert_eq!(node.copy_root(), node.id());
  assert_eq!(node.predecessor_count(), count + 1);
  assert_eq!(
    (node.created_path(), node.created_revision()),
    (path, revision)
  );
  assert_eq!(node.copy_from(), Some((path, revision - 1)));
}

/// A-553 (module documentation, "Reading"): a deleted directory leaves nothing beneath it, a later copy over the same
/// path brings back its source's node-revisions, and a revision's own replacement of a directory decides what its own
/// later rows find beneath it.
#[test]
fn what_stands_beneath_a_path_follows_the_latest_change_of_its_directories() {
  let mut index = trunk();
  let file = at(&index, "/trunk/lib/a.c", 1).id();
  commit(&mut index, vec![deleted("/trunk", NodeKind::Directory)]);
  assert!(index.node("/trunk/lib/a.c", 2).unwrap().is_none());
  assert_eq!(at(&index, "/trunk/lib/a.c", 1).id(), file);
  commit(
    &mut index,
    vec![copied("/trunk", NodeKind::Directory, "/trunk", 1)],
  );
  assert_eq!(at(&index, "/trunk/lib/a.c", 3).id(), file);
  assert_eq!(index.created_revision("/trunk", 3).unwrap(), Some(3));
  assert_eq!(index.created_revision("/trunk/lib", 3).unwrap(), Some(1));
  commit(
    &mut index,
    vec![
      replaced_by_copy("/branches", NodeKind::Directory, "/trunk", 1),
      edit("/branches/lib/a.c", 9),
      deleted("/branches/README", NodeKind::File),
    ],
  );
  assert_eq!(at(&index, "/branches/lib/a.c", 4).text(), Some(text(9)));
  assert!(index.node("/branches/README", 4).unwrap().is_none());
  assert_eq!(at(&index, "/trunk/README", 4).created_revision(), 1);
}

/// A-553 (`tree.c` `copy_helper`, which refuses no copy into its own subtree): a directory copied beneath itself shares
/// its source's node-revisions there, and a change beneath the copy leaves the source as it was.
#[test]
fn a_copy_into_its_own_subtree_is_kept_as_fsfs_keeps_it() {
  let mut index = trunk();
  commit(
    &mut index,
    vec![copied("/trunk/lib/self", NodeKind::Directory, "/trunk", 1)],
  );
  assert_eq!(
    at(&index, "/trunk/lib/self/lib/a.c", 2).id(),
    at(&index, "/trunk/lib/a.c", 1).id()
  );
  assert!(index.node("/trunk/lib/self/lib/self", 2).unwrap().is_none());
  assert_eq!(
    index
      .relation(("/trunk", 2), ("/trunk/lib/self", 2))
      .unwrap(),
    NodeRelation::CommonAncestor
  );
  commit(&mut index, vec![edit("/trunk/lib/self/lib/a.c", 5)]);
  assert_eq!(
    at(&index, "/trunk/lib/self/lib/a.c", 3).created_revision(),
    3
  );
  assert_eq!(at(&index, "/trunk/lib/a.c", 3).created_revision(), 1);
}

/// A-553 (`svn_fspath__is_canonical`, `svn_fs__path_valid`): a path out of canonical form is refused, in a question,
/// in a row and in a copy source, as `SVN_ERR_FS_PATH_SYNTAX`.
#[test]
fn a_path_out_of_canonical_form_is_refused() {
  let mut index = trunk();
  for path in [
    "",
    "trunk",
    "/trunk/",
    "//trunk",
    "/trunk//lib",
    "/trunk/./lib",
    "/trunk/../lib",
    "/a\nb",
    "/a\0b",
  ] {
    let refusal = index.node(path, 1).unwrap_err();
    assert_eq!(
      refusal,
      IndexRefusal::PathSyntax {
        path: path.to_owned()
      }
    );
    assert_eq!(refusal.subversion_code(), 160_005);
    assert!(index.history(path).is_err());
  }
  let before = format!("{index:?}");
  assert_eq!(
    refused(&mut index, vec![add_dir("/x/")]),
    IndexRefusal::PathSyntax {
      path: "/x/".to_owned()
    }
  );
  assert_eq!(
    refused(
      &mut index,
      vec![copied("/x", NodeKind::Directory, "trunk", 1)]
    ),
    IndexRefusal::PathSyntax {
      path: "trunk".to_owned()
    }
  );
  assert_eq!(format!("{index:?}"), before);
}

/// A-553: a question about a revision past the youngest is refused as `SVN_ERR_FS_NO_SUCH_REVISION`, and so is a copy
/// from the revision it would make.
#[test]
fn a_revision_past_the_youngest_is_refused() {
  let mut index = trunk();
  let refusal = index.node("/", 2).unwrap_err();
  assert_eq!(refusal, IndexRefusal::NoSuchRevision { revision: 2 });
  assert_eq!(refusal.subversion_code(), 160_006);
  assert!(index.changes(2).is_err() && index.transaction(2).is_err());
  assert_eq!(
    refused(
      &mut index,
      vec![copied("/x", NodeKind::Directory, "/trunk", 2)]
    ),
    IndexRefusal::NoSuchRevision { revision: 2 }
  );
  assert_eq!(index.youngest(), 1);
}

/// A-553: a row that the revision its record builds cannot hold is refused, typed with FSFS's own error for the
/// operation, and the index is as it was.
#[test]
fn a_row_its_revision_cannot_hold_is_refused_and_changes_nothing() {
  let file = NodeKind::File;
  let directory = NodeKind::Directory;
  let not_found = |path: &str, revision| IndexRefusal::NotFound {
    path: path.to_owned(),
    revision,
  };
  let cases = [
    (
      vec![edit("/trunk/nope.c", 3)],
      not_found("/trunk/nope.c", 2),
      160_013,
    ),
    (vec![deleted("/nope", file)], not_found("/nope", 2), 160_013),
    (
      vec![ChangedPath {
        action: Action::Replace,
        ..add_file("/nope", 3)
      }],
      not_found("/nope", 2),
      160_013,
    ),
    (vec![add_file("/nope/x", 3)], not_found("/nope", 2), 160_013),
    (
      vec![copied("/x", directory, "/nope", 1)],
      not_found("/nope", 1),
      160_013,
    ),
    (
      vec![deleted("/trunk", directory), edit("/trunk/README", 3)],
      not_found("/trunk", 2),
      160_013,
    ),
    (
      vec![add_file("/trunk/README", 3)],
      IndexRefusal::AlreadyExists {
        path: "/trunk/README".to_owned(),
      },
      160_020,
    ),
    (
      vec![add_file("/trunk/README/x", 3)],
      IndexRefusal::NotDirectory {
        path: "/trunk/README".to_owned(),
      },
      160_016,
    ),
    (
      vec![deleted("/", directory)],
      IndexRefusal::RootChange,
      160_021,
    ),
    (vec![add_dir("/")], IndexRefusal::RootChange, 160_021),
    (
      vec![set_properties("/trunk", file, listed(3, false), false)],
      kind_mismatch("/trunk"),
      160_004,
    ),
    (
      vec![deleted("/trunk", file)],
      kind_mismatch("/trunk"),
      160_004,
    ),
    (
      vec![copied("/x", file, "/trunk", 1)],
      kind_mismatch("/x"),
      160_004,
    ),
    (
      vec![edit("/trunk/README", 3), edit("/trunk/README", 4)],
      IndexRefusal::Duplicate {
        path: "/trunk/README".to_owned(),
      },
      160_004,
    ),
  ];
  for (changes, expected, code) in cases {
    let mut index = trunk();
    let before = format!("{index:?}");
    let refusal = refused(&mut index, changes);
    assert_eq!((&refusal, refusal.subversion_code()), (&expected, code));
    assert_eq!(format!("{index:?}"), before, "{expected:?}");
  }
}

/// The kind refusal at `path`.
fn kind_mismatch(path: &str) -> IndexRefusal {
  IndexRefusal::KindMismatch {
    path: path.to_owned(),
  }
}

/// A-553: a row whose own fields contradict each other is refused before the revision is built, as no FSFS commit
/// writes it; a text on a directory as `SVN_ERR_FS_NOT_FILE`, as FSFS answers `apply-textdelta` on one.
#[test]
fn a_row_whose_fields_contradict_each_other_is_refused() {
  let malformed = |path: &str, shape| IndexRefusal::Malformed {
    path: path.to_owned(),
    shape,
  };
  let cases = [
    (
      ChangedPath {
        text: Some(text(1)),
        ..deleted("/trunk/README", NodeKind::File)
      },
      malformed("/trunk/README", RowShape::DeleteCarriesChange),
    ),
    (
      ChangedPath {
        copy_from: Some(CopySource {
          path: "/trunk".to_owned(),
          revision: 1,
        }),
        ..edit("/trunk/README", 3)
      },
      malformed("/trunk/README", RowShape::ModifyCopies),
    ),
    (
      bare("/trunk/README", Action::Modify, NodeKind::File),
      malformed("/trunk/README", RowShape::ModifyChangesNothing),
    ),
    (
      ChangedPath {
        mergeinfo_modified: true,
        ..edit("/trunk/README", 3)
      },
      malformed("/trunk/README", RowShape::MergeinfoWithoutProperties),
    ),
    (
      set_properties("/trunk/README", NodeKind::File, listed(4, true), false),
      malformed("/trunk/README", RowShape::MergeinfoUnflagged),
    ),
    (
      ChangedPath {
        properties: Some(listed(4, true)),
        ..add_dir("/trunk/new")
      },
      malformed("/trunk/new", RowShape::MergeinfoUnflagged),
    ),
    (
      ChangedPath {
        text: Some(text(1)),
        ..add_dir("/trunk/new")
      },
      IndexRefusal::DirectoryText {
        path: "/trunk/new".to_owned(),
      },
    ),
  ];
  for (change, expected) in cases {
    let mut index = trunk();
    let before = format!("{index:?}");
    let refusal = refused(&mut index, vec![change]);
    let code = if matches!(expected, IndexRefusal::DirectoryText { .. }) {
      160_017
    } else {
      160_004
    };
    assert_eq!((&refusal, refusal.subversion_code()), (&expected, code));
    assert_eq!(format!("{index:?}"), before);
  }
}

/// A-553 (`svn_fs.h`: "Transaction names are guaranteed to contain only letters (upper- and lower-case), digits, `-',
/// and `.'"): a record whose transaction name breaks that promise, or is empty, is refused.
#[test]
fn a_transaction_name_outside_what_svn_fs_h_promises_is_refused() {
  let mut index = trunk();
  for name in ["", "1 2", "1/2", "1_2", "naïve"] {
    let record = RevisionRecord {
      transaction: name.to_owned(),
      changes: Vec::new(),
    };
    let refusal = index.append(record, ALL_ROOM).unwrap_err();
    assert_eq!(
      (&refusal, refusal.subversion_code()),
      (
        &IndexRefusal::TransactionName {
          name: name.to_owned()
        },
        160_004
      )
    );
  }
  let record = RevisionRecord {
    transaction: "41-Zz.9".to_owned(),
    changes: Vec::new(),
  };
  assert_eq!(index.append(record, ALL_ROOM).unwrap(), 2);
  assert_eq!(index.transaction(2).unwrap(), Some("41-Zz.9"));
}

/// A-553 (§2 ban 8: every structure has a bound and a typed refusal at it): a revision that would take the index past
/// the room its caller gives, by one entry, is refused and changes nothing; the same revision with exactly the room
/// it needs is accepted. The edit makes four node-revisions: the file, its two directories and the root.
#[test]
fn an_index_that_would_outgrow_its_room_is_refused_one_entry_past_it() {
  let mut index = trunk();
  let needed = index.entries() + 4;
  let before = format!("{index:?}");
  let record = || RevisionRecord {
    transaction: TRANSACTION.to_owned(),
    changes: vec![edit("/trunk/lib/a.c", 3)],
  };
  let refusal = index.append(record(), needed - 1).unwrap_err();
  assert_eq!(
    refusal,
    IndexRefusal::Room {
      needed,
      room: needed - 1
    }
  );
  assert_eq!(refusal.subversion_code(), 160_000);
  assert_eq!(format!("{index:?}"), before);
  assert_eq!(index.append(record(), needed).unwrap(), 2);
  assert_eq!(index.entries(), needed);
  let deletion = RevisionRecord {
    transaction: TRANSACTION.to_owned(),
    changes: vec![deleted("/trunk/README", NodeKind::File)],
  };
  let after_deletion = needed + 2 + 1;
  assert_eq!(
    index
      .append(deletion.clone(), after_deletion - 1)
      .unwrap_err(),
    IndexRefusal::Room {
      needed: after_deletion,
      room: after_deletion - 1
    }
  );
  assert_eq!(index.append(deletion, after_deletion).unwrap(), 3);
}

/// A-553: a node-revision another index made, or none made, is refused as `SVN_ERR_FS_ID_NOT_FOUND`; a relation to
/// an absent place as `SVN_ERR_FS_NOT_FOUND`.
#[test]
fn an_unknown_node_revision_or_an_absent_place_is_refused() {
  let index = trunk();
  let other = trunk();
  let refusal = Index::new()
    .node_revision(at(&other, "/trunk/lib/a.c", 1).id())
    .unwrap_err();
  assert_eq!(
    (&refusal, refusal.subversion_code()),
    (&IndexRefusal::UnknownNodeRevision, 160_014)
  );
  let refusal = index.relation(("/nope", 1), ("/", 1)).unwrap_err();
  assert_eq!(
    refusal,
    IndexRefusal::NotFound {
      path: "/nope".to_owned(),
      revision: 1
    }
  );
}

// ---------------------------------------------------------------------------------------------------------------------
// The oracle: a naive filesystem.

/// The names generated paths are made of: few, so paths collide and are reused.
const NAMES: [&str; 3] = ["a", "b", "c"];

/// The most paths a generated copy brings, so a history's whole trees stay small enough to compare at every revision.
const COPY_CAP: usize = 24;

/// A node-revision of the naive filesystem, held as FSFS holds one (`structure` 551-588): identities as revision and
/// number, the copy root as a revision and a path.
#[derive(Clone, Debug)]
pub(crate) struct NaiveNode {
  pub(crate) kind: NodeKind,
  pub(crate) node_id: (Revision, u64),
  pub(crate) copy_id: (Revision, u64),
  created_path: String,
  created_revision: Revision,
  predecessor: Option<usize>,
  count: u64,
  copy_from: Option<(String, Revision)>,
  copy_root: (Revision, String),
  text: Option<Text>,
  pub(crate) properties: PropertyList,
}

/// How a naive clone took its copy identity (`tree.c` `copy_id_inherit_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Way {
  Kept,
  Parent,
  Fresh,
}

/// What a history met, so the oracle can show each rule it compared was exercised.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Census {
  /// Clones that kept their copy identity.
  kept: usize,
  /// Clones that took their parent's copy identity where it differed from their own: lazy copies.
  lazy: usize,
  /// Clones that took a fresh copy identity: soft copies.
  soft: usize,
  /// Copies, and copies into their own source's subtree.
  copies: usize,
  self_copies: usize,
  /// Deletions and replacements, and replacements by a copy of the very node-revision they replace.
  deletions: usize,
  replacements: usize,
  replaced_by_itself: usize,
  /// Revisions with no rows.
  empty: usize,
  /// Index answers created at another path than the one asked: paths beneath a copy.
  beneath_copies: usize,
  /// Directories counting `svn:mergeinfo` beneath themselves.
  mergeinfo_beneath: usize,
  /// Relations answered, by kind: unrelated, unchanged, common ancestor.
  relations: [usize; 3],
  /// Revisions whose drive order differed from path order.
  reordered: usize,
  /// Inheritance decisions where FSFS's drive order could matter: a child compared against a parent that this
  /// transaction already cloned with another copy identity, which the other order compares against the parent's old
  /// node-revision (`get_copy_inheritance`'s comparison of the two copy identities).
  sensitive: usize,
  /// Directory listings holding a name the index never named beneath the directory itself, reached through a copy's
  /// source (`Index::children`).
  listed_through_copies: usize,
}

impl Census {
  /// Adds another census's counts.
  fn add(&mut self, other: &Self) {
    self.kept += other.kept;
    self.lazy += other.lazy;
    self.soft += other.soft;
    self.copies += other.copies;
    self.self_copies += other.self_copies;
    self.deletions += other.deletions;
    self.replacements += other.replacements;
    self.replaced_by_itself += other.replaced_by_itself;
    self.empty += other.empty;
    self.beneath_copies += other.beneath_copies;
    self.mergeinfo_beneath += other.mergeinfo_beneath;
    self.reordered += other.reordered;
    self.sensitive += other.sensitive;
    self.listed_through_copies += other.listed_through_copies;
    for (sum, more) in self.relations.iter_mut().zip(other.relations) {
      *sum += more;
    }
  }
}

/// Whether `path` is `directory` or beneath it.
fn at_or_beneath(path: &str, directory: &str) -> bool {
  directory == "/"
    || path == directory
    || path
      .strip_prefix(directory)
      .is_some_and(|rest| rest.starts_with('/'))
}

/// `path`'s directory.
fn parent_path(path: &str) -> &str {
  match path.rsplit_once('/') {
    Some(("", _)) | None => "/",
    Some((parent, _)) => parent,
  }
}

/// `name` in `directory`.
fn joined(directory: &str, name: &str) -> String {
  if directory == "/" {
    format!("/{name}")
  } else {
    format!("{directory}/{name}")
  }
}

/// `path` beneath `from` moved to the same place beneath `to`.
fn moved(path: &str, from: &str, to: &str) -> String {
  if path == from {
    to.to_owned()
  } else if from == "/" {
    format!("{to}{path}")
  } else {
    format!("{to}{}", path.strip_prefix(from).unwrap())
  }
}

/// The naive filesystem: every node-revision ever made and every revision's whole tree, path by path; FSFS's operations
/// applied to them literally (`tree.c`, `dag.c`); and what each revision's deletions removed.
#[derive(Clone, Debug)]
pub(crate) struct NaiveFs {
  pub(crate) nodes: Vec<NaiveNode>,
  pub(crate) trees: Vec<BTreeMap<String, usize>>,
  deleted: Vec<BTreeMap<String, usize>>,
}

impl NaiveFs {
  /// Revision 0: the root alone.
  pub(crate) fn new() -> Self {
    let root = NaiveNode {
      kind: NodeKind::Directory,
      node_id: (0, 0),
      copy_id: (0, 0),
      created_path: "/".to_owned(),
      created_revision: 0,
      predecessor: None,
      count: 0,
      copy_from: None,
      copy_root: (0, "/".to_owned()),
      text: None,
      properties: PropertyList::Empty,
    };
    Self {
      nodes: vec![root],
      trees: vec![BTreeMap::from([("/".to_owned(), 0)])],
      deleted: vec![BTreeMap::new()],
    }
  }

  /// The node at `path` in `revision`, as FSFS reads a copy root (`svn_fs_fs__revision_root`, then `get_dag`).
  fn at(&self, revision: Revision, path: &str) -> usize {
    self.trees[usize::try_from(revision).unwrap()][path]
  }

  /// A transaction on the youngest revision, its root already a new node-revision (`create_new_txn_noderev_from_rev`).
  pub(crate) fn begin(&mut self) -> Txn<'_> {
    let youngest = u64::try_from(self.trees.len() - 1).unwrap();
    self.begin_on(youngest)
  }

  /// A transaction on revision `base`, its root already a new node-revision of `base`'s root, numbered as the next
  /// revision so that no identity it makes equals one a revision holds: a work's transaction on its base (A-592).
  pub(crate) fn begin_on(&mut self, base: Revision) -> Txn<'_> {
    let revision = u64::try_from(self.trees.len()).unwrap();
    let mut tree = self.trees[usize::try_from(base).unwrap()].clone();
    let old_root = tree["/"];
    let mut root = self.nodes[old_root].clone();
    root.predecessor = Some(old_root);
    root.count += 1;
    root.created_revision = revision;
    root.copy_from = None;
    let id = self.nodes.len();
    self.nodes.push(root);
    tree.insert("/".to_owned(), id);
    Txn {
      fs: self,
      revision,
      tree,
      made: BTreeSet::from([id]),
      deleted: BTreeMap::new(),
      next_node: 0,
      next_copy: 0,
      census: Census::default(),
    }
  }

  /// The count of nodes holding `svn:mergeinfo` at and beneath `path` in `revision`, by walking that part of the tree:
  /// every path at or beneath `path` begins with it, and those are one run of the tree's order.
  fn mergeinfo_beneath(&self, revision: Revision, path: &str) -> u64 {
    let tree = &self.trees[usize::try_from(revision).unwrap()];
    let holding = tree
      .range::<str, _>((Bound::Included(path), Bound::Unbounded))
      .take_while(|(place, _)| place.starts_with(path))
      .filter(|(place, node)| {
        at_or_beneath(place, path) && self.nodes[**node].properties.has_mergeinfo()
      })
      .count();
    u64::try_from(holding).unwrap()
  }

  /// Applies one revision's rows in `order`, as FSFS's editor drive calls them.
  fn replay(&mut self, order: &[&ChangedPath]) -> Census {
    let mut txn = self.begin();
    for change in order {
      txn.apply(change);
    }
    txn.commit()
  }
}

/// One element of an opened path: its path, its node when present, and its inheritance as decided when opened.
type Opened = (String, Option<usize>, Way);

/// A transaction of the naive filesystem: its tree as it stands, the node-revisions it made, and what its deletions
/// removed.
pub(crate) struct Txn<'fs> {
  fs: &'fs mut NaiveFs,
  revision: Revision,
  tree: BTreeMap<String, usize>,
  made: BTreeSet<usize>,
  deleted: BTreeMap<String, usize>,
  next_node: u64,
  next_copy: u64,
  census: Census,
}

impl Txn<'_> {
  /// Makes the transaction the next revision; answers what it met.
  pub(crate) fn commit(self) -> Census {
    self.fs.trees.push(self.tree);
    self.fs.deleted.push(self.deleted);
    self.census
  }

  /// The transaction's tree as it stands, never committed: what A-592's oracle merges.
  pub(crate) fn into_tree(self) -> BTreeMap<String, usize> {
    self.tree
  }

  /// Keeps a node-revision this transaction made.
  fn push(&mut self, node: NaiveNode) -> usize {
    let id = self.fs.nodes.len();
    self.fs.nodes.push(node);
    self.made.insert(id);
    id
  }

  /// A fresh node identity.
  fn fresh_node(&mut self) -> (Revision, u64) {
    self.next_node += 1;
    (self.revision, self.next_node - 1)
  }

  /// A fresh copy identity.
  fn fresh_copy(&mut self) -> (Revision, u64) {
    self.next_copy += 1;
    (self.revision, self.next_copy - 1)
  }

  /// `open_path`: every element from the root to `path`, each one's inheritance decided against its parent as the
  /// parent stands now.
  fn open_path(&mut self, path: &str) -> Vec<Opened> {
    let mut opened = vec![("/".to_owned(), self.tree.get("/").copied(), Way::Kept)];
    let mut so_far = String::new();
    for name in path.split('/').filter(|name| !name.is_empty()) {
      so_far = joined(if so_far.is_empty() { "/" } else { &so_far }, name);
      let parent = opened.last().unwrap().1.unwrap();
      let child = self.tree.get(&so_far).copied();
      let way = child.map_or(Way::Kept, |child| self.inheritance(child, parent, &so_far));
      if child.is_some_and(|child| self.sensitive(child, parent)) {
        self.census.sensitive += 1;
      }
      opened.push((so_far.clone(), child, way));
    }
    opened
  }

  /// Whether deciding `child`'s inheritance against `parent` is a decision FSFS's drive order could change: the child
  /// not made here and once copied, the parent cloned here with a copy identity other than its old node-revision's.
  fn sensitive(&self, child: usize, parent: usize) -> bool {
    let parent_node = &self.fs.nodes[parent];
    let moved = parent_node.copy_from.is_none()
      && parent_node
        .predecessor
        .is_some_and(|old| self.fs.nodes[old].copy_id != parent_node.copy_id);
    !self.made.contains(&child)
      && self.made.contains(&parent)
      && self.fs.nodes[child].copy_id != (0, 0)
      && moved
  }

  /// `get_copy_inheritance`, line by line.
  fn inheritance(&self, child: usize, parent: usize, path: &str) -> Way {
    if self.made.contains(&child) {
      return Way::Kept;
    }
    let node = &self.fs.nodes[child];
    if node.copy_id == (0, 0) || node.copy_id == self.fs.nodes[parent].copy_id {
      return Way::Parent;
    }
    let copy_root = self.fs.at(node.copy_root.0, &node.copy_root.1);
    if self.fs.nodes[copy_root].node_id != node.node_id {
      return Way::Parent;
    }
    if node.created_path == path {
      Way::Kept
    } else {
      Way::Fresh
    }
  }

  /// `make_path_mutable` over `opened`, the root first: each element not yet made here becomes a successor of its old
  /// node-revision (`svn_fs_fs__dag_clone_child`).
  fn make_mutable(&mut self, opened: &[Opened]) {
    for pair in opened.windows(2) {
      let (parent_place, (place, node, way)) = (&pair[0].0, &pair[1]);
      let node = node.unwrap();
      if self.made.contains(&node) {
        continue;
      }
      let parent = self.tree[parent_place];
      let old = self.fs.nodes[node].clone();
      let copy_id = match way {
        Way::Kept => old.copy_id,
        Way::Parent => self.fs.nodes[parent].copy_id,
        Way::Fresh => self.fresh_copy(),
      };
      match way {
        Way::Kept => self.census.kept += 1,
        Way::Parent if copy_id != old.copy_id => self.census.lazy += 1,
        Way::Parent => self.census.kept += 1,
        Way::Fresh => self.census.soft += 1,
      }
      let copy_root_node = self.fs.at(old.copy_root.0, &old.copy_root.1);
      let copy_root = if self.fs.nodes[copy_root_node].node_id != old.node_id {
        self.fs.nodes[parent].copy_root.clone()
      } else {
        old.copy_root.clone()
      };
      let clone = NaiveNode {
        copy_id,
        copy_root,
        created_path: place.clone(),
        created_revision: self.revision,
        predecessor: Some(node),
        count: old.count + 1,
        copy_from: None,
        ..old
      };
      let id = self.push(clone);
      self.tree.insert(place.clone(), id);
    }
  }

  /// One row, as the editor call it records.
  fn apply(&mut self, change: &ChangedPath) {
    match change.action {
      Action::Add => self.add(change),
      Action::Delete => {
        self
          .deleted
          .insert(change.path.clone(), self.tree[&change.path]);
        self.delete(&change.path);
      }
      Action::Replace => {
        self.census.replacements += 1;
        let replaced = self.tree[&change.path];
        if let Some(source) = &change.copy_from
          && self.fs.at(source.revision, &source.path) == replaced
        {
          self.census.replaced_by_itself += 1;
        }
        self.delete(&change.path);
        self.add(change);
      }
      Action::Modify => self.modify(change),
    }
  }

  /// `fs_make_dir`, `fs_make_file` or `copy_helper`: the parent made mutable, then the entry made.
  fn add(&mut self, change: &ChangedPath) {
    let opened = self.open_path(&change.path);
    self.make_mutable(&opened[..opened.len() - 1]);
    match &change.copy_from {
      None => self.make_entry(change),
      Some(source) => self.copy(change, source),
    }
  }

  /// `make_entry`: a new line with its parent's copy identity and copy root.
  fn make_entry(&mut self, change: &ChangedPath) {
    let parent = self.fs.nodes[self.tree[parent_path(&change.path)]].clone();
    let node = NaiveNode {
      kind: change.kind,
      node_id: self.fresh_node(),
      copy_id: parent.copy_id,
      created_path: change.path.clone(),
      created_revision: self.revision,
      predecessor: None,
      count: 0,
      copy_from: None,
      copy_root: parent.copy_root,
      text: change.text,
      properties: change.properties.unwrap_or(PropertyList::Empty),
    };
    let id = self.push(node);
    self.tree.insert(change.path.clone(), id);
  }

  /// `svn_fs_fs__dag_copy`: the source's line, a fresh copy identity, its own copy root; beneath it, the source's tree.
  fn copy(&mut self, change: &ChangedPath, source: &CopySource) {
    let source_id = self.fs.at(source.revision, &source.path);
    let from = self.fs.nodes[source_id].clone();
    let node = NaiveNode {
      kind: from.kind,
      node_id: from.node_id,
      copy_id: self.fresh_copy(),
      created_path: change.path.clone(),
      created_revision: self.revision,
      predecessor: Some(source_id),
      count: from.count + 1,
      copy_from: Some((source.path.clone(), source.revision)),
      copy_root: (self.revision, change.path.clone()),
      text: change.text.or(from.text),
      properties: change.properties.unwrap_or(from.properties),
    };
    let id = self.push(node);
    let beneath: Vec<(String, usize)> = self.fs.trees[usize::try_from(source.revision).unwrap()]
      .iter()
      .filter(|(place, _)| place.as_str() != source.path && at_or_beneath(place, &source.path))
      .map(|(place, node)| (moved(place, &source.path, &change.path), *node))
      .collect();
    self
      .tree
      .retain(|place, _| !at_or_beneath(place, &change.path));
    self.tree.extend(beneath);
    self.tree.insert(change.path.clone(), id);
    self.census.copies += 1;
    if at_or_beneath(&change.path, &source.path) {
      self.census.self_copies += 1;
    }
  }

  /// `fs_delete_node`: the parent made mutable, then the path and everything beneath it removed.
  fn delete(&mut self, path: &str) {
    let opened = self.open_path(path);
    self.make_mutable(&opened[..opened.len() - 1]);
    self.tree.retain(|place, _| !at_or_beneath(place, path));
    self.census.deletions += 1;
  }

  /// `fs_apply_textdelta` or `fs_change_node_prop`: the path made mutable, then its text or properties set.
  fn modify(&mut self, change: &ChangedPath) {
    let opened = self.open_path(&change.path);
    self.make_mutable(&opened);
    let node = &mut self.fs.nodes[self.tree[&change.path]];
    if let Some(text) = change.text {
      node.text = Some(text);
    }
    if let Some(properties) = change.properties {
      node.properties = properties;
    }
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// Generated histories.

/// One generated wish for a revision; a transaction turns it into a row when its tree allows one, else drops it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Wish {
  op: u8,
  place: u16,
  source: u16,
  from: u16,
  name: u8,
  mark: u8,
  flags: u8,
}

/// Any wish.
pub(crate) fn wish() -> impl Strategy<Value = Wish> {
  (
    any::<u8>(),
    any::<u16>(),
    any::<u16>(),
    any::<u16>(),
    any::<u8>(),
    any::<u8>(),
    any::<u8>(),
  )
    .prop_map(|(op, place, source, from, name, mark, flags)| Wish {
      op,
      place,
      source,
      from,
      name,
      mark,
      flags,
    })
}

/// Histories of up to nine revisions of up to six wishes each, and the priorities that rank a revision's rows in a
/// drive order.
pub(crate) fn histories() -> impl Strategy<Value = (Vec<Vec<Wish>>, Vec<u32>)> {
  (
    prop::collection::vec(prop::collection::vec(wish(), 0..7), 1..10),
    prop::collection::vec(any::<u32>(), 1..24),
  )
}

/// The `pick`-th of `choices`, if there are any.
fn pick<T: Clone>(choices: &[T], pick: u16) -> Option<T> {
  if choices.is_empty() {
    None
  } else {
    Some(choices[usize::from(pick) % choices.len()].clone())
  }
}

impl Txn<'_> {
  /// The rows a revision's wishes make, each applied as it is made: generation is itself one drive order.
  pub(crate) fn rows_from(&mut self, wishes: &[Wish]) -> Vec<ChangedPath> {
    let mut rows: Vec<ChangedPath> = Vec::new();
    for wish in wishes {
      if let Some(row) = self.row_for(wish, &rows) {
        self.apply(&row);
        rows.push(row);
      }
    }
    rows
  }

  /// The row a wish makes, or none.
  fn row_for(&self, wish: &Wish, rows: &[ChangedPath]) -> Option<ChangedPath> {
    match wish.op % 8 {
      0 => self.new_node(wish, rows, NodeKind::File),
      1 => self.new_node(wish, rows, NodeKind::Directory),
      2 | 3 => self.new_copy(wish, rows, Action::Add),
      4 => self.removal(wish, rows),
      5 => self.replacement(wish, rows),
      _ => self.modification(wish, rows),
    }
  }

  /// The kind of the node at `path` now.
  fn kind(&self, path: &str) -> NodeKind {
    self.fs.nodes[self.tree[path]].kind
  }

  /// An absent path in a present directory that no row of this revision names.
  fn free_place(&self, wish: &Wish, rows: &[ChangedPath]) -> Option<String> {
    let directories: Vec<&String> = self
      .tree
      .keys()
      .filter(|place| self.kind(place) == NodeKind::Directory)
      .collect();
    let path = joined(
      pick(&directories, wish.place)?,
      NAMES[usize::from(wish.name) % NAMES.len()],
    );
    let named = self.tree.contains_key(&path) || rows.iter().any(|row| row.path == path);
    if named { None } else { Some(path) }
  }

  /// A present path other than the root that no row of this revision names, at it or beneath it.
  fn untouched(&self, wish: &Wish, rows: &[ChangedPath]) -> Option<String> {
    let places: Vec<&String> = self
      .tree
      .keys()
      .filter(|place| {
        place.as_str() != "/" && !rows.iter().any(|row| at_or_beneath(&row.path, place))
      })
      .collect();
    pick(&places, wish.place).cloned()
  }

  /// The property list a wish sets, with the mergeinfo flag a stock commit would carry for it given what the node `had`.
  fn wished_properties(wish: &Wish, had: bool) -> (Option<PropertyList>, bool) {
    if wish.flags & 2 == 0 {
      return (None, false);
    }
    let has = wish.flags & 4 != 0;
    let list = if has || wish.flags & 64 == 0 {
      listed(wish.mark, has)
    } else {
      PropertyList::Empty
    };
    (Some(list), had != has || (has && wish.flags & 8 != 0))
  }

  /// A new node without history at a free place.
  fn new_node(&self, wish: &Wish, rows: &[ChangedPath], kind: NodeKind) -> Option<ChangedPath> {
    let path = self.free_place(wish, rows)?;
    let (properties, mergeinfo_modified) = Self::wished_properties(wish, false);
    let text = (kind == NodeKind::File && wish.flags & 1 != 0).then(|| text(wish.mark));
    Some(ChangedPath {
      text,
      properties,
      mergeinfo_modified,
      ..bare(&path, Action::Add, kind)
    })
  }

  /// A copy of a path of an earlier revision, of at most `COPY_CAP` paths, at a free place or over `replacing`.
  fn copy_of(&self, wish: &Wish, path: String, action: Action) -> Option<ChangedPath> {
    let revision = u64::from(wish.from) % self.revision;
    let tree = self.fs.trees.get(usize::try_from(revision).ok()?)?;
    let sources: Vec<&String> = tree.keys().collect();
    let source = pick(&sources, wish.source)?.clone();
    if tree
      .keys()
      .filter(|place| at_or_beneath(place, &source))
      .count()
      > COPY_CAP
    {
      return None;
    }
    let from = &self.fs.nodes[tree[&source]];
    let (properties, mergeinfo_modified) =
      Self::wished_properties(wish, from.properties.has_mergeinfo());
    let text = (from.kind == NodeKind::File && wish.flags & 1 != 0).then(|| text(wish.mark));
    Some(ChangedPath {
      path,
      action,
      kind: from.kind,
      copy_from: Some(CopySource {
        path: source,
        revision,
      }),
      text,
      properties,
      mergeinfo_modified,
    })
  }

  /// An addition by a copy at a free place.
  fn new_copy(&self, wish: &Wish, rows: &[ChangedPath], action: Action) -> Option<ChangedPath> {
    let path = self.free_place(wish, rows)?;
    self.copy_of(wish, path, action)
  }

  /// A deletion of an untouched path.
  fn removal(&self, wish: &Wish, rows: &[ChangedPath]) -> Option<ChangedPath> {
    let path = self.untouched(wish, rows)?;
    Some(deleted(&path, self.kind(&path)))
  }

  /// A replacement of an untouched path by a new node or by a copy, its own node-revision's among them (A-652): the
  /// deletion comes first, so FSFS's `copy_helper` finds no node at the destination and copies.
  fn replacement(&self, wish: &Wish, rows: &[ChangedPath]) -> Option<ChangedPath> {
    let path = self.untouched(wish, rows)?;
    if wish.flags & 16 != 0 {
      return self.copy_of(wish, path, Action::Replace);
    }
    let kind = if wish.flags & 32 != 0 {
      NodeKind::Directory
    } else {
      NodeKind::File
    };
    let (properties, mergeinfo_modified) = Self::wished_properties(wish, false);
    let text = (kind == NodeKind::File && wish.flags & 1 != 0).then(|| text(wish.mark));
    Some(ChangedPath {
      text,
      properties,
      mergeinfo_modified,
      ..bare(&path, Action::Replace, kind)
    })
  }

  /// A modification of a present path no row of this revision names: a text, a property list, or both.
  fn modification(&self, wish: &Wish, rows: &[ChangedPath]) -> Option<ChangedPath> {
    let places: Vec<&String> = self
      .tree
      .keys()
      .filter(|place| !rows.iter().any(|row| &row.path == *place))
      .collect();
    let path = pick(&places, wish.place)?.clone();
    let node = &self.fs.nodes[self.tree[&path]];
    let file = node.kind == NodeKind::File;
    let (mut properties, mut mergeinfo_modified) =
      Self::wished_properties(wish, node.properties.has_mergeinfo());
    let text = (file && (wish.flags & 1 != 0 || properties.is_none())).then(|| text(wish.mark));
    if text.is_none() && properties.is_none() {
      let has = wish.flags & 4 != 0;
      properties = Some(listed(wish.mark, has));
      mergeinfo_modified = node.properties.has_mergeinfo() != has || has;
    }
    Some(ChangedPath {
      text,
      properties,
      mergeinfo_modified,
      ..bare(&path, Action::Modify, node.kind)
    })
  }
}

/// The rows of a revision in a drive order FSFS could have met them in: each after the addition, copy or replacement
/// of every directory above it, otherwise as `priorities` rank them.
fn drive_order<'rows>(rows: &'rows [ChangedPath], priorities: &[u32]) -> Vec<&'rows ChangedPath> {
  let shapes_above = |above: &ChangedPath, row: &ChangedPath| {
    matches!(above.action, Action::Add | Action::Replace)
      && above.path != row.path
      && at_or_beneath(&row.path, &above.path)
  };
  let mut left: Vec<usize> = (0..rows.len()).collect();
  let mut order = Vec::new();
  while !left.is_empty() {
    let ready = left
      .iter()
      .copied()
      .filter(|&row| {
        !left
          .iter()
          .any(|&above| shapes_above(&rows[above], &rows[row]))
      })
      .min_by_key(|&row| (priorities[row % priorities.len()], row))
      .unwrap();
    order.push(&rows[ready]);
    left.retain(|&row| row != ready);
  }
  order
}

// ---------------------------------------------------------------------------------------------------------------------
// The comparison.

/// What both sides answer about a node-revision, each identity as its own side labels it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Answer {
  label: usize,
  node_id: (Revision, u64),
  copy_id: (Revision, u64),
  predecessor: Option<usize>,
  fields: Fields,
}

/// The fields both sides must hold equal outright.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fields {
  kind: NodeKind,
  created_path: String,
  created_revision: Revision,
  count: u64,
  copy_from: Option<(String, Revision)>,
  copy_root: (String, Revision),
  text: Option<Text>,
  properties: PropertyList,
  mergeinfo: u64,
}

/// Labels that must name one thing on both sides, paired as the comparison meets them.
struct Pairing<T> {
  forward: BTreeMap<T, T>,
  backward: BTreeMap<T, T>,
}

impl<T: Ord + Copy + Debug> Pairing<T> {
  fn new() -> Self {
    Self {
      forward: BTreeMap::new(),
      backward: BTreeMap::new(),
    }
  }

  /// Pairs `left` with `right`; refused when either is already paired with another.
  fn pair(&mut self, left: T, right: T, what: &str) -> Result<(), String> {
    match (self.forward.get(&left), self.backward.get(&right)) {
      (None, None) => {
        self.forward.insert(left, right);
        self.backward.insert(right, left);
        Ok(())
      }
      (Some(&paired), Some(&back)) if paired == right && back == left => Ok(()),
      _ => Err(format!(
        "{what}: {left:?} and {right:?} are not one {what} on both sides"
      )),
    }
  }
}

/// The three pairings a comparison keeps.
struct Pairings {
  revisions: Pairing<usize>,
  nodes: Pairing<(Revision, u64)>,
  copies: Pairing<(Revision, u64)>,
}

impl Pairings {
  fn new() -> Self {
    Self {
      revisions: Pairing::new(),
      nodes: Pairing::new(),
      copies: Pairing::new(),
    }
  }

  /// Holds two answers for one place equal: the labels paired, the fields equal.
  fn same(&mut self, left: &Answer, right: &Answer, place: &str) -> Result<(), String> {
    self
      .revisions
      .pair(left.label, right.label, "node-revision")
      .map_err(|why| format!("{place}: {why}"))?;
    self
      .nodes
      .pair(left.node_id, right.node_id, "node identity")
      .map_err(|why| format!("{place}: {why}"))?;
    self
      .copies
      .pair(left.copy_id, right.copy_id, "copy identity")
      .map_err(|why| format!("{place}: {why}"))?;
    match (left.predecessor, right.predecessor) {
      (Some(first), Some(second)) => {
        self
          .revisions
          .pair(first, second, "predecessor")
          .map_err(|why| format!("{place}: {why}"))?;
      }
      (None, None) => {}
      _ => {
        return Err(format!(
          "{place}: predecessors {:?} and {:?}",
          left.predecessor, right.predecessor
        ));
      }
    }
    if left.fields == right.fields {
      Ok(())
    } else {
      Err(format!(
        "{place}: {:?} against {:?}",
        left.fields, right.fields
      ))
    }
  }
}

/// The naive answer at `path` in `revision`, if present.
fn naive_answer(fs: &NaiveFs, path: &str, revision: Revision) -> Option<Answer> {
  let label = *fs.trees.get(usize::try_from(revision).ok()?)?.get(path)?;
  let node = &fs.nodes[label];
  Some(Answer {
    label,
    node_id: node.node_id,
    copy_id: node.copy_id,
    predecessor: node.predecessor,
    fields: Fields {
      kind: node.kind,
      created_path: node.created_path.clone(),
      created_revision: node.created_revision,
      count: node.count,
      copy_from: node.copy_from.clone(),
      copy_root: (node.copy_root.1.clone(), node.copy_root.0),
      text: node.text,
      properties: node.properties,
      mergeinfo: fs.mergeinfo_beneath(revision, path),
    },
  })
}

/// The index's answer at `path` in `revision`, if present; a refusal of the question is the oracle's failure.
fn index_answer(index: &Index, path: &str, revision: Revision) -> Result<Option<Answer>, String> {
  let asked = index.node(path, revision);
  let Some(node) = asked.map_err(|refusal| format!("{path}@{revision}: {refusal:?}"))? else {
    return Ok(None);
  };
  let id = |id: NodeRevisionId| id.0;
  Ok(Some(Answer {
    label: id(node.id()),
    node_id: (node.node_id().revision, node.node_id().number),
    copy_id: (node.copy_id().revision, node.copy_id().number),
    predecessor: node.predecessor().map(id),
    fields: Fields {
      kind: node.kind(),
      created_path: node.created_path().to_owned(),
      created_revision: node.created_revision(),
      count: node.predecessor_count(),
      copy_from: node
        .copy_from()
        .map(|(from, revision)| (from.to_owned(), revision)),
      copy_root: named(index, node.copy_root()),
      text: node.text(),
      properties: node.properties(),
      mergeinfo: node.mergeinfo_count(),
    },
  }))
}

/// Every path any revision of `fs` held.
fn every_path(fs: &NaiveFs) -> BTreeSet<String> {
  fs.trees
    .iter()
    .flat_map(|tree| tree.keys().cloned())
    .collect()
}

/// Holds two naive replays of one history equal at every path of every revision.
fn same_naive(left: &NaiveFs, right: &NaiveFs) -> Result<(), String> {
  let mut pairings = Pairings::new();
  for (position, tree) in left.trees.iter().enumerate() {
    let revision = u64::try_from(position).map_err(|error| error.to_string())?;
    let paths: Vec<&String> = tree.keys().collect();
    let other: Vec<&String> = right
      .trees
      .get(position)
      .ok_or_else(|| format!("revision {revision}: absent from the other replay"))?
      .keys()
      .collect();
    if paths != other {
      return Err(format!(
        "revision {revision}: paths {paths:?} against {other:?}"
      ));
    }
    for path in paths {
      let absent = || format!("{path}@{revision}: listed and absent");
      let first = naive_answer(left, path, revision).ok_or_else(absent)?;
      let second = naive_answer(right, path, revision).ok_or_else(absent)?;
      pairings.same(&first, &second, &format!("{path}@{revision}"))?;
    }
  }
  Ok(())
}

/// The naive history of `path`: each revision whose tree holds a node-revision created there in it, and each revision
/// whose deletion removed it, with the node-revision.
fn naive_history(fs: &NaiveFs, path: &str) -> Vec<(Revision, bool, usize)> {
  let mut history = Vec::new();
  for (position, tree) in fs.trees.iter().enumerate() {
    let revision = u64::try_from(position).unwrap();
    match (tree.get(path), fs.deleted[position].get(path)) {
      (Some(&node), _) if fs.nodes[node].created_revision == revision => {
        history.push((revision, true, node))
      }
      (_, Some(&node)) => history.push((revision, false, node)),
      _ => {}
    }
  }
  history
}

/// Holds the index equal to the naive replay: every path of every revision, every history, and the relations among
/// a sample of places.
fn same_as_index(index: &Index, fs: &NaiveFs, census: &mut Census) -> Result<(), String> {
  let mut pairings = Pairings::new();
  let paths = every_path(fs);
  let mut places = Vec::new();
  for revision in 0..=index.youngest() {
    for path in &paths {
      let place = format!("{path}@{revision}");
      match (
        index_answer(index, path, revision)?,
        naive_answer(fs, path, revision),
      ) {
        (None, None) => {}
        (Some(ours), Some(theirs)) => {
          pairings.same(&ours, &theirs, &place)?;
          census.beneath_copies += usize::from(ours.fields.created_path != *path);
          let own = u64::from(ours.fields.properties.has_mergeinfo());
          census.mergeinfo_beneath +=
            usize::from(ours.fields.kind == NodeKind::Directory && ours.fields.mergeinfo > own);
          places.push((path.clone(), revision, theirs));
        }
        (ours, theirs) => return Err(format!("{place}: index {ours:?} against naive {theirs:?}")),
      }
    }
  }
  for path in &paths {
    same_history(index, fs, path, &mut pairings)?;
  }
  for revision in 0..=index.youngest() {
    same_listings(index, fs, revision, census)?;
  }
  same_relations(index, &places, census)
}

/// The name of `path` when it stands one level beneath `directory`.
fn child_name(directory: &str, path: &str) -> Option<String> {
  let name = path.rsplit('/').next().unwrap_or_default();
  (path != directory && parent_path(path) == directory).then(|| name.to_owned())
}

/// Holds every directory's listing in `revision` equal to the naive tree's (the names one level beneath it, in byte
/// order), a file's and the youngest absent path's none; counts the listings holding a name only a copy's source gave.
fn same_listings(
  index: &Index,
  fs: &NaiveFs,
  revision: Revision,
  census: &mut Census,
) -> Result<(), String> {
  let tree = &fs.trees[usize::try_from(revision).unwrap()];
  for (path, &label) in tree {
    let listed = index
      .children(path, revision)
      .map_err(|refusal| format!("{path}@{revision}: {refusal:?}"))?;
    let expected = (fs.nodes[label].kind == NodeKind::Directory)
      .then(|| tree.keys().filter_map(|other| child_name(path, other)).collect::<Vec<_>>());
    if listed != expected {
      return Err(format!("{path}@{revision}: listed {listed:?}, naive {expected:?}"));
    }
    let through_copy = listed.iter().flatten().any(|name| {
      let child = if path == "/" { format!("/{name}") } else { format!("{path}/{name}") };
      index.history(&child).is_ok_and(<[HistoryEntry]>::is_empty)
    });
    census.listed_through_copies += usize::from(through_copy);
  }
  let absent = index
    .children("/never/named", revision)
    .map_err(|refusal| format!("/never/named@{revision}: {refusal:?}"))?;
  if absent.is_some() {
    return Err(format!("/never/named@{revision} listed {absent:?}"));
  }
  Ok(())
}

/// Holds one path's index history equal to its naive history.
fn same_history(
  index: &Index,
  fs: &NaiveFs,
  path: &str,
  pairings: &mut Pairings,
) -> Result<(), String> {
  let ours: Vec<(Revision, bool, usize)> = index
    .history(path)
    .map_err(|refusal| format!("{path}: {refusal:?}"))?
    .iter()
    .map(|entry| match entry.event {
      HistoryEvent::Created(node) => (entry.revision, true, node.0),
      HistoryEvent::Deleted(node) => (entry.revision, false, node.0),
    })
    .collect();
  let theirs = naive_history(fs, path);
  if ours.len() != theirs.len() {
    return Err(format!("history of {path}: {ours:?} against {theirs:?}"));
  }
  for (mine, other) in ours.iter().zip(&theirs) {
    if (mine.0, mine.1) != (other.0, other.1) {
      return Err(format!("history of {path}: {ours:?} against {theirs:?}"));
    }
    pairings
      .revisions
      .pair(mine.2, other.2, "history's node-revision")?;
  }
  Ok(())
}

/// Holds the index's relation of every pair among a sample of places equal to the naive one.
fn same_relations(
  index: &Index,
  places: &[(String, Revision, Answer)],
  census: &mut Census,
) -> Result<(), String> {
  let step = places.len() / 24 + 1;
  let sample: Vec<&(String, Revision, Answer)> = places.iter().step_by(step).collect();
  for (first_path, first_revision, first) in &sample {
    for (second_path, second_revision, second) in &sample {
      let expected = if first.label == second.label {
        NodeRelation::Unchanged
      } else if first.node_id == second.node_id {
        NodeRelation::CommonAncestor
      } else {
        NodeRelation::Unrelated
      };
      let answered = index
        .relation(
          (first_path, *first_revision),
          (second_path, *second_revision),
        )
        .map_err(|refusal| format!("{refusal:?}"))?;
      if answered != expected {
        return Err(format!(
          "relation of {first_path}@{first_revision} and {second_path}@{second_revision}: {answered:?}, naive {expected:?}"
        ));
      }
      census.relations[expected as usize] += 1;
    }
  }
  Ok(())
}

/// Replays one generated history into the index and three naive filesystems (generation order, path order, and a
/// drive order ranked by `priorities`), and holds them all equal.
fn check_history(
  wishes: &[Vec<Wish>],
  priorities: &[u32],
  census: &mut Census,
) -> Result<(), String> {
  let mut generated = NaiveFs::new();
  let mut path_order = NaiveFs::new();
  let mut drive = NaiveFs::new();
  let mut index = Index::new();
  for revision_wishes in wishes {
    let mut txn = generated.begin();
    let rows = txn.rows_from(revision_wishes);
    census.add(&txn.commit());
    census.empty += usize::from(rows.is_empty());
    let mut sorted = rows.clone();
    sorted.sort_by(|first, second| first.path.cmp(&second.path));
    census.sensitive += path_order
      .replay(&sorted.iter().collect::<Vec<_>>())
      .sensitive;
    let reordered = drive_order(&sorted, priorities);
    census.reordered += usize::from(
      reordered
        .iter()
        .zip(&sorted)
        .any(|(one, other)| one.path != other.path),
    );
    census.sensitive += drive.replay(&reordered).sensitive;
    let record = RevisionRecord {
      transaction: TRANSACTION.to_owned(),
      changes: rows,
    };
    index
      .append(record, ALL_ROOM)
      .map_err(|refusal| format!("refused {refusal:?} of {sorted:?}"))?;
  }
  same_naive(&path_order, &drive).map_err(|why| format!("drive order: {why}"))?;
  same_naive(&path_order, &generated).map_err(|why| format!("generation order: {why}"))?;
  same_as_index(&index, &path_order, census).map_err(|why| format!("index: {why}"))
}

/// A-553 (§4.17.4 piece 4, the oracle): on generated histories (additions, copies of any earlier revision's paths,
/// copies into their own subtrees, deletions, replacements, text and property changes, `svn:mergeinfo` set and
/// dropped, empty revisions), the index equals a naive filesystem replaying the same revisions with FSFS's operations
/// applied literally to whole trees: at every path of every revision the same presence, created path and revision,
/// predecessor and count, copy source, copy root, text, properties and mergeinfo count (the naive one counted by
/// walking the tree), with node-revisions, node identities and copy identities paired one to one; the same history
/// list at every path; and the same relation for every pair among a sample of places. The naive filesystem replaying
/// each revision's rows in random drive orders, and in the order they were generated, equals its own replay in path
/// order. The census shows every rule was met: kept, lazy and soft copy identities, copies and copies into their own
/// subtrees, deletions, replacements, empty revisions, paths reached beneath a copy, directories counting mergeinfo
/// beneath them, all three relations, drive orders that differ from path order, and the inheritance decisions where
/// FSFS's drive order could matter; every run prints it.
#[test]
#[allow(
  clippy::print_stderr,
  reason = "the census is the oracle's evidence, printed with every run"
)]
fn the_index_equals_a_naive_filesystem_replaying_the_same_revisions() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config::default()));
  let result = runner.run(&histories(), |(wishes, priorities)| {
    check_history(&wishes, &priorities, &mut census.borrow_mut()).map_err(TestCaseError::fail)
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  eprintln!("index oracle census: {census:?}");
  let met = [
    census.kept,
    census.lazy,
    census.soft,
    census.copies,
    census.self_copies,
    census.deletions,
    census.replacements,
    census.replaced_by_itself,
    census.empty,
    census.beneath_copies,
    census.mergeinfo_beneath,
    census.reordered,
    census.sensitive,
    census.listed_through_copies,
  ];
  assert!(
    met.iter().all(|&count| count > 0),
    "a rule never met: {census:?}"
  );
  assert!(
    census.relations.iter().all(|&count| count > 0),
    "a relation never met: {census:?}"
  );
}

/// A-655 (`Index::plan`, `Index::commit_planned`; `transaction.c` `commit_body` 3784-3786). Do: plan a change of
/// `/trunk/README` after revision 1, append another revision, then commit the plan; plan the change again and commit
/// it. Expect: the stale plan refused out of date (`E160028`), the index as the append left it; the fresh plan
/// committed as revision 3, the index equal to the one `append` makes of the same record.
#[test]
fn a_plan_is_refused_once_a_revision_was_appended_after_it() {
  let mut index = trunk();
  let record = || RevisionRecord {
    transaction: TRANSACTION.to_owned(),
    changes: vec![edit("/trunk/README", 3)],
  };
  let stale = index.plan(record(), ALL_ROOM).unwrap();
  commit(&mut index, vec![edit("/trunk/lib/a.c", 4)]);
  let before = format!("{index:?}");
  let refusal = index.commit_planned(stale).unwrap_err();
  let answered = (refusal.subversion_code(), format!("{index:?}"));
  assert_eq!(
    refusal,
    IndexRefusal::OutOfDate {
      planned: 2,
      next: 3
    }
  );
  assert_eq!(answered, (160_028, before));
  let mut appended = index.clone();
  let fresh = index.plan(record(), ALL_ROOM).unwrap();
  assert_eq!(index.commit_planned(fresh), Ok(3));
  assert_eq!(appended.append(record(), ALL_ROOM), Ok(3));
  assert_eq!(format!("{index:?}"), format!("{appended:?}"));
}
