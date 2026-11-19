//! The trees and commits the server makes (A-664): held to stock git's own bytes (`golden`), to the trees built from
//! each version's whole path map from scratch over generated histories and to themselves derived twice (`oracle`), and,
//! here, by use: what a derivation costs, a directory past one node, a path resolved, an identity git would change,
//! a pushed version's commit as the next made one's parent, and every refusal by kind.

mod golden;
mod oracle;

use std::collections::BTreeMap;

use slates_vcs::derived::paths::{Change, Entry, PathChanges};
use slates_vcs::{ContentId, ContentStore, Graph, Mode as Computed};

use super::{
  Authored, CommitSource, Commits, CommitsRefused, Fields, Identity, IdentityPart, IdentityRefused,
  Item, Trees, TreesRefused,
};
use crate::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use crate::objects::fsck::FsckId;
use crate::objects::tree::{Entry as TreeEntry, Mode, compare, write_entries};

/// Shape: the store's window in these tests: a directory's node names 29 subtrees beside its object, so a directory
/// of 40 or 100 is named through lists, and a tree of 40 entries is past one window.
const WINDOW: usize = 1 << 10;

/// Shape: the tests' content budget, room to spare.
const BUDGET: u64 = 1 << 28;

/// Both of git's object formats.
const FORMATS: [ObjectFormat; 2] = [ObjectFormat::Sha1, ObjectFormat::Sha256];

/// A version's files as the tests keep them: each path's class and bytes.
type Files = BTreeMap<Vec<u8>, (u8, Vec<u8>)>;

/// A store of the tests' window and budget.
fn store() -> ContentStore {
  ContentStore::new(BUDGET, WINDOW).unwrap()
}

/// How a version's commit is had, as the tests record it.
#[derive(Clone, Debug)]
enum Commit {
  /// A stock client pushed it.
  Pushed(ObjectId),
  /// The server makes it.
  Server {
    name: String,
    email: String,
    seconds: u64,
    message: String,
  },
}

/// A recorded version: its parents, its change, how its commit is had.
#[derive(Clone, Debug)]
struct Recorded {
  parents: Vec<u64>,
  change: Change,
  commit: Commit,
}

/// A history of versions, and each version's trees value once derived, which its made commit names.
#[derive(Debug, Default)]
struct History {
  versions: BTreeMap<u64, Recorded>,
  trees: BTreeMap<u64, ContentId>,
}

impl Graph for History {
  type Version = u64;

  fn parents(&self, version: u64) -> Option<Vec<u64>> {
    self
      .versions
      .get(&version)
      .map(|recorded| recorded.parents.clone())
  }

  fn name(&self, version: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&version.to_be_bytes());
  }
}

impl PathChanges for History {
  fn change(&self, version: u64) -> Option<&Change> {
    self.versions.get(&version).map(|recorded| &recorded.change)
  }
}

impl CommitSource for History {
  fn commit(&self, version: u64) -> Option<Authored<'_>> {
    let recorded = self.versions.get(&version)?;
    match &recorded.commit {
      Commit::Pushed(id) => Some(Authored::Pushed(*id)),
      Commit::Server {
        name,
        email,
        seconds,
        message,
      } => Some(Authored::Server(Fields {
        tree: *self.trees.get(&version)?,
        identity: Identity::new(name, email).ok()?,
        seconds: *seconds,
        message,
      })),
    }
  }
}

impl History {
  fn record(&mut self, version: u64, parents: &[u64], change: Change, commit: Commit) {
    let parents = parents.to_vec();
    self.versions.insert(
      version,
      Recorded {
        parents,
        change,
        commit,
      },
    );
  }
}

/// The content of `bytes` in `store`, held once more by the caller.
fn content(store: &mut ContentStore, bytes: &[u8]) -> ContentId {
  store.put_object(bytes).unwrap()
}

/// The path map's entry of a file of `class` and `bytes`, its content put in `store`.
fn entry(store: &mut ContentStore, (class, bytes): &(u8, Vec<u8>)) -> Entry {
  Entry {
    class: *class,
    content: content(store, bytes),
  }
}

