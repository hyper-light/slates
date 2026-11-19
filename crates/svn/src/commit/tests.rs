//! The commit rule's tests (A-592): one case for each rule of §4.17.4 item 3.3, the order its conflicts are answered
//! in, the lock table's check after the merge (item 3.4), the drive's out-of-date test (item 3.2), and the oracle:
//! `tree.c` `merge()` and `transaction.c` `verify_locks` transcribed over the naive FSFS of the index's tests, which must
//! answer every generated history as the verdict does with the lock table's check (A-602).

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::*;
use crate::index::tests::{ALL_ROOM, NaiveFs, TRANSACTION, Wish, histories, wish};
use crate::index::{Action, CopySource, Text};
use crate::lock::{AprTime, CommitLocks, Lock, LockEntry, LockTable, LockToken, TOKEN_SCHEME};

/// A text named by one byte.
fn text(mark: u8) -> Text {
  Text {
    identity: [mark; 32],
    md5: [mark; 16],
    sha1: [mark; 20],
    size: u64::from(mark),
  }
}

/// A property list named by one byte, without `svn:mergeinfo`.
fn listed(mark: u8) -> PropertyList {
  PropertyList::Held {
    identity: [mark; 32],
    has_mergeinfo: false,
  }
}

/// A row naming its path, action and kind, and nothing else.
fn row(path: &str, action: Action, kind: NodeKind) -> ChangedPath {
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

/// A new directory at `path`.
fn add_dir(path: &str) -> ChangedPath {
  row(path, Action::Add, NodeKind::Directory)
}

/// A new file at `path` holding the text `mark`.
fn add_file(path: &str, mark: u8) -> ChangedPath {
  ChangedPath {
    text: Some(text(mark)),
    ..row(path, Action::Add, NodeKind::File)
  }
}

/// The file at `path` given the text `mark`.
fn edit(path: &str, mark: u8) -> ChangedPath {
  ChangedPath {
    text: Some(text(mark)),
    ..row(path, Action::Modify, NodeKind::File)
  }
}

/// The directory at `path` given the property list `mark`.
fn set_properties(path: &str, mark: u8) -> ChangedPath {
  ChangedPath {
    properties: Some(listed(mark)),
    ..row(path, Action::Modify, NodeKind::Directory)
  }
}

/// A copy of the directory `from` in `revision`, added at `path`.
fn copied(path: &str, from: &str, revision: Revision) -> ChangedPath {
  ChangedPath {
    copy_from: Some(CopySource {
      path: from.to_owned(),
      revision,
    }),
    ..add_dir(path)
  }
}

/// A record of `changes`: a revision's, or a work's.
fn work(changes: Vec<ChangedPath>) -> RevisionRecord {
  RevisionRecord {
    transaction: TRANSACTION.to_owned(),
    changes,
  }
}

/// A repository whose revisions from 1 on hold `revisions`' rows.
fn repository(revisions: Vec<Vec<ChangedPath>>) -> Index {
  let mut index = Index::new();
  for changes in revisions {
    index.append(work(changes), ALL_ROOM).unwrap();
  }
  index
}

/// The verdict on a work of `changes` built on `base`, with no lock anywhere.
fn judged(index: &Index, base: Revision, changes: Vec<ChangedPath>) -> Result<(), CommitRefusal> {
  verdict(index, base, &work(changes), |_| Ok(()))
}

/// The refusal of a conflict at `path` found by `kind`.
fn conflict_at(path: &str, kind: ConflictKind) -> Result<(), CommitRefusal> {
  Err(CommitRefusal::Conflict {
    path: path.to_owned(),
    kind,
  })
}

/// Revision 1 of most cases: `/trunk` holding the files `a` and `b`.
fn trunk() -> Vec<ChangedPath> {
  vec![
    add_dir("/trunk"),
    add_file("/trunk/a", 1),
    add_file("/trunk/b", 2),
  ]
}

/// The committing principal, when a commit has one.
const COMMITTER: &str = "carol";

/// The principal who holds the locks that are not the committer's.
const OTHER: &str = "dan";

/// The lock token named by `mark`.
fn token(mark: u8) -> LockToken {
  LockToken::supplied(&format!("{TOKEN_SCHEME}{mark:02x}")).unwrap()
}

/// A lock on `path` held by `owner`, its token named by `mark`, never expiring.
fn lock_on(path: &str, owner: &str, mark: u8) -> Lock {
  Lock {
    path: path.to_owned(),
    token: token(mark),
    owner: owner.to_owned(),
    comment: None,
    is_dav_comment: false,
    created: AprTime(0),
    expires: None,
  }
}

/// A lock table holding `locks`, as the register's entries set them.
fn table_of(locks: &[Lock]) -> LockTable {
  let mut table = LockTable::new();
  for lock in locks {
    table.apply(&LockEntry::Set(lock.clone())).unwrap();
  }
  table
}

/// A commit by `user` presenting the tokens of `presented`, its locks released when it is accepted.
fn commit_by(user: Option<&str>, presented: &[&Lock]) -> CommitLocks {
  CommitLocks {
    user: user.map(str::to_owned),
    presented: presented
      .iter()
      .map(|lock| (lock.path.clone(), lock.token.as_str().to_owned()))
      .collect(),
    keep: false,
  }
}

/// The verdict on a work of `changes` built on `base`, its lock check the lock table's for `commit`, as the merge task
/// passes it (`LockTable::verdict` over each change's path and action, at the owner's instant).
fn judged_with_locks(
  index: &Index,
  base: Revision,
  changes: Vec<ChangedPath>,
  (table, commit): (&LockTable, &CommitLocks),
) -> Result<Vec<LockEntry>, CommitRefusal> {
  verdict(index, base, &work(changes), |changes| {
    let paths = changes.iter().map(|row| (row.path.as_str(), row.action));
    table.verdict(paths, commit, AprTime(0))
  })
}

/// A-592 (§4.17.4 item 3.3 rule 5; the worked example's Carol and Dan). Do: commit a work on the head, and a work on
/// revision 1 that edits `/trunk/a` after revision 2 edited `/trunk/b`. Expect: both commit.
#[test]
fn a_work_commits_on_the_head_and_beside_a_change_made_since_its_base() {
  let index = repository(vec![trunk()]);
  assert_eq!(judged(&index, 1, vec![edit("/trunk/a", 7)]), Ok(()));
  let index = repository(vec![trunk(), vec![edit("/trunk/b", 8)]]);
  assert_eq!(judged(&index, 1, vec![edit("/trunk/a", 7)]), Ok(()));
}

/// A-592 (rule 1: "If any of the three entries is of type file, declare a conflict", `tree.c` 2052). Do: edit
/// `/trunk/a` on revision 1 after revision 2 edited it, once with another text and once with revision 2's own. Expect:
/// both refused at `/trunk/a`, `E160024`: no identity accept.
#[test]
fn a_file_both_sides_changed_conflicts_even_when_both_made_the_same_change() {
  let index = repository(vec![trunk(), vec![edit("/trunk/a", 8)]]);
  for mark in [7, 8] {
    let answer = judged(&index, 1, vec![edit("/trunk/a", mark)]);
    assert_eq!(answer, conflict_at("/trunk/a", ConflictKind::FileChanged));
    assert_eq!(answer.unwrap_err().subversion_code(), 160_024);
  }
}

/// A-592 (rule 3, `tree.c` 1946, Subversion's issue #2751). Do: edit `/trunk/a` on revision 1 after revision 2 set
/// `/trunk`'s properties, and after revision 2 set the root's. Expect: refused at the directory whose properties
/// changed.
#[test]
fn a_directory_whose_properties_the_head_changed_refuses_a_change_beneath_it() {
  for directory in ["/trunk", "/"] {
    let index = repository(vec![trunk(), vec![set_properties(directory, 5)]]);
    assert_eq!(
      judged(&index, 1, vec![edit("/trunk/a", 7)]),
      conflict_at(directory, ConflictKind::PropertiesChanged)
    );
  }
}

/// A-592 (rule 4, `compare_dir_structure`; the worked example's Frank and Gina). Do: set `/trunk`'s properties on
/// revision 1 after revision 2 edited `/trunk/b` in place, and after revision 2 added `/trunk/c`. Expect: the first
/// commits; the second is refused at `/trunk`.
#[test]
fn a_directory_property_change_meets_an_added_entry_but_not_an_entry_changed_in_place() {
  let in_place = repository(vec![trunk(), vec![edit("/trunk/b", 8)]]);
  assert_eq!(
    judged(&in_place, 1, vec![set_properties("/trunk", 5)]),
    Ok(())
  );
  let added = repository(vec![trunk(), vec![add_file("/trunk/c", 9)]]);
  assert_eq!(
    judged(&added, 1, vec![set_properties("/trunk", 5)]),
    conflict_at("/trunk", ConflictKind::StructureChanged)
  );
}

/// A-592 (rule 2: a double delete, a delete against a modification; `tree.c` 2046). Do: on revision 1, delete
/// `/trunk/a` after revision 2 deleted it; edit it after revision 2 deleted it; delete it after revision 2 edited it.
/// Expect: each refused at `/trunk/a`, the first a double delete, the others a delete against a change.
#[test]
fn a_double_delete_and_a_delete_against_a_change_conflict() {
  let deletion = || row("/trunk/a", Action::Delete, NodeKind::File);
  let deleted_since = repository(vec![trunk(), vec![deletion()]]);
  assert_eq!(
    judged(&deleted_since, 1, vec![deletion()]),
    conflict_at("/trunk/a", ConflictKind::DeletedTwice)
  );
  assert_eq!(
    judged(&deleted_since, 1, vec![edit("/trunk/a", 7)]),
    conflict_at("/trunk/a", ConflictKind::DeletedAgainstChange)
  );
  let edited_since = repository(vec![trunk(), vec![edit("/trunk/a", 8)]]);
  assert_eq!(
    judged(&edited_since, 1, vec![deletion()]),
    conflict_at("/trunk/a", ConflictKind::DeletedAgainstChange)
  );
}

/// A-592 (a node kind conflict, `tree.c` 2052). Do: edit the file `/trunk/a` on revision 1 after revision 2 replaced
/// it with a directory. Expect: refused at `/trunk/a`, its three kinds differing.
#[test]
fn an_entry_one_side_replaced_with_another_kind_is_a_node_kind_conflict() {
  let index = repository(vec![
    trunk(),
    vec![row("/trunk/a", Action::Replace, NodeKind::Directory)],
  ]);
  assert_eq!(
    judged(&index, 1, vec![edit("/trunk/a", 7)]),
    conflict_at("/trunk/a", ConflictKind::KindChanged)
  );
}

/// A-592 (rule 2: "A replacement is incompatible with a modification or other replacement--even an identical
/// replacement", `tree.c` 1886-1889). Do: on revision 1, edit `/trunk/d/f` after revision 2 replaced `/trunk/d` with a
/// new directory; and replace `/trunk/d` with a new directory as revision 2 did. Expect: both refused at `/trunk/d`.
#[test]
fn a_replaced_directory_is_no_direct_modification_even_when_both_replace_it_alike() {
  let replacement = || row("/trunk/d", Action::Replace, NodeKind::Directory);
  let index = repository(vec![
    vec![
      add_dir("/trunk"),
      add_dir("/trunk/d"),
      add_file("/trunk/d/f", 1),
    ],
    vec![replacement()],
  ]);
  assert_eq!(
    judged(&index, 1, vec![edit("/trunk/d/f", 7)]),
    conflict_at("/trunk/d", ConflictKind::NotADirectModification)
  );
  assert_eq!(
    judged(&index, 1, vec![replacement()]),
    conflict_at("/trunk/d", ConflictKind::NotADirectModification)
  );
}

/// A-592 (a lazy copy, `tree.c` `get_copy_inheritance`: a node never copied, made mutable beneath a copy, takes the
/// copy's copy identity). Do: revision 2 copies `/trunk` to `/branch` and revision 3 edits `/branch/d/x`; a work on
/// revision 2 edits `/branch/d/y`. The same on trunk: revision 3 edits `/trunk/d/x`, a work on revision 2 edits
/// `/trunk/d/y`. Expect: the branch's work refused at `/branch/d`, which both sides first changed inside the branch and
/// so took the branch's copy identity where the base's has trunk's, as in FSFS; the trunk's work commits.
#[test]
fn two_works_beneath_a_directory_a_branch_has_not_changed_since_its_copy_conflict_there() {
  let made = vec![
    add_dir("/trunk"),
    add_dir("/trunk/d"),
    add_file("/trunk/d/x", 1),
    add_file("/trunk/d/y", 2),
  ];
  let branched = vec![copied("/branch", "/trunk", 1)];
  let on_branch = repository(vec![
    made.clone(),
    branched.clone(),
    vec![edit("/branch/d/x", 8)],
  ]);
  assert_eq!(
    judged(&on_branch, 2, vec![edit("/branch/d/y", 7)]),
    conflict_at("/branch/d", ConflictKind::NotADirectModification)
  );
  let on_trunk = repository(vec![made, branched, vec![edit("/trunk/d/x", 8)]]);
  assert_eq!(judged(&on_trunk, 2, vec![edit("/trunk/d/y", 7)]), Ok(()));
}

/// A-592 (rule 2: "Even if SOURCE and TARGET are adding exactly the same thing, two additions are not
/// auto-mergeable", `tree.c` 1898-1900). Do: add `/trunk/c` on revision 1 with the very text revision 2 added there.
/// Expect: refused at `/trunk/c`.
#[test]
fn an_entry_both_sides_added_conflicts_even_when_alike() {
  let index = repository(vec![trunk(), vec![add_file("/trunk/c", 9)]]);
  assert_eq!(
    judged(&index, 1, vec![add_file("/trunk/c", 9)]),
    conflict_at("/trunk/c", ConflictKind::AddedTwice)
  );
}

/// A-592 (the first conflict in `merge()`'s order is the one answered). Do: on revision 1, repeat revision 2's addition
/// of `/trunk/0` and edit of `/trunk/b`; then, in another history, repeat revision 2's edits of `/trunk/d/f` and
/// `/trunk/d-x`. Expect: `/trunk/b`, since a directory's base entries are judged before the head's additions whatever
/// their names; and `/trunk/d/f`, since a directory is judged whole before the names after it, though byte order puts
/// `/trunk/d-x` first.
#[test]
fn the_first_conflict_in_fsfs_order_is_the_one_answered() {
  let made = vec![
    add_dir("/trunk"),
    add_file("/trunk/b", 1),
    add_dir("/trunk/d"),
    add_file("/trunk/d/f", 2),
    add_file("/trunk/d-x", 3),
  ];
  let both = vec![add_file("/trunk/0", 9), edit("/trunk/b", 8)];
  let index = repository(vec![made.clone(), both.clone()]);
  assert_eq!(
    judged(&index, 1, both),
    conflict_at("/trunk/b", ConflictKind::FileChanged)
  );
  let both = vec![edit("/trunk/d/f", 8), edit("/trunk/d-x", 8)];
  let index = repository(vec![made, both.clone()]);
  assert_eq!(
    judged(&index, 1, both),
    conflict_at("/trunk/d/f", ConflictKind::FileChanged)
  );
}

/// A-592 (§4.17.4 item 3.4; FSFS checks the locks after its merge, `transaction.c` 3797). Do: commit a work on the head
/// that edits `/a/b` and deletes `/c`, its lock check the lock table's: with `/a/b` locked by another principal; with
/// `/c/d`, beneath the deleted `/c`, locked by the committer and its token not presented; with that lock and no
/// principal; and with both locks the committer's and their tokens presented. Expect: refused at `/a/b` (`E160039`), at
/// `/c/d` (`E160037`) and for no principal (`E160034`), each the lock table's own refusal; the last accepted with the
/// table's entries, which release both presented locks.
#[test]
fn the_lock_tables_check_answers_for_a_work_the_merge_accepts() {
  let index = repository(vec![vec![
    add_dir("/a"),
    add_file("/a/b", 1),
    add_dir("/c"),
    add_file("/c/d", 2),
  ]]);
  let changes = || {
    vec![
      edit("/a/b", 5),
      row("/c", Action::Delete, NodeKind::Directory),
    ]
  };
  let others = table_of(&[lock_on("/a/b", OTHER, 1)]);
  let refused = judged_with_locks(
    &index,
    1,
    changes(),
    (&others, &commit_by(Some(COMMITTER), &[])),
  );
  assert_eq!(
    refused,
    Err(CommitRefusal::Locked(LockRefusal::OwnerMismatch {
      path: "/a/b".to_owned(),
      owner: OTHER.to_owned(),
    }))
  );
  assert_eq!(refused.unwrap_err().subversion_code(), 160_039);
  let mine = table_of(&[lock_on("/c/d", COMMITTER, 2)]);
  let refused = judged_with_locks(
    &index,
    1,
    changes(),
    (&mine, &commit_by(Some(COMMITTER), &[])),
  );
  assert_eq!(
    refused,
    Err(CommitRefusal::Locked(LockRefusal::TokenNotPresented {
      path: "/c/d".to_owned(),
    }))
  );
  assert_eq!(refused.unwrap_err().subversion_code(), 160_037);
  let refused = judged_with_locks(&index, 1, changes(), (&mine, &commit_by(None, &[])));
  assert_eq!(
    refused,
    Err(CommitRefusal::Locked(LockRefusal::NoUser {
      path: Some("/c/d".to_owned()),
    }))
  );
  assert_eq!(refused.unwrap_err().subversion_code(), 160_034);
  let both = [lock_on("/a/b", COMMITTER, 1), lock_on("/c/d", COMMITTER, 2)];
  let commit = commit_by(Some(COMMITTER), &[&both[0], &both[1]]);
  let released: Vec<LockEntry> = both
    .iter()
    .map(|lock| LockEntry::Removed {
      path: lock.path.clone(),
      token: lock.token.clone(),
    })
    .collect();
  assert_eq!(
    judged_with_locks(&index, 1, changes(), (&table_of(&both), &commit)),
    Ok(released)
  );
}

/// A-592 (FSFS's merge refuses before its locks are verified: `merge_changes`, `tree.c` 2284, before `commit_body`'s
/// `verify_locks`, `transaction.c` 3797). Do: after revision 2 edited `/a/b`, commit a work on revision 1 that edits it
/// too, counting the lock check's calls. Expect: the conflict at `/a/b` answers, and the lock check is never asked.
#[test]
fn a_conflict_answers_before_the_lock_check_is_asked() {
  let index = repository(vec![
    vec![add_dir("/a"), add_file("/a/b", 1)],
    vec![edit("/a/b", 8)],
  ]);
  let mut asked = 0_usize;
  let answer = verdict(
    &index,
    1,
    &work(vec![edit("/a/b", 5)]),
    |_| -> Result<(), LockRefusal> {
      asked += 1;
      Ok(())
    },
  );
  assert_eq!(answer, conflict_at("/a/b", ConflictKind::FileChanged));
  assert_eq!(asked, 0);
}

/// A-592 (§4.17.4 item 3.2; `commit.c` `check_out_of_date` 179-198; the worked example's Carol and Eve). Do: on a
/// repository whose `/trunk/core.c` was created in revision 1 and edited in 2 while revision 3 changed another file,
/// test a transaction on revision 3 with `open-file` from revisions 1, 2 and 4, the created revision read from the
/// index; then a node the transaction made, from revisions 1 and 4. Expect: 1 out of date (`E160028`, a file's words),
/// 2 goes on, 4 no such revision (`E160006`); for the made node 1 goes on and 4 is no such revision.
#[test]
fn out_of_date_is_a_clients_revision_older_than_the_nodes_created_revision_in_the_transaction() {
  let path = "/trunk/core.c";
  let index = repository(vec![
    vec![
      add_dir("/trunk"),
      add_file(path, 1),
      add_file("/trunk/fork.c", 2),
    ],
    vec![edit(path, 3)],
    vec![edit("/trunk/fork.c", 4)],
  ]);
  let created = index.created_revision(path, 3).unwrap();
  let opened = |client| check_out_of_date(path, NodeKind::File, client, created, 3);
  let stale = opened(1);
  assert_eq!(
    stale,
    Err(CommitRefusal::OutOfDate {
      path: path.to_owned(),
      kind: Some(NodeKind::File),
    })
  );
  assert_eq!(stale.unwrap_err().subversion_code(), 160_028);
  assert_eq!(opened(2), Ok(()));
  let past = opened(4);
  assert_eq!(past, Err(CommitRefusal::RevisionPastBase { revision: 4 }));
  assert_eq!(past.unwrap_err().subversion_code(), 160_006);
  let made = |client| check_out_of_date("/trunk", NodeKind::Directory, client, None, 3);
  assert_eq!(made(1), Ok(()));
  assert_eq!(
    made(4),
    Err(CommitRefusal::RevisionPastBase { revision: 4 })
  );
}

/// A-592. Do: judge a work that deletes a path its base does not hold, and a work built on a revision past the head.
/// Expect: each refused with the index's refusal and its error (`E160013`, `E160006`).
#[test]
fn a_work_its_base_cannot_hold_is_refused_with_the_indexs_refusal() {
  let index = repository(vec![trunk()]);
  let absent = judged(
    &index,
    1,
    vec![row("/trunk/z", Action::Delete, NodeKind::File)],
  );
  assert_eq!(
    absent,
    Err(CommitRefusal::Index(IndexRefusal::NotFound {
      path: "/trunk/z".to_owned(),
      revision: 2,
    }))
  );
  assert_eq!(absent.unwrap_err().subversion_code(), 160_013);
  let past = judged(&index, 2, vec![edit("/trunk/a", 7)]);
  assert_eq!(
    past,
    Err(CommitRefusal::Index(IndexRefusal::NoSuchRevision {
      revision: 2
    }))
  );
  assert_eq!(past.unwrap_err().subversion_code(), 160_006);
}

// ---------------------------------------------------------------------------------------------------------------------
// The oracle: FSFS's commit transcribed over the naive filesystem's whole trees.

/// A tree of the naive filesystem: every path's node.
type Tree = BTreeMap<String, usize>;

/// What `merge()` answers: nothing to refuse, or a conflict's path and the check that found it.
type Merged = Result<(), (String, ConflictKind)>;

/// The three trees `merge()` reads: the transaction's, the head's and the base's.
#[derive(Clone, Copy)]
struct Trees<'tree> {
  /// The transaction's tree.
  target: &'tree Tree,
  /// The head's tree.
  source: &'tree Tree,
  /// The base's tree.
  ancestor: &'tree Tree,
}

