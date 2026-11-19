//! What an import records where a dump's records meet FSFS's rules, and how an export compares a node: each a small
//! history written record by record.

use super::support::{
  Dump, Node, VECTOR_UUID, created, export, history, import_whole, properties, revision_properties,
};
use crate::dump::Format;
use crate::index::{Action, ChangedPath, NodeKind};
use crate::migration::{Properties, Repository};

/// The bytes of a file the first revision adds.
const TEXT: &[u8] = b"a text\n";

/// A dump's revision 0 and a first revision adding `d/`, `d/x`, `f`, `trunk/` with `a`, `b` and `c`, and `z`.
fn first_revision() -> Dump {
  let mut dump = Dump::new(Format::Two, Some(VECTOR_UUID));
  dump
    .revision(0, &properties(&[("svn:date", "2005-04-07T22:13:00.000000Z")]))
    .revision(1, &revision_properties(1));
  for directory in ["d", "trunk"] {
    dump.node(&Node::add(directory, NodeKind::Directory).properties(Properties::new()));
  }
  for file in ["d/x", "f", "trunk/a", "trunk/b", "trunk/c", "z"] {
    dump.node(&Node::add(file, NodeKind::File).properties(Properties::new()).text(TEXT));
  }
  dump
}

/// A created repository with `dump` imported.
fn imported(dump: &Dump) -> Repository {
  let mut repository = created();
  import_whole(&mut repository, &dump.bytes()).unwrap();
  repository
}

/// The changed paths of `repository`'s youngest revision.
fn youngest_changes(repository: &Repository) -> Vec<ChangedPath> {
  repository.index().changes(repository.youngest()).unwrap().to_vec()
}

/// The actions of `changes`, by path.
fn actions(changes: &[ChangedPath]) -> Vec<(&str, Action)> {
  changes.iter().map(|change| (change.path.as_str(), change.action)).collect()
}

/// A-628 (rules): do import a dump whose UUID record and revision 0 come first into a repository `CreateRepository`
/// made, then a dump of the next revision alone into another. Expect the first to carry the stream's UUID and exactly
/// the stream's revision-0 properties, and the second to keep its own UUID and creation date.
#[test]
fn an_empty_repository_takes_the_streams_uuid_and_revision_zero() {
  let repository = imported(&first_revision());
  assert_eq!(repository.uuid().as_bytes(), VECTOR_UUID);
  assert_eq!(
    repository.revision_properties(0),
    Some(&properties(&[("svn:date", "2005-04-07T22:13:00.000000Z")]))
  );
  let mut alone = Dump::new(Format::Two, None);
  alone
    .revision(1, &revision_properties(1))
    .node(&Node::add("f", NodeKind::File).properties(Properties::new()).text(TEXT));
  let continued = imported(&alone);
  assert_eq!(continued.uuid(), created().uuid());
  assert_eq!(continued.revision_properties(0), created().revision_properties(0));
}

/// A-628 (rules): do import a revision whose records change `f` with no section and give `d`, which has no properties,
/// an empty whole property list. Expect the revision to record no changed path, as FSFS records none when no call is
/// made (`load-fs-vtable.c` makes none for a change without sections, `remove_node_props` none for a node without
/// properties), and its export to be its revision record alone.
#[test]
fn a_change_that_makes_no_call_records_nothing() {
  let mut dump = first_revision();
  dump
    .revision(2, &revision_properties(2))
    .node(&Node::change("f"))
    .node(&Node::change("d").properties(Properties::new()));
  let repository = imported(&dump);
  assert_eq!(youngest_changes(&repository), Vec::new());
  let record = export(&repository, 2, 2).unwrap();
  assert!(!record.windows(b"Node-path".len()).any(|window| window == b"Node-path"));
}

/// A-628 (rules): do import a revision setting `f`'s text to the bytes it holds. Expect a modification with its text
/// set, as FSFS records a text change for every text applied, and an export of a change record with no section, as
/// `svnadmin dump` compares representation keys, which FSFS shares among equal texts.
#[test]
fn a_text_set_to_the_bytes_it_holds_is_a_change_written_without_its_text() {
  let mut dump = first_revision();
  dump
    .revision(2, &revision_properties(2))
    .node(&Node::change("f").text(TEXT));
  let repository = imported(&dump);
  let changes = youngest_changes(&repository);
  assert_eq!(actions(&changes), vec![("/f", Action::Modify)]);
  assert!(changes[0].text.is_some());
  let record = export(&repository, 2, 2).unwrap();
  let bare = b"Node-path: f\nNode-kind: file\nNode-action: change\n\n\n";
  assert!(record.ends_with(bare), "{}", String::from_utf8_lossy(&record));
}

/// A-628 (rules): do import a revision that deletes `f`, adds `g`, adds `f` again, then adds and deletes `h`. Expect
/// `f` one replacement, `g` an addition and nothing at `h` (`transaction.c` `fold_change`), and the export to import
/// to the same history.
#[test]
fn records_of_one_path_fold_as_fsfs_folds_them() {
  let mut dump = first_revision();
  dump
    .revision(2, &revision_properties(2))
    .node(&Node::delete("f"))
    .node(&Node::add("g", NodeKind::File).properties(Properties::new()).text(b"g"))
    .node(&Node::add("f", NodeKind::Directory).properties(Properties::new()))
    .node(&Node::add("h", NodeKind::File).properties(Properties::new()).text(b"h"))
    .node(&Node::delete("h"));
  let repository = imported(&dump);
  let changes = youngest_changes(&repository);
  assert_eq!(
    actions(&changes),
    vec![("/f", Action::Replace), ("/g", Action::Add)]
  );
  assert_eq!(changes[0].kind, NodeKind::Directory);
  same_history_through_its_export(&repository);
}