/// `files` as the path map's entries, their contents put in `store`.
fn entries(store: &mut ContentStore, files: &Files) -> BTreeMap<Vec<u8>, Entry> {
  files
    .iter()
    .map(|(path, file)| (path.clone(), entry(store, file)))
    .collect()
}

/// The mode a class names, written here as the model's own table.
fn naive_mode(class: u8) -> Mode {
  match class {
    0 => Mode::File,
    1 => Mode::Executable,
    2 => Mode::Symlink,
    _ => Mode::Gitlink,
  }
}

/// The id of the tree of `files` in `format`, recomputed from scratch as git writes trees: every directory's entries
/// written in git's order and hashed, every blob hashed, a submodule link's bytes its id.
fn naive_tree(format: ObjectFormat, files: &Files) -> ObjectId {
  let mut below: BTreeMap<Vec<u8>, Files> = BTreeMap::new();
  let mut here: Vec<(Mode, Vec<u8>, ObjectId)> = Vec::new();
  for (path, (class, bytes)) in files {
    match path.iter().position(|byte| *byte == b'/') {
      Some(at) => {
        below
          .entry(path[..at].to_vec())
          .or_default()
          .insert(path[at + 1..].to_vec(), (*class, bytes.clone()));
      }
      None => {
        let id = match class {
          3 => ObjectId::from_bytes(format, bytes).unwrap(),
          _ => name_of(format, Kind::Blob, bytes).unwrap(),
        };
        here.push((naive_mode(*class), path.clone(), id));
      }
    }
  }
  for (name, files) in &below {
    here.push((Mode::Directory, name.clone(), naive_tree(format, files)));
  }
  let mut list: Vec<TreeEntry<'_>> = here
    .iter()
    .map(|(mode, name, id)| TreeEntry::new(*mode, name, id.as_bytes()))
    .collect();
  list.sort_by(compare);
  let mut bytes = Vec::new();
  write_entries(&list, &mut bytes);
  name_of(format, Kind::Tree, &bytes).unwrap()
}

/// A commit's bytes and id, written here as `git commit-tree -m` writes one.
fn naive_commit(
  format: ObjectFormat,
  (tree, parents): (ObjectId, &[ObjectId]),
  (name, email, seconds): (&str, &str, u64),
  message: &str,
) -> (ObjectId, Vec<u8>) {
  let mut text = format!("tree {tree}\n");
  for parent in parents {
    text.push_str(&format!("parent {parent}\n"));
  }
  for role in ["author", "committer"] {
    text.push_str(&format!("{role} {name} <{email}> {seconds} +0000\n"));
  }
  text.push_str(&format!("\n{message}"));
  if !message.is_empty() && !message.ends_with('\n') {
    text.push('\n');
  }
  (
    name_of(format, Kind::Commit, text.as_bytes()).unwrap(),
    text.into_bytes(),
  )
}

/// The trees of `format` over the tests' store, eager.
fn trees(format: ObjectFormat) -> Trees<History> {
  Trees::declare(format, WINDOW, Computed::Eager).unwrap()
}

/// The value `root` is the value built from scratch of `files`, identity for identity, and its tree's id is the naive
/// recomputation's.
fn same_as_built(
  store: &mut ContentStore,
  trees: &mut Trees<History>,
  root: ContentId,
  files: &Files,
) {
  let entries = entries(store, files);
  let built = trees.build(store, &entries).unwrap();
  assert_eq!(
    built, root,
    "the incremental value is the one built from scratch"
  );
  assert_eq!(
    trees.id(store, root).unwrap(),
    naive_tree(trees.format(), files)
  );
  store.release(&built).unwrap();
}

/// What a store holds, to compare before and after a refusal.
fn held(store: &ContentStore) -> (usize, u64, u64) {
  (store.len(), store.used(), store.charged())
}

/// `count` files spread over `src/d<n>/f<m>` and a few at the root and in `docs`.
fn many_files(count: usize) -> Files {
  let mut files = Files::new();
  for at in 0..count {
    let path = format!("src/d{}/f{}", at % 4, at / 4);
    files.insert(path.into_bytes(), (0, format!("file {at}\n").into_bytes()));
  }
  files.insert(b"README".to_vec(), (0, b"readme\n".to_vec()));
  files.insert(b"docs/guide.md".to_vec(), (0, b"# guide\n".to_vec()));
  files
}