/// What the verdict and the oracle answer alike.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Answer {
  /// The work commits.
  Commits,
  /// A conflict at a path, found by a check.
  Conflict(String, ConflictKind),
  /// A lock in the way.
  Locked(LockRefusal),
}

/// `dirent_uri.c` `svn_fspath__skip_ancestor` (2664) through `svn_relpath_skip_ancestor` (1530-1551), transcribed:
/// whether `child` is `parent` or beneath it.
fn svn_fspath_skip_ancestor(parent: &str, child: &str) -> bool {
  let (Some(parent), Some(child)) = (parent.strip_prefix('/'), child.strip_prefix('/')) else {
    return false;
  };
  if parent.is_empty() {
    return true;
  }
  if !child.starts_with(parent) {
    return false;
  }
  matches!(child.as_bytes().get(parent.len()), None | Some(b'/'))
}

/// `path.c` `svn_path_compare_paths` (418-453), transcribed byte by byte, a missing byte read as C's terminating NUL.
fn svn_path_compare_paths(path1: &str, path2: &str) -> Ordering {
  let (one, two) = (path1.as_bytes(), path2.as_bytes());
  // "Skip past common prefix."
  let common = one
    .iter()
    .zip(two)
    .take_while(|(first, second)| first == second)
    .count();
  // "Are the paths exactly the same?"
  if one.len() == two.len() && common >= one.len().min(two.len()) {
    return Ordering::Equal;
  }
  let at = |bytes: &[u8]| bytes.get(common).copied().unwrap_or(0);
  // "Children of paths are greater than their parents, but less than greater siblings of their parents.", then the
  // next byte, unsigned.
  match (at(one), at(two)) {
    (b'/', 0) => Ordering::Greater,
    (0, b'/') | (b'/', _) => Ordering::Less,
    (_, b'/') => Ordering::Greater,
    (first, second) if first < second => Ordering::Less,
    _ => Ordering::Greater,
  }
}