/// A-628 (rules): do import a revision that changes `d/x`, then deletes `d`. Expect `d`'s deletion alone, as FSFS
/// drops a deleted path's children from its changes.
#[test]
fn what_a_revision_changed_beneath_a_path_it_deleted_is_dropped() {
  let mut dump = first_revision();
  dump
    .revision(2, &revision_properties(2))
    .node(&Node::change("d/x").text(b"changed"))
    .node(&Node::delete("d"));
  let repository = imported(&dump);
  assert_eq!(actions(&youngest_changes(&repository)), vec![("/d", Action::Delete)]);
  same_history_through_its_export(&repository);
}

/// A-628 (rules): do set `svn:mergeinfo` on `d`, then change another of its properties, then change `svn:mergeinfo`,
/// then delete it, a revision each. Expect the mergeinfo flag set exactly where `svn:mergeinfo`'s value changed, so a
/// history exported as whole property lists imports with the flags it had.
#[test]
fn the_mergeinfo_flag_says_whether_svn_mergeinfo_changed() {
  let lists = [
    properties(&[("svn:mergeinfo", "/trunk:1")]),
    properties(&[("other", "x"), ("svn:mergeinfo", "/trunk:1")]),
    properties(&[("other", "x"), ("svn:mergeinfo", "/trunk:1-2")]),
    properties(&[("other", "x")]),
  ];
  let mut dump = first_revision();
  for (revision, list) in (2..).zip(&lists) {
    dump
      .revision(revision, &revision_properties(revision))
      .node(&Node::change("d").properties(list.clone()));
  }
  let repository = imported(&dump);
  let flags: Vec<bool> = (2..=repository.youngest())
    .map(|revision| repository.index().changes(revision).unwrap()[0].mergeinfo_modified)
    .collect();
  assert_eq!(flags, vec![true, false, true, true]);
  same_history_through_its_export(&repository);
}

/// A-628 (rules): do copy `trunk` to `branch` and, in the same revision, change `branch/a`'s text, delete `branch/b`
/// and set a property on `branch/c`. Expect the export to write `branch/a` and `branch/c` as changes against the copy
/// source (`open_file` taking the copied directory's comparison) and `branch/b`'s deletion when `branch` closes, and
/// to import to the same history.
#[test]
fn changes_beneath_a_copy_are_written_against_the_copy_source() {
  let mut dump = first_revision();
  dump
    .revision(2, &revision_properties(2))
    .node(&Node::add("branch", NodeKind::Directory).copied(1, "trunk"))
    .node(&Node::change("branch/a").text(b"changed on the branch\n"))
    .node(&Node::delete("branch/b"))
    .node(&Node::change("branch/c").properties(properties(&[("p", "v")])));
  let repository = imported(&dump);
  let record = String::from_utf8(export(&repository, 2, 2).unwrap()).unwrap();
  let order: Vec<usize> = [
    "Node-path: branch\n",
    "Node-path: branch/a\nNode-kind: file\nNode-action: change\nText-content-md5",
    "Node-path: branch/c\nNode-kind: file\nNode-action: change\nProp-content-length",
    "Node-path: branch/b\nNode-action: delete\n",
  ]
  .iter()
  .map(|part| record.find(part).unwrap_or_else(|| panic!("{part:?} in {record}")))
  .collect();
  assert!(order.is_sorted(), "{record}");
  same_history_through_its_export(&repository);
}

/// A-628 (rules): do delete `trunk/c` and `trunk/a`, change `trunk/b` and delete `z` in one revision. Expect the export
/// to write `trunk/b`'s change first, then `trunk`'s deletions in byte order when `trunk` closes, then `z`'s when the
/// root closes (`close_directory`).
#[test]
fn deletions_are_written_when_their_directory_closes_in_byte_order() {
  let mut dump = first_revision();
  dump
    .revision(2, &revision_properties(2))
    .node(&Node::delete("z"))
    .node(&Node::delete("trunk/c"))
    .node(&Node::change("trunk/b").text(b"b"))
    .node(&Node::delete("trunk/a"));
  let repository = imported(&dump);
  let record = String::from_utf8(export(&repository, 2, 2).unwrap()).unwrap();
  let order: Vec<usize> = [
    "Node-path: trunk/b\n",
    "Node-path: trunk/a\nNode-action: delete\n",
    "Node-path: trunk/c\nNode-action: delete\n",
    "Node-path: z\nNode-action: delete\n",
  ]
  .iter()
  .map(|part| record.find(part).unwrap_or_else(|| panic!("{part:?} in {record}")))
  .collect();
  assert!(order.is_sorted(), "{record}");
}

/// Holds `repository` to the history its own export imports to.
fn same_history_through_its_export(repository: &Repository) {
  let mut again = created();
  import_whole(&mut again, &export(repository, 0, repository.youngest()).unwrap()).unwrap();
  assert_eq!(history(&again), history(repository));
}