/// The counters a call moved: directories made, blobs hashed, directories read.
fn moved(before: super::TreeCounters, after: super::TreeCounters) -> (u64, u64, u64) {
  (
    after.directories - before.directories,
    after.blobs - before.blobs,
    after.read - before.read,
  )
}

/// A-664 (the non-vacuity counters, §4.17.2 item 5's "changing only the directories the change touched"). Do: build a
/// version of 202 files (four directories of 50 under `src`, `README`, `docs/guide.md`), then change one file two
/// directories down. Expect: the derivation read the root, `src` and `src/d2` and made those three again, and hashed
/// the one blob it set; its value is the one built from scratch of the new files, whose tree id is the naive
/// recomputation's; and a change of nothing keeps the parent's value, reading one node and making none.
#[test]
fn t_a664_a_derivation_makes_only_its_changes_directories_and_hashes_only_what_it_sets() {
  for format in FORMATS {
    let mut store = store();
    let mut trees = trees(format);
    let mut files = many_files(200);
    let all = entries(&mut store, &files);
    let parent = trees.build(&mut store, &all).unwrap();
    let file = (0, b"changed\n".to_vec());
    let change = Change::from([(b"src/d2/f7".to_vec(), Some(entry(&mut store, &file)))]);
    files.insert(b"src/d2/f7".to_vec(), file);
    let before = trees.counters();
    let root = trees.apply(&mut store, Some(parent), &change).unwrap();
    assert_eq!(moved(before, trees.counters()), (3, 1, 3));
    same_as_built(&mut store, &mut trees, root, &files);
    let before = trees.counters();
    let same = trees.apply(&mut store, Some(root), &Change::new()).unwrap();
    assert_eq!((same, moved(before, trees.counters())), (root, (0, 0, 0)));
  }
}

/// The subdirectories `wide/s<n>`, each holding one file, for `n` in `range`.
fn wide(range: std::ops::Range<usize>) -> Files {
  range
    .map(|at| {
      let path = format!("wide/s{at:03}/f");
      (path.into_bytes(), (0, format!("{at}\n").into_bytes()))
    })
    .collect()
}

/// The node of the directory `wide` in the value `root`.
fn wide_node(store: &ContentStore, trees: &Trees<History>, root: ContentId) -> ContentId {
  match trees.get(store, root, b"wide").unwrap() {
    Some(Item::Tree { node, .. }) => node,
    other => panic!("wide is a directory, not {other:?}"),
  }
}

/// The directory `node`'s subtrees are its tree's subtree entries' nodes, in its tree's order, and it names `names`
/// contents itself: its object and its subtrees' nodes, or the lists above them.
fn names_its_subtrees(store: &ContentStore, trees: &Trees<History>, node: ContentId, names: usize) {
  let directory = trees.directory(store, node).unwrap();
  let listed: Vec<ContentId> = trees
    .names(store, node)
    .unwrap()
    .values()
    .filter_map(|item| match item {
      Item::Tree { node, .. } => Some(*node),
      Item::File { .. } => None,
    })
    .collect();
  assert_eq!(directory.subtrees, listed);
  assert_eq!(store.derived(&node).unwrap().len(), names);
}