/// The entries of the directory at `path` in `tree`, by name in byte order (FSFS's sorted entries), each its node; the
/// root, the one path its own prefix leaves empty, is no entry of itself.
fn entries<'tree>(tree: &'tree Tree, path: &str) -> BTreeMap<&'tree str, usize> {
  let prefix = if path == "/" {
    "/".to_owned()
  } else {
    format!("{path}/")
  };
  tree
    .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
    .take_while(|(place, _)| place.starts_with(&prefix))
    .filter_map(|(place, node)| {
      let name = place.strip_prefix(prefix.as_str())?;
      (!name.is_empty() && !name.contains('/')).then_some((name, *node))
    })
    .collect()
}

/// `name` in the directory `path`, as `svn_fspath__join` joins them.
fn joined_path(path: &str, name: &str) -> String {
  if path == "/" {
    format!("/{name}")
  } else {
    format!("{path}/{name}")
  }
}

/// Whether the naive node `entry` has the node and copy identities of `ancestor` (`tree.c` 2061-2069).
fn same_line(fs: &NaiveFs, entry: usize, ancestor: usize) -> bool {
  let (entry, ancestor) = (&fs.nodes[entry], &fs.nodes[ancestor]);
  entry.node_id == ancestor.node_id && entry.copy_id == ancestor.copy_id
}

/// `tree.c` `compare_dir_structure` (1724-1765), transcribed: whether `lhs` and `rhs`, the directory at `path` in two
/// trees, differ in their count of entries, or at one position in a name, a node identity or a copy identity.
fn compare_dir_structure(fs: &NaiveFs, (lhs, rhs): (&Tree, &Tree), path: &str) -> bool {
  let (lhs, rhs) = (entries(lhs, path), entries(rhs, path));
  if lhs.len() != rhs.len() {
    return true;
  }
  lhs
    .iter()
    .zip(rhs.iter())
    .any(|((lhs_name, lhs_node), (rhs_name, rhs_node))| {
      lhs_name != rhs_name || !same_line(fs, *lhs_node, *rhs_node)
    })
}

/// `tree.c` `merge()` (1790-2146), transcribed over the naive filesystem: the transaction's tree as the target, the
/// head's as the source and the base's as the ancestor, from the root as `merge_changes` calls it (2186).
fn merge(fs: &NaiveFs, trees: Trees<'_>, path: &str) -> Merged {
  let (target, source, ancestor) = (trees.target[path], trees.source[path], trees.ancestor[path]);
  // "Either no change made in source, or same change as made in target." (1837-1843)
  if ancestor == source || source == target {
    return Ok(());
  }
  // 1908-1913: never met, since an entry's kinds are judged before `merge()` goes down into it, and every root is a
  // directory; the verdict refuses an index that breaks this as corrupt.
  assert!(
    [target, source, ancestor]
      .iter()
      .all(|node| fs.nodes[*node].kind == NodeKind::Directory),
    "merge() met a node that is not a directory at {path}"
  );
  merge_properties(fs, trees, path)?;
  merge_ancestor_entries(fs, trees, path)?;
  merge_source_additions(trees, path)
}