/// A-664 (a directory past one node; A-581's rule for an object past a window). Do: build a version whose `wide`
/// directory holds 100 subdirectories in a store whose directory node names 29 subtrees, change one subdirectory's
/// file, then remove 90 of them. Expect: lists were made and `wide`'s node names its object and 4 lists (100 over 31 a
/// list), its subtrees read back in its tree's order; the change's value is the one built from scratch; and with 10
/// left, `wide`'s node names its object and its 10 subtrees itself again, still the value built from scratch.
#[test]
fn t_a664_a_directory_past_one_node_names_its_subtrees_through_lists() {
  for format in FORMATS {
    let mut store = store();
    let mut trees = trees(format);
    let mut files = wide(0..100);
    let all = entries(&mut store, &files);
    let first = trees.build(&mut store, &all).unwrap();
    assert!(trees.counters().lists >= 4);
    names_its_subtrees(&store, &trees, wide_node(&store, &trees, first), 1 + 4);
    let file = (1, b"changed\n".to_vec());
    files.insert(b"wide/s042/f".to_vec(), file.clone());
    let change = Change::from([(b"wide/s042/f".to_vec(), Some(entry(&mut store, &file)))]);
    let second = trees.apply(&mut store, Some(first), &change).unwrap();
    same_as_built(&mut store, &mut trees, second, &files);
    let removed: Change = wide(10..100).into_keys().map(|path| (path, None)).collect();
    files.retain(|path, _| !removed.contains_key(path));
    let third = trees.apply(&mut store, Some(second), &removed).unwrap();
    same_as_built(&mut store, &mut trees, third, &files);
    names_its_subtrees(&store, &trees, wide_node(&store, &trees, third), 1 + 10);
  }
}

/// A-664 (a version read as a tree, §4.17.2 item 5). Do: build a version of a file, an executable, a symbolic link and a
/// submodule's commit beneath directories, and ask for each path, a directory, a path beneath a file and a missing one.
/// Expect: each file's mode and id as git names them (the link's commit id as its content holds it), each directory's
/// tree id the naive recomputation's, and nothing for the rest.
#[test]
fn t_a664_a_path_resolves_one_tree_a_name() {
  for format in FORMATS {
    let mut store = store();
    let mut trees = trees(format);
    let link = vec![7; format.raw_len()];
    let files = Files::from([
      (b"a/b/file".to_vec(), (0, b"file\n".to_vec())),
      (b"a/run".to_vec(), (1, b"#!/bin/sh\n".to_vec())),
      (b"a/b/link".to_vec(), (2, b"../run".to_vec())),
      (b"module".to_vec(), (3, link.clone())),
    ]);
    let all = entries(&mut store, &files);
    let root = trees.build(&mut store, &all).unwrap();
    let blob = |bytes: &[u8]| name_of(format, Kind::Blob, bytes).unwrap();
    let file = |mode, id| Some(Item::File { mode, id });
    let get = |path: &[u8]| trees.get(&store, root, path).unwrap();
    assert_eq!(get(b"a/b/file"), file(Mode::File, blob(b"file\n")));
    assert_eq!(get(b"a/run"), file(Mode::Executable, blob(b"#!/bin/sh\n")));
    assert_eq!(get(b"a/b/link"), file(Mode::Symlink, blob(b"../run")));
    assert_eq!(
      get(b"module"),
      file(Mode::Gitlink, ObjectId::from_bytes(format, &link).unwrap())
    );
    let under_a: Files = files
      .iter()
      .filter_map(|(path, file)| Some((path.strip_prefix(b"a/")?.to_vec(), file.clone())))
      .collect();
    assert_eq!(
      get(b"a").map(|item| item.id()),
      Some(naive_tree(format, &under_a))
    );
    assert_eq!((get(b"a/run/x"), get(b"a/missing")), (None, None));
  }
}

/// A file at `path` of `class` and `bytes`, as a change sets it.
fn set(store: &mut ContentStore, path: &[u8], class: u8, bytes: &[u8]) -> Change {
  Change::from([(path.to_vec(), Some(entry(store, &(class, bytes.to_vec()))))])
}