/// `merge()`'s property checks (1916-1964): the source's list against the ancestor's, then the target's against a
/// change of the directory's structure.
fn merge_properties(fs: &NaiveFs, trees: Trees<'_>, path: &str) -> Merged {
  let properties = |tree: &Tree| fs.nodes[tree[path]].properties;
  if properties(trees.source) != properties(trees.ancestor) {
    return Err((path.to_owned(), ConflictKind::PropertiesChanged));
  }
  if properties(trees.target) != properties(trees.ancestor)
    && compare_dir_structure(fs, (trees.source, trees.ancestor), path)
  {
    return Err((path.to_owned(), ConflictKind::StructureChanged));
  }
  Ok(())
}

/// `merge()`'s loop over the ancestor's entries (1978-2094).
fn merge_ancestor_entries(fs: &NaiveFs, trees: Trees<'_>, path: &str) -> Merged {
  let s_entries = entries(trees.source, path);
  let t_entries = entries(trees.target, path);
  for (name, a_entry) in entries(trees.ancestor, path) {
    let (s_entry, t_entry) = (s_entries.get(name).copied(), t_entries.get(name).copied());
    // No change made to the entry while the transaction was in progress (1989), or a change the transaction did not
    // touch, which the target takes from the source (1994).
    if s_entry == Some(a_entry) || t_entry == Some(a_entry) {
      continue;
    }
    merge_changed_entry(
      fs,
      trees,
      &joined_path(path, name),
      (s_entry, t_entry, a_entry),
    )?;
  }
  Ok(())
}

/// `merge()` at an entry both sides changed (2041-2092): a deletion on either side, a file among the three, a side
/// that is no direct modification, else the merge goes down into it.
fn merge_changed_entry(
  fs: &NaiveFs,
  trees: Trees<'_>,
  entry_path: &str,
  (s_entry, t_entry, a_entry): (Option<usize>, Option<usize>, usize),
) -> Merged {
  let (Some(s_entry), Some(t_entry)) = (s_entry, t_entry) else {
    let kind = if s_entry.is_none() && t_entry.is_none() {
      ConflictKind::DeletedTwice
    } else {
      ConflictKind::DeletedAgainstChange
    };
    return Err((entry_path.to_owned(), kind));
  };
  if let Some(kind) = file_among(fs, [s_entry, t_entry, a_entry]) {
    return Err((entry_path.to_owned(), kind));
  }
  if !same_line(fs, s_entry, a_entry) || !same_line(fs, t_entry, a_entry) {
    return Err((entry_path.to_owned(), ConflictKind::NotADirectModification));
  }
  merge(fs, trees, entry_path)
}

/// `merge()`'s file check (2052), the conflict named by whether all three sides are files.
fn file_among(fs: &NaiveFs, nodes: [usize; 3]) -> Option<ConflictKind> {
  let kinds = nodes.map(|node| fs.nodes[node].kind);
  if !kinds.contains(&NodeKind::File) {
    None
  } else if kinds.iter().all(|kind| *kind == NodeKind::File) {
    Some(ConflictKind::FileChanged)
  } else {
    Some(ConflictKind::KindChanged)
  }
}

/// `merge()`'s loop over the source's entries the ancestor does not hold (2096-2133).
fn merge_source_additions(trees: Trees<'_>, path: &str) -> Merged {
  let a_entries = entries(trees.ancestor, path);
  let t_entries = entries(trees.target, path);
  for name in entries(trees.source, path).keys() {
    if a_entries.contains_key(name) {
      continue;
    }
    if t_entries.contains_key(name) {
      return Err((joined_path(path, name), ConflictKind::AddedTwice));
    }
  }
  Ok(())
}

/// `lock.c` `verify_lock` (606-629), transcribed: whether a commit by `commit`'s principal, presenting its tokens, may
/// pass `lock`.
fn verify_lock(lock: &Lock, commit: &CommitLocks) -> Result<(), LockRefusal> {
  let Some(user) = commit.user.as_deref() else {
    return Err(LockRefusal::NoUser {
      path: Some(lock.path.clone()),
    });
  };
  if user != lock.owner {
    return Err(LockRefusal::OwnerMismatch {
      path: lock.path.clone(),
      owner: lock.owner.clone(),
    });
  }
  if !commit
    .presented
    .values()
    .any(|token| token == lock.token.as_str())
  {
    return Err(LockRefusal::TokenNotPresented {
      path: lock.path.clone(),
    });
  }
  Ok(())
}

/// `lock.c` `svn_fs_fs__allow_locked_operation` (645-669), transcribed: the lock at `path`, or with `recurse` every
/// lock at or beneath it, each verified. FSFS walks a subtree's lock digests in its hash's order; here in path order.
fn allow_locked_operation(
  table: &LockTable,
  path: &str,
  recurse: bool,
  commit: &CommitLocks,
) -> Result<(), LockRefusal> {
  for lock in table.locks() {
    let reached = if recurse {
      svn_fspath_skip_ancestor(path, &lock.path)
    } else {
      lock.path == path
    };
    if reached {
      verify_lock(lock, commit)?;
    }
  }
  Ok(())
}

/// `transaction.c` `verify_locks` (3527-3593), transcribed over the work's rows: sorted "depth-first-ily", a path
/// beneath the last one checked recursively skipped (counted), each checked recursively unless it is a modification.
fn verify_locks(
  changes: &[ChangedPath],
  (table, commit): (&LockTable, &CommitLocks),
  census: &mut Census,
) -> Answer {
  let mut sorted: Vec<&ChangedPath> = changes.iter().collect();
  sorted.sort_by(|first, second| svn_path_compare_paths(&first.path, &second.path));
  let mut last_recursed: Option<&str> = None;
  for change in sorted {
    if let Some(last) = last_recursed
      && svn_fspath_skip_ancestor(last, &change.path)
    {
      census.skipped += 1;
      continue;
    }
    let recurse = change.action != Action::Modify;
    if let Err(refusal) = allow_locked_operation(table, &change.path, recurse, commit) {
      return Answer::Locked(refusal);
    }
    if recurse {
      last_recursed = Some(&change.path);
    }
  }
  Answer::Commits
}

/// The path of the lock a refusal names, when it names one.
fn refused_at(refusal: &LockRefusal) -> Option<&str> {
  match refusal {
    LockRefusal::NoUser { path } => path.as_deref(),
    LockRefusal::OwnerMismatch { path, .. } | LockRefusal::TokenNotPresented { path } => {
      Some(path.as_str())
    }
    _ => None,
  }
}

/// How often each answer came out, the oracle's evidence that every rule it compares was met.
#[derive(Debug, Default)]
struct Census {
  /// Works that committed.
  commits: usize,
  /// Works built on the head itself.
  on_the_head: usize,
  /// Conflicts, by the check that found them.
  conflicts: BTreeMap<ConflictKind, usize>,
  /// Lock refusals, by the Subversion error they answer.
  locked: BTreeMap<u32, usize>,
  /// Lock refusals at a lock beneath every path the work changed: met by a read of a subtree.
  locked_beneath: usize,
  /// Changed paths the oracle skipped beneath one checked recursively.
  skipped: usize,
}

impl Census {
  /// Counts one case's answer for a work of `changes`.
  fn count(&mut self, answer: &Answer, on_the_head: bool, changes: &[ChangedPath]) {
    self.on_the_head += usize::from(on_the_head);
    match answer {
      Answer::Commits => self.commits += 1,
      Answer::Conflict(_, kind) => *self.conflicts.entry(*kind).or_default() += 1,
      Answer::Locked(refusal) => {
        *self.locked.entry(refusal.subversion_code()).or_default() += 1;
        let at = refused_at(refusal);
        if at.is_some_and(|at| changes.iter().all(|row| row.path != at)) {
          self.locked_beneath += 1;
        }
      }
    }
  }