/// The refusals of changes over a version holding `a` and `dir/b`, each with the change that meets it.
fn refused_changes(store: &mut ContentStore, format: ObjectFormat) -> Vec<(Change, TreesRefused)> {
  let node = store.put_derived(&[], b"no file").unwrap().identity;
  let path = |path: &[u8]| path.to_vec();
  let long = vec![b'n'; 4097];
  vec![
    (
      Change::from([(path(b"gone"), None)]),
      TreesRefused::RemovedAbsent {
        path: path(b"gone"),
      },
    ),
    (
      set(store, b"dir", 0, b"x"),
      TreesRefused::Shadowed { path: path(b"dir") },
    ),
    (
      set(store, b"a/x", 0, b"x"),
      TreesRefused::Shadowed { path: path(b"a") },
    ),
    (
      set(store, b"n\0ul", 0, b"x"),
      TreesRefused::BadPath {
        path: path(b"n\0ul"),
      },
    ),
    (
      set(store, b"x//y", 0, b"x"),
      TreesRefused::BadPath {
        path: path(b"x//y"),
      },
    ),
    (
      set(store, b"dir/.git", 0, b"x"),
      fsck(b"dir", FsckId::HasDotgit),
    ),
    (set(store, b".GiT", 0, b"x"), fsck(b"", FsckId::HasDotgit)),
    (
      set(store, b".gitmodules", 2, b"x"),
      fsck(b"", FsckId::GitmodulesSymlink),
    ),
    (set(store, &long, 0, b"x"), fsck(b"", FsckId::LargePathname)),
    (
      set(store, b"m", 3, &vec![0; format.raw_len()]),
      fsck(b"", FsckId::NullSha1),
    ),
    (
      set(store, b"m", 3, b"short"),
      TreesRefused::BadGitlink { path: path(b"m") },
    ),
    (
      set(store, b"c", 9, b"x"),
      TreesRefused::UnknownClass {
        path: path(b"c"),
        class: 9,
      },
    ),
    (
      Change::from([(
        path(b"d"),
        Some(Entry {
          class: 0,
          content: node,
        }),
      )]),
      TreesRefused::NotAFile { path: path(b"d") },
    ),
  ]
}

/// The refusal of the tree at `path` by strict fsck's `finding`.
fn fsck(path: &[u8], finding: FsckId) -> TreesRefused {
  TreesRefused::Fsck {
    path: path.to_vec(),
    finding,
  }
}

/// `change` over `parent` is refused `refusal`, and the store is as it was.
fn refused_as(
  (store, trees): (&mut ContentStore, &mut Trees<History>),
  parent: ContentId,
  (change, refusal): (Change, TreesRefused),
) {
  let before = held(store);
  assert_eq!(trees.apply(store, Some(parent), &change), Err(refusal));
  assert_eq!(
    held(store),
    before,
    "a refused change leaves the store as it was"
  );
}

/// Every refusal of `refused_changes` in `format`, then a store of another window.
fn refuses_by_kind(format: ObjectFormat) {
  let mut store = store();
  let mut trees = trees(format);
  let base = Files::from([
    (b"a".to_vec(), (0, b"a\n".to_vec())),
    (b"dir/b".to_vec(), (0, b"b\n".to_vec())),
  ]);
  let all = entries(&mut store, &base);
  let parent = trees.build(&mut store, &all).unwrap();
  for case in refused_changes(&mut store, format) {
    refused_as((&mut store, &mut trees), parent, case);
  }
  let mut other = ContentStore::new(BUDGET, WINDOW * 2).unwrap();
  let refused = trees.build(&mut other, &BTreeMap::new());
  let mismatch = TreesRefused::WindowMismatch {
    declared: WINDOW,
    store: WINDOW * 2,
  };
  assert_eq!(refused, Err(mismatch));
}

/// A-664 (typed refusals; §4.17.2 "A push" step 4's checks on what the server makes). Do: over a version holding `a`
/// and `dir/b`, apply changes that remove an absent file, set a file over a directory and beneath a file, name a path
/// with a NUL or an empty name, name `.git` in a directory and at the root in any case, make `.gitmodules` a symbolic
/// link, name a file past 4096 bytes, link a submodule to the null id or to bytes no id is, give a class git has no mode
/// for, and name a content that is no file; then use a store of another window. Expect: each refused by its kind,
/// naming its path (a tree strict fsck refuses, its directory and finding), and the store as it was after each.
#[test]
fn t_a664_the_trees_refuse_by_kind_and_leave_the_store_as_it_was() {
  for format in FORMATS {
    refuses_by_kind(format);
  }
}