  /// Fails unless every rule the oracle compares was met.
  fn assert_met(&self) {
    let kinds = [
      ConflictKind::PropertiesChanged,
      ConflictKind::StructureChanged,
      ConflictKind::DeletedTwice,
      ConflictKind::DeletedAgainstChange,
      ConflictKind::KindChanged,
      ConflictKind::FileChanged,
      ConflictKind::NotADirectModification,
      ConflictKind::AddedTwice,
    ];
    for kind in kinds {
      assert!(
        self.conflicts.contains_key(&kind),
        "never met {kind:?}: {self:?}"
      );
    }
    // `E160034` no principal, `E160037` a token not presented, `E160039` another principal's lock.
    for code in [160_034, 160_037, 160_039] {
      assert!(
        self.locked.contains_key(&code),
        "never met E{code}: {self:?}"
      );
    }
    let met = [
      self.commits,
      self.on_the_head,
      self.locked_beneath,
      self.skipped,
    ];
    assert!(
      met.iter().all(|&count| count > 0),
      "a rule never met: {self:?}"
    );
  }
}

/// The cases the oracle runs unless `PROPTEST_CASES` asks for another count (`slates_test_seeds::cases`).
/// Derived: the census asserts that every rule it compares was met, and proptest draws a new seed every run, so the
/// count must make the rarest rule's absence a broken rule rather than chance. Measured on the Mac Studio (kiln,
/// 2026-11-19 04:18 UTC, this count): the rarest, a file both sides changed, 49 times in 4,096 cases (a double delete
/// 74, a lock met beneath every changed path 77), so a run expects it about 49 times and misses it with probability
/// about e^-49; the whole crate's suite ran in 1.03 s.

/// How many of the last revisions a near base is drawn from. A work on an old base meets every revision since, and a
/// change of the root's properties among them refuses it at the root before any other check (the first census, at
/// most bases anywhere: 112 of 256 cases), so most bases are drawn near the head and after the root's last property
/// change, where the checks beneath the root are reached.
const NEAR_HEAD: u64 = 4;

/// The bits of a case's base that, both set, draw it from the whole history instead of near the head: a quarter of
/// the cases.
const ANYWHERE: u16 = 0xC000;

/// The bit of a lock's mark that places it on any of the head's files instead of one the work's changes reach.
const ANY_FILE: u8 = 0x80;

/// The bit of a lock's mark that places it strictly beneath a path the work adds, deletes or replaces, where only a
/// check of that path's whole subtree meets it.
const BENEATH: u8 = 0x40;

/// The bit of a lock's mark that gives it to another principal than the committer.
const OTHERS: u8 = 0x01;

/// The bit of a lock's mark that has the commit present its token.
const PRESENTED: u8 = 0x02;

/// One case in this many commits with no principal.
const ANONYMOUS_ONE_IN: u8 = 4;

/// One generated case: a history, a base drawn from it, the wishes of a work on that base, its principal, and the
/// locks to take.
#[derive(Clone, Debug)]
struct Case {
  /// Each revision's wishes.
  history: Vec<Vec<Wish>>,
  /// Which revision the work is built on: one of the last `NEAR_HEAD` after the root's last property change, or any
  /// with the `ANYWHERE` bits.
  base: u16,
  /// Which wishes of the revision after the base the work repeats, a bit for each: the same changes made on both
  /// sides, so double deletions, double additions and files both sides changed are met.
  repeat: u8,
  /// The work's own wishes, after those it repeats.
  work: Vec<Wish>,
  /// Whether the commit has a principal: none for one case in `ANONYMOUS_ONE_IN`.
  user: u8,
  /// Each lock: which file (one the work's changes reach; one beneath a path it adds, deletes or replaces with the
  /// `BENEATH` bit; any of the head's with the `ANY_FILE` bit), whose (`OTHERS`), and whether its token is presented
  /// (`PRESENTED`).
  locks: Vec<(u16, u8)>,
}

/// Any case: histories as the index's oracle draws them, a work of up to six wishes of its own, and up to six locks.
fn cases() -> impl Strategy<Value = Case> {
  (
    histories(),
    any::<u16>(),
    any::<u8>(),
    prop::collection::vec(wish(), 0..7),
    any::<u8>(),
    prop::collection::vec((any::<u16>(), any::<u8>()), 0..7),
  )
    .prop_map(|((history, _), base, repeat, work, user, locks)| Case {
      history,
      base,
      repeat,
      work,
      user,
      locks,
    })
}

/// The case's base revision in `index`'s history.
fn base_of(case: &Case, index: &Index) -> Revision {
  let head = index.youngest();
  let drawn = u64::from(case.base & !ANYWHERE);
  if case.base & ANYWHERE == ANYWHERE {
    return drawn % (head + 1);
  }
  let root_changed = (1..=head)
    .rev()
    .find(|revision| {
      index
        .changes(*revision)
        .unwrap()
        .iter()
        .any(|row| row.path == "/" && row.properties.is_some())
    })
    .unwrap_or(0);
  head.saturating_sub(drawn % NEAR_HEAD).max(root_changed)
}

/// The work's wishes: those of the revision after `base` that the case repeats, then its own.
fn work_wishes(case: &Case, base: Revision) -> Vec<Wish> {
  let next = case.history.get(usize::try_from(base).unwrap());
  let repeated = next.into_iter().flatten().enumerate().filter(|(place, _)| {
    u32::try_from(*place).is_ok_and(|place| {
      case
        .repeat
        .checked_shr(place)
        .is_some_and(|bits| bits & 1 == 1)
    })
  });
  repeated
    .map(|(_, wish)| *wish)
    .chain(case.work.iter().copied())
    .collect()
}

/// The naive filesystem and the index after the same generated history.
fn built(history: &[Vec<Wish>]) -> Result<(NaiveFs, Index), String> {
  let mut fs = NaiveFs::new();
  let mut index = Index::new();
  for wishes in history {
    let mut txn = fs.begin();
    let rows = txn.rows_from(wishes);
    txn.commit();
    index
      .append(work(rows), ALL_ROOM)
      .map_err(|refusal| format!("the history was refused: {refusal:?}"))?;
  }
  Ok((fs, index))
}