/// A-664 (hostile nodes; §4.9). Do: read as a directory a node with no payload, one whose id is short, one whose count
/// of subtrees is more than it names, one whose list is not a list, and a commit's node. Expect: each refused as
/// malformed, naming the node, with no panic.
#[test]
fn t_a664_a_node_that_is_no_directory_is_refused_as_malformed() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let trees = trees(format);
  let object = store.put(b"").unwrap();
  let mut head = vec![b'T'];
  head.extend_from_slice(&[0; 20]);
  head.extend_from_slice(&0_u64.to_le_bytes());
  let mut counted = head.clone();
  counted.extend_from_slice(&1_u32.to_le_bytes());
  counted.push(0);
  let mut listed = head.clone();
  listed.extend_from_slice(&1_u32.to_le_bytes());
  listed.push(1);
  let not_a_list = store.put_derived(&[], b"X").unwrap().identity;
  let nodes = [
    store.put_derived(&[object], b"").unwrap().identity,
    store.put_derived(&[object], &head[..10]).unwrap().identity,
    store.put_derived(&[object], &counted).unwrap().identity,
    store
      .put_derived(&[object, not_a_list], &listed)
      .unwrap()
      .identity,
    store
      .put_derived(&[], b"P01234567890123456789")
      .unwrap()
      .identity,
  ];
  for node in nodes {
    assert_eq!(
      trees.id(&store, node),
      Err(TreesRefused::Malformed { node })
    );
  }
}

/// `name` and `email` are refused `refusal`.
fn identity_refused((name, email, refusal): (&str, &str, IdentityRefused)) {
  assert_eq!(
    Identity::new(name, email),
    Err(refusal),
    "{name:?} <{email:?}>"
  );
}

/// `name` and `email` are an identity, as given.
fn identity_kept((name, email): (&str, &str)) {
  let identity = Identity::new(name, email).unwrap();
  assert_eq!((identity.name(), identity.email()), (name, email));
}

/// A-664 (an identity as git writes it, ident.c's `fmt_ident`; the golden cases `crud`, `inner` and `noemail` show stock
/// git doing each). Do: make identities of an empty name; a name or email holding `<`, `>`, a newline or a NUL; and a
/// name or email beginning or ending with a space, a tab, `,`, `:`, `;`, `"`, `\` or `'`; then of names and emails
/// git keeps as they are. Expect: the first refused by kind naming the part and the byte; the rest kept, a period at an
/// end, inner spaces, UTF-8 and an empty email among them.
#[test]
fn t_a664_an_identity_git_would_change_or_refuse_is_refused() {
  use IdentityPart::{Email, Name};
  let delimiter = |part, byte| IdentityRefused::Delimiter { part, byte };
  let crud = |part, byte| IdentityRefused::Crud { part, byte };
  [
    ("", "a@example.com", IdentityRefused::EmptyName),
    ("A <B", "a@example.com", delimiter(Name, b'<')),
    ("A", "a@exa>mple.com", delimiter(Email, b'>')),
    ("A", "a\n@example.com", delimiter(Email, b'\n')),
    ("A\0B", "a@example.com", delimiter(Name, 0)),
    (" Ada", "a@example.com", crud(Name, b' ')),
    ("Ada\t", "a@example.com", crud(Name, b'\t')),
    ("Ada,", "a@example.com", crud(Name, b',')),
    ("\"Ada", "a@example.com", crud(Name, b'"')),
    ("Ada", "'a@example.com", crud(Email, b'\'')),
    ("Ada", "a@example.com;", crud(Email, b';')),
    ("Ada", "a@example.com:", crud(Email, b':')),
    ("Ada", "\\a@example.com", crud(Email, b'\\')),
  ]
  .into_iter()
  .for_each(identity_refused);
  [
    ("A U Thor", "author@example.com"),
    ("Ada L.", "ada@example.com."),
    ("\u{c1}d\u{e1}", "\u{e1}da@example.com"),
    ("A", ""),
  ]
  .into_iter()
  .for_each(identity_kept);
}

/// The commits of `format` over the tests' store, eager.
fn commits(format: ObjectFormat) -> Commits<History> {
  Commits::declare(format, WINDOW, Computed::Eager)
}

/// A-664 (§4.17.1 item 3: "the parent being the previous version's commit"). Do: record a version a stock client pushed
/// (its head the client's commit), a version the server makes on it, and a merge the server makes of the second and the
/// first, and derive each version's trees and commit through the derived-data mechanism. Expect: the pushed version's
/// commit the client's, with no bytes of its own; the made one's the naive `git commit-tree -m` commit of its tree with
/// the client's commit as parent; the merge's with both parents in order; each node the commit's object and its tree.
#[test]
fn t_a664_a_pushed_versions_commit_is_the_clients_and_the_next_made_one_names_it() {
  for format in FORMATS {
    let mut fixture = Chain::new(format);
    let client = name_of(format, Kind::Commit, b"a commit a client made").unwrap();
    let first = Files::from([(b"f".to_vec(), (0, b"one\n".to_vec()))]);
    fixture.record(0, &[], (&first, &Files::new()), Commit::Pushed(client));
    let second = Files::from([(b"f".to_vec(), (0, b"two\n".to_vec()))]);
    fixture.record(
      1,
      &[0],
      (&second, &first),
      server("A U Thor", 1112911993, "two"),
    );
    let third = Files::from([(b"g/h".to_vec(), (1, b"#!\n".to_vec()))]);
    let merged: Files = second.clone().into_iter().chain(third).collect();
    fixture.record(
      2,
      &[1, 0],
      (&merged, &second),
      server("C O Mitter", 1112912000, "merge\n"),
    );
    assert_eq!((fixture.commit(0), fixture.object(0)), (client, None));
    let made = naive_commit(
      format,
      (naive_tree(format, &second), &[client]),
      ("A U Thor", "a@example.com", 1112911993),
      "two",
    );
    assert_eq!(
      (fixture.commit(1), fixture.object(1)),
      (made.0, Some(made.1))
    );
    let merge = naive_commit(
      format,
      (naive_tree(format, &merged), &[made.0, client]),
      ("C O Mitter", "a@example.com", 1112912000),
      "merge\n",
    );
    assert_eq!(
      (fixture.commit(2), fixture.object(2)),
      (merge.0, Some(merge.1))
    );
    let counters = fixture.commits.counters();
    assert_eq!((counters.pushed, counters.made), (1, 2));
  }
}

/// A made commit's record: `name` and `a@example.com` at `seconds`, with `message`.
fn server(name: &str, seconds: u64, message: &str) -> Commit {
  Commit::Server {
    name: name.to_owned(),
    email: "a@example.com".to_owned(),
    seconds,
    message: message.to_owned(),
  }
}

/// A history derived as it is recorded: each version's trees, then its commit.
struct Chain {
  store: ContentStore,
  history: History,
  derivations: slates_vcs::Derivations,
  trees: Trees<History>,
  commits: Commits<History>,
  roots: BTreeMap<u64, ContentId>,
}

impl Chain {
  fn new(format: ObjectFormat) -> Chain {
    Chain {
      store: store(),
      history: History::default(),
      derivations: slates_vcs::Derivations::new(),
      trees: trees(format),
      commits: commits(format),
      roots: BTreeMap::new(),
    }
  }

  /// Records `version` over `parents`, its files `now` stated against its first parent's `before`, and derives its
  /// trees and then its commit, eagerly.
  fn record(
    &mut self,
    version: u64,
    parents: &[u64],
    (now, before): (&Files, &Files),
    commit: Commit,
  ) {
    let mut change = Change::new();
    for (path, file) in now {
      if before.get(path) != Some(file) {
        change.insert(path.clone(), Some(entry(&mut self.store, file)));
      }
    }
    for path in before.keys().filter(|path| !now.contains_key(*path)) {
      change.insert(path.clone(), None);
    }
    self.history.record(version, parents, change, commit);
    let tree = oracle::fill_recorded(
      &mut self.derivations,
      &mut self.trees,
      &self.history,
      &mut self.store,
      version,
    )
    .0
    .unwrap();
    self.history.trees.insert(version, tree);
    let commit = oracle::fill_recorded(
      &mut self.derivations,
      &mut self.commits,
      &self.history,
      &mut self.store,
      version,
    )
    .0
    .unwrap();
    self.roots.insert(version, commit);
  }