/// Whether a lock on `path` is placed by `mark`: on any file, strictly beneath a path the work adds, deletes or
/// replaces, or at or beneath any path it changes.
fn placed(changes: &[ChangedPath], path: &str, mark: u8) -> bool {
  if mark & ANY_FILE != 0 {
    return true;
  }
  let beneath = mark & BENEATH != 0;
  changes.iter().any(|row| {
    let whole = row.action != Action::Modify && row.path != path;
    (whole || !beneath) && svn_fspath_skip_ancestor(&row.path, path)
  })
}

/// The case's lock table and commit: each lock on one of the head's files (only a file is locked, §4.17.4 item 7) that
/// its mark places, held by the committer or another principal, its token presented or not, the last lock on a path
/// standing; the commit's principal, unless the case has none.
fn locks_of(
  fs: &NaiveFs,
  head: &Tree,
  changes: &[ChangedPath],
  case: &Case,
) -> (LockTable, CommitLocks) {
  let mut chosen: BTreeMap<String, (Lock, bool)> = BTreeMap::new();
  for (number, (place, mark)) in case.locks.iter().enumerate() {
    let candidates: Vec<&String> = head
      .iter()
      .filter(|(path, node)| {
        fs.nodes[**node].kind == NodeKind::File && placed(changes, path, *mark)
      })
      .map(|(path, _)| path)
      .collect();
    let Some(path) = candidates.get(usize::from(*place) % candidates.len().max(1)) else {
      continue;
    };
    let owner = if mark & OTHERS != 0 { OTHER } else { COMMITTER };
    let lock = lock_on(path, owner, u8::try_from(number).unwrap());
    chosen.insert((*path).clone(), (lock, mark & PRESENTED != 0));
  }
  let mut table = LockTable::new();
  let mut presented = BTreeMap::new();
  for (lock, shown) in chosen.into_values() {
    if shown {
      presented.insert(lock.path.clone(), lock.token.as_str().to_owned());
    }
    table.apply(&LockEntry::Set(lock)).unwrap();
  }
  let user = (case.user % ANONYMOUS_ONE_IN != 0).then(|| COMMITTER.to_owned());
  let commit = CommitLocks {
    user,
    presented,
    keep: false,
  };
  (table, commit)
}

/// FSFS's answer: `merge()` from the root, then `verify_locks` once it accepts.
fn oracle_answer(
  fs: &NaiveFs,
  trees: Trees<'_>,
  changes: &[ChangedPath],
  locks: (&LockTable, &CommitLocks),
  census: &mut Census,
) -> Answer {
  match merge(fs, trees, "/") {
    Err((path, kind)) => Answer::Conflict(path, kind),
    Ok(()) => verify_locks(changes, locks, census),
  }
}

/// The verdict's answer, its lock check the lock table's, as the merge task passes it.
fn rule_answer(
  index: &Index,
  base: Revision,
  changes: Vec<ChangedPath>,
  locks: (&LockTable, &CommitLocks),
) -> Result<Answer, String> {
  match judged_with_locks(index, base, changes, locks) {
    Ok(_) => Ok(Answer::Commits),
    Err(CommitRefusal::Conflict { path, kind }) => Ok(Answer::Conflict(path, kind)),
    Err(CommitRefusal::Locked(refusal)) => Ok(Answer::Locked(refusal)),
    Err(refusal) => Err(format!("the work was refused: {refusal:?}")),
  }
}

/// One case answered by the verdict and by the oracle.
fn check_case(case: &Case, census: &mut Census) -> Result<(), String> {
  let (mut fs, index) = built(&case.history)?;
  let head = index.youngest();
  let base = base_of(case, &index);
  let mut txn = fs.begin_on(base);
  let changes = txn.rows_from(&work_wishes(case, base));
  let target = txn.into_tree();
  let tree = |revision: Revision| &fs.trees[usize::try_from(revision).unwrap()];
  let (table, commit) = locks_of(&fs, tree(head), &changes, case);
  let trees = Trees {
    target: &target,
    source: tree(head),
    ancestor: tree(base),
  };
  let oracle = oracle_answer(&fs, trees, &changes, (&table, &commit), census);
  let rule = rule_answer(&index, base, changes.clone(), (&table, &commit))?;
  if rule != oracle {
    return Err(format!(
      "base {base}, head {head}, work {changes:?}, locks {table:?}, commit {commit:?}: the verdict answered {rule:?}, FSFS {oracle:?}"
    ));
  }
  census.count(&oracle, base == head, &changes);
  Ok(())
}

/// A-592 (§4.17.4 items 3.3 and 3.4, piece 5's oracle). Do: over generated histories of up to nine revisions
/// (additions, copies of any earlier revision's paths, their own subtrees among them, deletions, replacements, text
/// and property changes, the root's among them), build a work on a base drawn from them, mostly near the head, as the
/// naive FSFS of the index's tests builds a transaction: some of the next revision's own changes made again, then up
/// to six of its own; lock up to six of the head's files, mostly ones the work's changes reach, each the committer's or
/// another principal's, its token presented or not, the commit anonymous in a quarter of the cases; answer the work by
/// the verdict, its lock check the lock table's (A-602), and by FSFS's `merge()` and `verify_locks` transcribed over
/// the naive filesystem's whole trees. Expect: the same answer on every case; and the census, printed every run,
/// showing commits, works on the head, every conflict kind, each lock refusal, a lock met beneath every changed path,
/// and a changed path skipped beneath one checked recursively.
#[test]
#[allow(
  clippy::print_stderr,
  reason = "the census is the oracle's evidence, printed with every run"
)]
fn the_verdict_is_fsfs_commit_on_every_generated_history() {
  let census = RefCell::new(Census::default());
  let config = Config {
    cases: slates_test_seeds::cases(CASES),
    ..Config::default()
  };
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(config));
  let result = runner.run(&cases(), |case| {
    check_case(&case, &mut census.borrow_mut()).map_err(TestCaseError::fail)
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  eprintln!("commit rule census: {census:?}");
  census.assert_met();
}