  /// `version`'s commit id.
  fn commit(&self, version: u64) -> ObjectId {
    self.commits.id(&self.store, self.roots[&version]).unwrap()
  }

  /// `version`'s made commit's bytes, if the server made it.
  fn object(&self, version: u64) -> Option<Vec<u8>> {
    self
      .commits
      .object(&self.store, self.roots[&version])
      .unwrap()
  }
}

/// A-664 (typed refusals; §4.17.1 item 4). Do: make commits whose time is past what git reads, whose message holds a
/// NUL, whose tree is a commit's node, whose parent is of the other format, and a pushed one of the other format; then
/// make one over a store of another window. Expect: each refused by kind (the NUL as strict fsck's `nulInCommit`), and
/// the store as it was after each.
#[test]
fn t_a664_the_commits_refuse_by_kind_and_leave_the_store_as_it_was() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut trees = trees(format);
  let mut commits = commits(format);
  let tree = trees.build(&mut store, &BTreeMap::new()).unwrap();
  let identity = Identity::new("A U Thor", "author@example.com").unwrap();
  let fields = |tree, seconds, message| Fields {
    tree,
    identity,
    seconds,
    message,
  };
  let made = commits
    .make(&mut store, &fields(tree, 0, "ok"), &[])
    .unwrap();
  let foreign = name_of(ObjectFormat::Sha256, Kind::Commit, b"x").unwrap();
  let cases: [(Fields<'_>, Vec<ObjectId>, CommitsRefused); 4] = [
    (
      fields(tree, u64::MAX, "m"),
      vec![],
      CommitsRefused::DateOverflow { seconds: u64::MAX },
    ),
    (
      fields(tree, 0, "a\0b"),
      vec![],
      CommitsRefused::Fsck {
        finding: FsckId::NulInCommit,
      },
    ),
    (
      fields(made, 0, "m"),
      vec![],
      CommitsRefused::Malformed { node: made },
    ),
    (
      fields(tree, 0, "m"),
      vec![foreign],
      CommitsRefused::ForeignId { id: foreign },
    ),
  ];
  for case in cases {
    commit_refused((&mut store, &mut commits), case);
  }
  assert_eq!(
    commits.pushed(&mut store, foreign),
    Err(CommitsRefused::ForeignId { id: foreign })
  );
  let mut other = ContentStore::new(BUDGET, WINDOW * 2).unwrap();
  assert_eq!(
    commits.pushed(&mut other, name_of(format, Kind::Commit, b"y").unwrap()),
    Err(CommitsRefused::WindowMismatch {
      declared: WINDOW,
      store: WINDOW * 2,
    })
  );
}

/// The commit of `fields` over `parents` is refused `refusal`, and the store is as it was.
fn commit_refused(
  (store, commits): (&mut ContentStore, &mut Commits<History>),
  (fields, parents, refusal): (Fields<'_>, Vec<ObjectId>, CommitsRefused),
) {
  let before = held(store);
  assert_eq!(commits.make(store, &fields, &parents), Err(refusal));
  assert_eq!(
    held(store),
    before,
    "a refused commit leaves the store as it was"
  );
}

/// A-664 (a blob past one window, A-581's windows). Do: build a version holding a file of 5000 bytes, more than four of
/// the store's windows, in both formats. Expect: its blob's id is the one git names of the whole bytes, read a window at
/// a time.
#[test]
fn t_a664_a_file_past_one_window_is_named_from_its_windows() {
  for format in FORMATS {
    let mut store = store();
    let mut trees = trees(format);
    let bytes: Vec<u8> = (0..5000_u32)
      .map(|at| u8::try_from(at % 251).unwrap())
      .collect();
    let files = Files::from([(b"big".to_vec(), (0, bytes.clone()))]);
    let all = entries(&mut store, &files);
    let root = trees.build(&mut store, &all).unwrap();
    let blob = name_of(format, Kind::Blob, &bytes).unwrap();
    let got = trees.get(&store, root, b"big").unwrap();
    assert_eq!(
      got,
      Some(Item::File {
        mode: Mode::File,
        id: blob
      })
    );
  }
}
