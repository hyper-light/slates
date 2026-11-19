//! A-618's tests (§4.17.1 "A push is a work", shared piece 4): pushes built from trees in a content-addressed store,
//! submitted to a green and read back. The oracle walks both whole trees and compares them path by path; the build must
//! make its work, byte for byte, while reading only the subtrees whose ids changed, which its counters and the store's
//! own prove; and every accepted push must leave the green holding exactly what the client pushed (the client-fidelity
//! test, item 4).

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::{
  Built, EntryKind, HEAD_ATTRIBUTE, ObjectKey, Push, PushObjects, PushRefused, ROOT, Reads,
  TreeEntry, Unavailable, build, identity,
};
use crate::engine::{Green, Increment, Outcome};
use crate::increment::{DeriveError, VolumeOp, compose_volume_planned};
use crate::ops_doc::OpKind;
use crate::verdict::MergeConflictClass;

/// An object of the store.
#[derive(Clone, Debug)]
enum Object {
  /// A tree's entries.
  Tree(Vec<TreeEntry>),
  /// A blob's bytes.
  Blob(Vec<u8>),
}

/// A content-addressed store, as a tool's is: an object's id is the hash of its kind and bytes, so a subtree that did not
/// change keeps its id. It counts what it is asked, so a test can prove from the store's side what a build read.
#[derive(Default)]
struct Store {
  /// The objects by id.
  objects: BTreeMap<ObjectKey, Object>,
  /// Trees given.
  trees_given: Cell<u64>,
  /// Blobs whose bytes were given.
  blobs_given: Cell<u64>,
}

/// An id of twenty bytes, as SHA-1's, from the hash of a kind and bytes.
fn key(kind: &[u8], bytes: &[u8]) -> ObjectKey {
  let mut hasher = blake3::Hasher::new();
  hasher.update(kind);
  hasher.update(&(bytes.len() as u64).to_le_bytes());
  hasher.update(bytes);
  ObjectKey::new(&hasher.finalize().as_bytes()[..20]).unwrap()
}

impl Store {
  /// Puts a blob; its id.
  fn put_blob(&mut self, bytes: &[u8]) -> ObjectKey {
    let id = key(b"blob", bytes);
    self.objects.insert(id, Object::Blob(bytes.to_vec()));
    id
  }

  /// Puts a tree of `entries`, whatever their order; its id, from its entries sorted by name.
  fn put_tree(&mut self, mut entries: Vec<TreeEntry>) -> ObjectKey {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut encoded = Vec::new();
    for entry in &entries {
      encoded.push(entry.kind as u8);
      encoded.extend_from_slice(&entry.name);
      encoded.push(0);
      encoded.extend_from_slice(entry.id.as_bytes());
    }
    let id = key(b"tree", &encoded);
    self.objects.insert(id, Object::Tree(entries));
    id
  }
}

impl PushObjects for Store {
  fn tree(&self, id: ObjectKey) -> Result<Vec<TreeEntry>, Unavailable> {
    self.trees_given.set(self.trees_given.get() + 1);
    match self.objects.get(&id) {
      Some(Object::Tree(entries)) => Ok(entries.clone()),
      Some(Object::Blob(_)) => Err(Unavailable::NotATree),
      None => Err(Unavailable::Missing),
    }
  }

  fn blob_len(&self, id: ObjectKey) -> Result<u64, Unavailable> {
    match self.objects.get(&id) {
      Some(Object::Blob(bytes)) => Ok(bytes.len() as u64),
      Some(Object::Tree(_)) => Err(Unavailable::NotABlob),
      None => Err(Unavailable::Missing),
    }
  }

  fn blob(&self, id: ObjectKey) -> Result<Cow<'_, [u8]>, Unavailable> {
    self.blobs_given.set(self.blobs_given.get() + 1);
    match self.objects.get(&id) {
      Some(Object::Blob(bytes)) => Ok(Cow::Borrowed(bytes)),
      Some(Object::Tree(_)) => Err(Unavailable::NotABlob),
      None => Err(Unavailable::Missing),
    }
  }
}

/// What a path of a branch holds.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
  /// A file: its bytes, and whether it is executable.
  File(Vec<u8>, bool),
  /// A symbolic link and its target.
  Link(String),
}

/// A branch's tree as its paths: files and links by path, each directory implied by the paths beneath it, as git's
/// trees hold them.
type Files = BTreeMap<String, Node>;

/// A tree of nested directories, as `put` builds it.
#[derive(Default)]
struct Dir {
  /// The entries by name.
  entries: BTreeMap<String, DirEntry>,
}

/// An entry of a [`Dir`].
enum DirEntry {
  /// A file or a link.
  Leaf(Node),
  /// A subtree.
  Dir(Dir),
}

/// Puts `files` in `store` as trees, one a directory: the root's id.
fn put(store: &mut Store, files: &Files) -> ObjectKey {
  let mut root = Dir::default();
  for (path, node) in files {
    let mut dir = &mut root;
    let mut parts: Vec<&str> = path.split('/').collect();
    let last = parts.pop().unwrap();
    for part in parts {
      let entry = dir
        .entries
        .entry(part.to_owned())
        .or_insert_with(|| DirEntry::Dir(Dir::default()));
      let DirEntry::Dir(inner) = entry else {
        panic!("{path} under a file");
      };
      dir = inner;
    }
    dir
      .entries
      .insert(last.to_owned(), DirEntry::Leaf(node.clone()));
  }
  put_dir(store, &root)
}

/// Puts one directory and everything beneath it; its id.
fn put_dir(store: &mut Store, dir: &Dir) -> ObjectKey {
  let mut entries = Vec::new();
  for (name, entry) in &dir.entries {
    let (kind, id) = match entry {
      DirEntry::Dir(inner) => (EntryKind::Directory, put_dir(store, inner)),
      DirEntry::Leaf(Node::File(bytes, false)) => (EntryKind::File, store.put_blob(bytes)),
      DirEntry::Leaf(Node::File(bytes, true)) => (EntryKind::Executable, store.put_blob(bytes)),
      DirEntry::Leaf(Node::Link(target)) => (EntryKind::Symlink, store.put_blob(target.as_bytes())),
    };
    entries.push(TreeEntry {
      name: name.as_bytes().to_vec(),
      kind,
      id,
    });
  }
  store.put_tree(entries)
}

/// Every path of the tree `root` and what it is, its subtrees included, by walking the whole tree (the oracle's way):
/// the root itself under the empty path.
fn flatten(store: &Store, root: Option<ObjectKey>) -> BTreeMap<String, (EntryKind, ObjectKey)> {
  let mut out = BTreeMap::new();
  let Some(root) = root else {
    return out;
  };
  out.insert(String::new(), (EntryKind::Directory, root));
  let mut pending = vec![(String::new(), root)];
  while let Some((path, id)) = pending.pop() {
    let Some(Object::Tree(entries)) = store.objects.get(&id) else {
      panic!("no tree {id:?}");
    };
    for entry in entries {
      let name = String::from_utf8(entry.name.clone()).unwrap();
      let child = if path.is_empty() {
        name
      } else {
        format!("{path}/{name}")
      };
      if entry.kind == EntryKind::Directory {
        pending.push((child.clone(), entry.id));
      }
      out.insert(child, (entry.kind, entry.id));
    }
  }
  out
}

/// The bytes of blob `id`, read past the counters.
fn bytes_of(store: &Store, id: ObjectKey) -> Vec<u8> {
  match store.objects.get(&id) {
    Some(Object::Blob(bytes)) => bytes.clone(),
    other => panic!("no blob {id:?}: {other:?}"),
  }
}

/// Whether two kinds are both files, executable or not.
fn files(a: EntryKind, b: EntryKind) -> bool {
  let file = |kind| matches!(kind, EntryKind::File | EntryKind::Executable);
  file(a) && file(b)
}

/// What the oracle expects of a push from `base` to `pushed`.
#[derive(Debug)]
enum Expected {
  /// The work, and what the build must have read.
  Work(Increment, Reads),
  /// A refusal for a path changing kind: the paths that do.
  KindChanged(BTreeSet<String>),
}

/// The oracle: compares the two whole trees path by path, with neither identity nor order taken from the build, and
/// makes the journal the design states (§4.17.1 item 2): removals deepest first, then additions and changes parents
/// first, then the head. It composes it against the green's base, lays the post-state from the trees' own bytes, and
/// counts the reads a walk by identity must make: both trees of each directory whose id changed, the one tree of a
/// directory on one side alone, a skip for each directory whose id stayed beneath one that was read.
fn oracle(store: &Store, green: &Green, push: &Push<'_>) -> Expected {
  let based = flatten(store, push.base_tree);
  let given = flatten(store, Some(push.tree));
  let changed_kind: BTreeSet<String> = based
    .iter()
    .filter_map(|(path, (was, _))| {
      let (now, _) = given.get(path)?;
      (was != now && !files(*was, *now)).then(|| path.clone())
    })
    .collect();
  if !changed_kind.is_empty() {
    return Expected::KindChanged(changed_kind);
  }
  let mut journal = Vec::new();
  for (path, (kind, _)) in based.iter().rev() {
    if given.contains_key(path) {
      continue;
    }
    journal.push(match kind {
      EntryKind::Directory => VolumeOp::Rmdir { path: path.clone() },
      _ => VolumeOp::Unlink { path: path.clone() },
    });
  }
  let mut contents: BTreeMap<String, Vec<u8>> = BTreeMap::new();
  let mut reads = Reads::default();
  for (path, (kind, id)) in &given {
    let was = based.get(path);
    if path.is_empty() {
      continue;
    }
    let same = was == Some(&(*kind, *id));
    match kind {
      EntryKind::Directory if was.is_none() => journal.push(VolumeOp::Mkdir { path: path.clone() }),
      EntryKind::Directory => {}
      EntryKind::Symlink if !same => {
        let target = String::from_utf8(bytes_of(store, *id)).unwrap();
        reads.blobs += 1;
        if was.is_some() {
          journal.push(VolumeOp::Unlink { path: path.clone() });
        }
        journal.push(VolumeOp::Symlink {
          path: path.clone(),
          target,
        });
      }
      EntryKind::Symlink => {}
      EntryKind::File | EntryKind::Executable => {
        let bytes = bytes_of(store, *id);
        let content_changed = was.is_none_or(|(_, was_id)| was_id != id);
        if content_changed {
          match was {
            None => journal.push(VolumeOp::Create { path: path.clone() }),
            Some(_) => journal.push(VolumeOp::Truncate {
              path: path.clone(),
              len: 0,
            }),
          }
          if !bytes.is_empty() {
            reads.blobs += 1;
            journal.push(VolumeOp::Extend {
              path: path.clone(),
              at: 0,
              len: bytes.len() as u64,
            });
            contents.insert(path.clone(), bytes);
          }
        }
        let executable = *kind == EntryKind::Executable;
        let was_executable = was.is_some_and(|(kind, _)| *kind == EntryKind::Executable);
        if (was.is_none() && executable) || (was.is_some() && executable != was_executable) {
          journal.push(VolumeOp::SetMode {
            path: path.clone(),
            mode: if executable { 0o755 } else { 0o644 },
          });
        }
      }
      EntryKind::Submodule => panic!("the generator makes no submodule"),
    }
  }
  if let Some(head) = push.head {
    journal.push(VolumeOp::SetXattr {
      path: ROOT.to_owned(),
      name: HEAD_ATTRIBUTE.to_owned(),
      value: head.to_vec(),
    });
  }
  // The reads a walk by identity makes: a directory's trees are read when its parent's were (the root always) and its
  // id changed; a directory whose id stayed beneath a read parent is one skip.
  let directories: BTreeSet<&String> = based
    .iter()
    .chain(given.iter())
    .filter(|(_, (kind, _))| *kind == EntryKind::Directory)
    .map(|(path, _)| path)
    .collect();
  let same = |path: &str| {
    let was = based.get(path);
    was.is_some() && was == given.get(path)
  };
  let parent_read = |path: &str| match path.rfind('/') {
    _ if path.is_empty() => true,
    Some(at) => !same(&path[..at]),
    None => !same(""),
  };
  for path in directories {
    if !parent_read(path) {
      continue;
    }
    if same(path) {
      reads.skipped += 1;
    } else {
      reads.trees +=
        u64::from(based.contains_key(path.as_str())) + u64::from(given.contains_key(path.as_str()));
    }
  }
  let base = green.base_for(push.base_version, &journal);
  let (doc, plan) = compose_volume_planned(&base, &journal).unwrap();
  let mut post = Vec::new();
  for op in doc.ops.iter().filter(|op| op.src != u64::MAX) {
    let path = doc.paths.path(op.path).unwrap();
    let run: &[u8] = match op.kind {
      OpKind::Insert | OpKind::Extend | OpKind::Overwrite => {
        let from = usize::try_from(plan[&op.src]).unwrap();
        &contents[path][from..from + usize::try_from(op.len).unwrap()]
      }
      OpKind::SetXattr => push.head.unwrap(),
      _ => continue,
    };
    let at = usize::try_from(op.src).unwrap();
    if post.len() < at + run.len() {
      post.resize(at + run.len(), 0);
    }
    post[at..at + run.len()].copy_from_slice(run);
  }
  let id = identity(&push.name, push.base_version, &doc, &post);
  Expected::Work(
    Increment {
      id,
      base: push.base_version,
      doc,
      post_state: post,
      evidence: Vec::new(),
    },
    reads,
  )
}

/// The client-fidelity test (§4.17.1 item 4): `green` holds exactly `files`, each file's bytes and executable bit, each
/// link's target, the directories they imply and nothing else, and its root's head is `head`.
fn holds_exactly(green: &Green, files: &Files, head: &[u8]) {
  let mut wanted_dirs = BTreeSet::new();
  for (path, node) in files {
    let mut at = path.as_str();
    while let Some(slash) = at.rfind('/') {
      at = &at[..slash];
      wanted_dirs.insert(at.to_owned());
    }
    match node {
      Node::File(bytes, executable) => {
        assert_eq!(
          green.content(path),
          Some(bytes.as_slice()),
          "{path}'s bytes"
        );
        let mode = green.mode(path).unwrap_or(0o644);
        assert_eq!(mode & 0o111 != 0, *executable, "{path}'s mode {mode:o}");
      }
      Node::Link(target) => assert_eq!(
        green.symlink(path),
        Some(target.as_str()),
        "{path}'s target"
      ),
    }
  }
  let held_files: BTreeSet<String> = green.files().map(|(path, _)| path).collect();
  let wanted_files: BTreeSet<String> = files
    .iter()
    .filter(|(_, node)| matches!(node, Node::File(..)))
    .map(|(path, _)| path.clone())
    .collect();
  assert_eq!(held_files, wanted_files, "the files and nothing else");
  let base = green.current_base();
  let held_links: BTreeSet<&String> = base.symlinks.iter().map(|(path, _)| path).collect();
  let wanted_links: BTreeSet<&String> = files
    .iter()
    .filter(|(_, node)| matches!(node, Node::Link(_)))
    .map(|(path, _)| path)
    .collect();
  assert_eq!(held_links, wanted_links, "the links and nothing else");
  let held_dirs: BTreeSet<String> = base.dirs.iter().cloned().collect();
  assert_eq!(held_dirs, wanted_dirs, "the directories and nothing else");
  assert_eq!(green.xattr(ROOT, HEAD_ATTRIBUTE), Some(head), "the head");
}

/// A head for the `n`th push of a history, twenty bytes as git's SHA-1 commit ids.
fn head(n: u64) -> Vec<u8> {
  blake3::hash(&n.to_le_bytes()).as_bytes()[..20].to_vec()
}

/// A push of `tree` over `base_tree` at `base_version`, named for its head.
fn push<'a>(
  base_version: u64,
  base_tree: Option<ObjectKey>,
  tree: ObjectKey,
  head: &'a [u8],
) -> Push<'a> {
  Push {
    name: *blake3::hash(head).as_bytes(),
    base_version,
    base_tree,
    tree,
    head: Some(head),
  }
}

/// `files` from pairs.
fn files_of(pairs: &[(&str, Node)]) -> Files {
  pairs
    .iter()
    .map(|(path, node)| ((*path).to_owned(), node.clone()))
    .collect()
}

/// A file of `bytes`, not executable.
fn file(bytes: &[u8]) -> Node {
  Node::File(bytes.to_vec(), false)
}

/// Builds and submits a push of `files` over the branch `green` holds at its head, whose tree is `base_tree`: the
/// build, its outcome, and the pushed tree's id. Every build is held to the oracle's work and reads first.
fn land(
  store: &mut Store,
  green: &mut Green,
  base_tree: Option<ObjectKey>,
  files: &Files,
  head: &[u8],
) -> (Built, Outcome, ObjectKey) {
  let tree = put(store, files);
  let push = push(green.head(), base_tree, tree, head);
  let built = build(&push, green, store, u64::MAX).unwrap();
  let Expected::Work(increment, reads) = oracle(store, green, &push) else {
    panic!("the oracle refused what the build made");
  };
  assert_eq!(built.increment, increment, "the oracle's work");
  assert_eq!(built.reads, reads, "the oracle's reads");
  let outcome = green.submit(&built.increment);
  (built, outcome, tree)
}

/// The kinds of a document's operations, in its order.
fn kinds(increment: &Increment) -> Vec<(String, OpKind, u64, u64)> {
  increment
    .doc
    .ops
    .iter()
    .map(|op| {
      (
        increment.doc.paths.path(op.path).unwrap().to_owned(),
        op.kind,
        op.at,
        op.len,
      )
    })
    .collect()
}

/// A-618 (§4.17.1 items 2 to 4). Do: push a branch's first tree to an empty green: nested directories, an empty file,
/// an executable and a symbolic link. Expect: one work the oracle makes too, every directory read once and none
/// skipped, accepted as version 1, and the green holding exactly the pushed tree with its head.
#[test]
fn a_first_push_creates_every_path_and_sets_the_head() {
  let mut store = Store::default();
  let mut green = Green::new();
  let files = files_of(&[
    ("README", file(b"read me\n")),
    ("empty", file(b"")),
    ("run.sh", Node::File(b"#!/bin/sh\n".to_vec(), true)),
    ("src/lib.rs", file(b"fn main() {}\n")),
    ("src/deep/mod.rs", file(b"mod deep;\n")),
    ("latest", Node::Link("src/lib.rs".to_owned())),
  ]);
  let (built, outcome, _) = land(&mut store, &mut green, None, &files, &head(1));
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  assert_eq!(
    built.reads,
    Reads {
      trees: 3,
      blobs: 5,
      skipped: 0
    }
  );
  holds_exactly(&green, &files, &head(1));
}

/// A-618 (§4.17.1 item 2: "never walking unchanged subtrees"). Do: push a tree that changes one file in one of three
/// directories, the other two each holding a deep subtree. Expect: the root's two trees and the changed directory's two
/// read, both unchanged directories skipped unread, by the build's count and by the store's; one blob read; the work
/// one `Delete` of the base length and one `Insert` of the new bytes at that file, and the head.
#[test]
fn a_push_reads_only_the_subtrees_whose_ids_changed() {
  let mut store = Store::default();
  let mut green = Green::new();
  let mut files = files_of(&[
    ("a/x/y/z", file(b"deep\n")),
    ("a/x/w", file(b"w\n")),
    ("b/f", file(b"twelve bytes")),
    ("c/x/y/z", file(b"also deep\n")),
  ]);
  let (_, outcome, base_tree) = land(&mut store, &mut green, None, &files, &head(1));
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  files.insert("b/f".to_owned(), file(b"seven b"));
  let tree = put(&mut store, &files);
  let second_head = head(2);
  let push = push(1, Some(base_tree), tree, &second_head);
  store.trees_given.set(0);
  store.blobs_given.set(0);
  let built = build(&push, &green, &store, u64::MAX).unwrap();
  assert_eq!(
    built.reads,
    Reads {
      trees: 4,
      blobs: 1,
      skipped: 2
    }
  );
  assert_eq!(
    (store.trees_given.get(), store.blobs_given.get()),
    (4, 1),
    "the store's own count"
  );
  let ops: BTreeSet<(String, String, u64, u64)> = kinds(&built.increment)
    .into_iter()
    .map(|(path, kind, at, len)| {
      (
        path,
        kind,
        if kind == OpKind::SetXattr { 0 } else { at },
        len,
      )
    })
    .collect();
  let expected: BTreeSet<(String, String, u64, u64)> = [
    (String::new(), format!("{:?}", OpKind::SetXattr), 0, 20),
    ("b/f".to_owned(), format!("{:?}", OpKind::Delete), 0, 12),
    ("b/f".to_owned(), format!("{:?}", OpKind::Insert), 0, 7),
  ]
  .into_iter()
  .collect();
  assert_eq!(ops, expected, "the head, and the file replaced whole");
  assert_eq!(
    green.submit(&built.increment),
    Outcome::Accepted { version: 2 }
  );
  holds_exactly(&green, &files, &head(2));
}

/// A-618 (§4.17.1 item 2). Do: push the executable bit alone onto a file, then off it, then a symbolic link's new
/// target, then the removal of a directory with a file and a subdirectory in it. Expect: a `SetMode` 0755 and no
/// content, then a `SetMode` 0644, then one `Symlink`, then the directory's files unlinked and both directories removed;
/// each accepted and the green holding exactly each pushed tree.
#[test]
fn modes_links_and_removed_directories_are_the_operations_the_design_names() {
  let mut store = Store::default();
  let mut green = Green::new();
  let mut files = files_of(&[
    ("tool", file(b"#!/bin/sh\n")),
    ("link", Node::Link("tool".to_owned())),
    ("gone/a", file(b"a\n")),
    ("gone/deeper/b", file(b"b\n")),
    ("kept", file(b"kept\n")),
  ]);
  let (_, _, mut tree) = land(&mut store, &mut green, None, &files, &head(1));
  let mut step = |files: &Files, n: u64, tree: &mut ObjectKey| {
    let (built, outcome, pushed) = land(&mut store, &mut green, Some(*tree), files, &head(n));
    assert_eq!(outcome, Outcome::Accepted { version: n });
    holds_exactly(&green, files, &head(n));
    *tree = pushed;
    kinds(&built.increment)
      .into_iter()
      .filter(|(path, ..)| !path.is_empty())
      .map(|(path, kind, _, len)| (path, kind, len))
      .collect::<Vec<_>>()
  };
  files.insert("tool".to_owned(), Node::File(b"#!/bin/sh\n".to_vec(), true));
  assert_eq!(
    step(&files, 2, &mut tree),
    [("tool".to_owned(), OpKind::SetMode, 0o755)]
  );
  files.insert("tool".to_owned(), file(b"#!/bin/sh\n"));
  assert_eq!(
    step(&files, 3, &mut tree),
    [("tool".to_owned(), OpKind::SetMode, 0o644)]
  );
  files.insert("link".to_owned(), Node::Link("kept".to_owned()));
  let ops = step(&files, 4, &mut tree);
  assert_eq!(ops.len(), 1);
  assert_eq!((ops[0].0.as_str(), ops[0].1), ("link", OpKind::Symlink));
  files.remove("gone/a");
  files.remove("gone/deeper/b");
  let removed: BTreeSet<(String, String)> = step(&files, 5, &mut tree)
    .into_iter()
    .map(|(path, kind, _)| (path, format!("{kind:?}")))
    .collect();
  let expected: BTreeSet<(String, String)> = [
    ("gone", OpKind::Rmdir),
    ("gone/a", OpKind::Unlink),
    ("gone/deeper", OpKind::Rmdir),
    ("gone/deeper/b", OpKind::Unlink),
  ]
  .into_iter()
  .map(|(path, kind)| (path.to_owned(), format!("{kind:?}")))
  .collect();
  assert_eq!(removed, expected);
}

/// A-618 (§4.17.1 item 3, the head operation; the root, `""`, always present). Do: land a push at version 2, then
/// submit two more pushes from version 1: one with another head that changes a different file, and one with the head
/// and the tree of the push that landed. Expect: the first meets the landed push on the root's head and is a `MetaMeta`
/// conflict there, the green unchanged; the second accepts with nothing changed, as the push already happened.
#[test]
fn two_pushes_from_one_base_meet_on_the_roots_head() {
  let mut store = Store::default();
  let mut green = Green::new();
  let base_files = files_of(&[("a", file(b"a\n")), ("b", file(b"b\n"))]);
  let (_, _, base_tree) = land(&mut store, &mut green, None, &base_files, &head(1));
  let mut first = base_files.clone();
  first.insert("a".to_owned(), file(b"first\n"));
  let first_tree = put(&mut store, &first);
  let landed = build(
    &push(1, Some(base_tree), first_tree, &head(2)),
    &green,
    &store,
    u64::MAX,
  )
  .unwrap();
  assert_eq!(
    green.submit(&landed.increment),
    Outcome::Accepted { version: 2 }
  );
  let mut second = base_files.clone();
  second.insert("b".to_owned(), file(b"second\n"));
  let second_tree = put(&mut store, &second);
  let other = build(
    &push(1, Some(base_tree), second_tree, &head(3)),
    &green,
    &store,
    u64::MAX,
  )
  .unwrap();
  let Outcome::Conflict { windows } = green.submit(&other.increment) else {
    panic!("another head from the same base must conflict");
  };
  assert_eq!(windows.len(), 1);
  assert_eq!(
    (windows[0].path.as_str(), windows[0].class),
    (ROOT, MergeConflictClass::MetaMeta)
  );
  holds_exactly(&green, &first, &head(2));
  let landed_head = head(2);
  let mut again = push(1, Some(base_tree), first_tree, &landed_head);
  again.name = [7; 32];
  let same = build(&again, &green, &store, u64::MAX).unwrap();
  assert_ne!(
    same.increment.id, landed.increment.id,
    "another submit of the same commits"
  );
  assert_eq!(
    green.submit(&same.increment),
    Outcome::Accepted { version: 3 }
  );
  holds_exactly(&green, &first, &head(2));
}

/// A-618 (the green's root is the empty path, always present for modes and attributes: what the head operation
/// needs, and what an agent's `setxattr` or `chmod` on a mount's root declares). Do: compose and submit an attribute
/// and a mode on the root to an empty green. Expect: both compose, land as version 1, and read back.
#[test]
fn an_attribute_and_a_mode_on_the_root_compose_and_land() {
  let mut green = Green::new();
  let journal = [
    VolumeOp::SetXattr {
      path: ROOT.to_owned(),
      name: "user.note".to_owned(),
      value: b"root".to_vec(),
    },
    VolumeOp::SetMode {
      path: ROOT.to_owned(),
      mode: 0o700,
    },
  ];
  let base = green.base_for(0, &journal);
  let (doc, plan) = compose_volume_planned(&base, &journal).expect("the root is present");
  assert!(plan.is_empty());
  let xattr = doc
    .ops
    .iter()
    .find(|op| op.kind == OpKind::SetXattr)
    .unwrap();
  let at = usize::try_from(xattr.src).unwrap();
  let mut post = vec![0; at + usize::try_from(xattr.len).unwrap()];
  post[at..].copy_from_slice(b"root");
  let increment = Increment {
    id: [9; 32],
    base: 0,
    doc,
    post_state: post,
    evidence: Vec::new(),
  };
  assert_eq!(green.submit(&increment), Outcome::Accepted { version: 1 });
  assert_eq!(green.xattr(ROOT, "user.note"), Some(&b"root"[..]));
  assert_eq!(green.mode(ROOT), Some(0o700));
}

/// A store of one root tree with `entries`, beside the objects they name.
fn tree_with(store: &mut Store, entries: Vec<TreeEntry>) -> ObjectKey {
  store.put_tree(entries)
}

/// An entry named `name` of `kind` naming `id`.
fn entry(name: &[u8], kind: EntryKind, id: ObjectKey) -> TreeEntry {
  TreeEntry {
    name: name.to_vec(),
    kind,
    id,
  }
}

/// `push`'s refusal to an empty green.
fn refusal(store: &Store, tree: ObjectKey) -> PushRefused {
  build(
    &push(0, None, tree, &head(1)),
    &Green::new(),
    store,
    u64::MAX,
  )
  .unwrap_err()
}

/// A-618 (§4.17.1 objects items 3 to 5). Do: push trees naming a blob the store lacks, a tree it lacks, and a blob as a
/// tree. Expect: each refused naming the path and the id, `Missing` or `NotATree`.
#[test]
fn an_object_the_store_cannot_give_is_refused_with_its_path() {
  let mut store = Store::default();
  let absent = key(b"blob", b"never put");
  let tree = tree_with(&mut store, vec![entry(b"lost", EntryKind::File, absent)]);
  assert_eq!(
    refusal(&store, tree),
    PushRefused::Object {
      path: "lost".to_owned(),
      id: absent,
      why: Unavailable::Missing
    }
  );
  let blob = store.put_blob(b"not a tree");
  let tree = tree_with(&mut store, vec![entry(b"dir", EntryKind::Directory, blob)]);
  assert_eq!(
    refusal(&store, tree),
    PushRefused::Object {
      path: "dir".to_owned(),
      id: blob,
      why: Unavailable::NotATree
    }
  );
  let gone = key(b"tree", b"never put");
  assert_eq!(
    refusal(&store, gone),
    PushRefused::Object {
      path: String::new(),
      id: gone,
      why: Unavailable::Missing
    }
  );
}

/// A-618. Do: push a tree holding each name slates cannot hold in a path (not UTF-8, empty, `.`, `..`, holding `/`,
/// holding NUL), one at a time and once beneath a directory; a tree holding one name twice; links whose targets are not
/// UTF-8 or hold NUL. Expect: each refused by kind with its path.
#[test]
fn names_and_targets_slates_cannot_hold_are_refused_by_kind() {
  let mut store = Store::default();
  let blob = store.put_blob(b"x");
  for name in [&b"\xff"[..], b"", b".", b"..", b"a/b", b"a\0b"] {
    let tree = tree_with(&mut store, vec![entry(name, EntryKind::File, blob)]);
    assert_eq!(
      refusal(&store, tree),
      PushRefused::NameNotHeld {
        path: name.to_vec()
      },
      "{name:?}"
    );
  }
  let inner = tree_with(&mut store, vec![entry(b"\xc3\x28", EntryKind::File, blob)]);
  let tree = tree_with(&mut store, vec![entry(b"dir", EntryKind::Directory, inner)]);
  assert_eq!(
    refusal(&store, tree),
    PushRefused::NameNotHeld {
      path: b"dir/\xc3\x28".to_vec()
    }
  );
  let twice = tree_with(
    &mut store,
    vec![
      entry(b"same", EntryKind::File, blob),
      entry(b"same", EntryKind::Symlink, blob),
    ],
  );
  assert_eq!(
    refusal(&store, twice),
    PushRefused::DuplicateName {
      path: "same".to_owned()
    }
  );
  for target in [&b"\xff\xfe"[..], b"a\0b"] {
    let link = store.put_blob(target);
    let tree = tree_with(&mut store, vec![entry(b"link", EntryKind::Symlink, link)]);
    assert_eq!(
      refusal(&store, tree),
      PushRefused::TargetNotHeld {
        path: "link".to_owned()
      }
    );
  }
}

/// A-618 (§4.17.2 item 5: a submodule's special node kind is owed; GAPS §8f: the deriver's composition of a path that
/// changes kind is owed). Do: push a tree with a submodule link; then, over a landed tree, a file becoming a directory,
/// a directory becoming a link, and a link becoming a file. Expect: `Submodule`, then `KindChanged` naming each path;
/// the green unchanged.
#[test]
fn a_submodule_and_a_path_changing_kind_are_refused_until_their_compositions_exist() {
  let mut store = Store::default();
  let commit = key(b"commit", b"another repository's");
  let tree = tree_with(
    &mut store,
    vec![entry(b"sub", EntryKind::Submodule, commit)],
  );
  assert_eq!(
    refusal(&store, tree),
    PushRefused::Submodule {
      path: "sub".to_owned()
    }
  );
  let mut green = Green::new();
  let base_files = files_of(&[
    ("f", file(b"f\n")),
    ("d/x", file(b"x\n")),
    ("l", Node::Link("f".to_owned())),
  ]);
  let (_, _, base_tree) = land(&mut store, &mut green, None, &base_files, &head(1));
  let changes: [(&str, Files); 3] = [
    (
      "f",
      files_of(&[
        ("f/inner", file(b"f\n")),
        ("d/x", file(b"x\n")),
        ("l", Node::Link("f".to_owned())),
      ]),
    ),
    (
      "d",
      files_of(&[
        ("f", file(b"f\n")),
        ("d", Node::Link("f".to_owned())),
        ("l", Node::Link("f".to_owned())),
      ]),
    ),
    (
      "l",
      files_of(&[
        ("f", file(b"f\n")),
        ("d/x", file(b"x\n")),
        ("l", file(b"now a file\n")),
      ]),
    ),
  ];
  for (path, files) in changes {
    let tree = put(&mut store, &files);
    assert_eq!(
      build(
        &push(1, Some(base_tree), tree, &head(2)),
        &green,
        &store,
        u64::MAX
      ),
      Err(PushRefused::KindChanged {
        path: path.to_owned()
      })
    );
  }
  holds_exactly(&green, &base_files, &head(1));
}

/// A-618 (§4.9; the work charged as it is made). Do: push a tree holding a file of a mebibyte under a room of half a
/// mebibyte; a base version past the green's head; a store whose root tree names itself; a work naming more names than
/// one ops document holds. Expect: `PastRoom` before the file's bytes are read (the store gave none), `UnknownBase`,
/// `PastRoom` for the endless tree under a room of a kibibyte, and `TooManyNames`.
#[test]
fn a_push_past_its_room_its_green_or_a_document_is_refused_before_it_is_held() {
  let mut store = Store::default();
  let large = store.put_blob(&vec![b'x'; 1 << 20]);
  let tree = tree_with(&mut store, vec![entry(b"large", EntryKind::File, large)]);
  let room = 1 << 19;
  let refused = build(&push(0, None, tree, &head(1)), &Green::new(), &store, room).unwrap_err();
  assert!(
    matches!(refused, PushRefused::PastRoom { asked, room: given } if asked > room && given == room),
    "{refused:?}"
  );
  assert_eq!(store.blobs_given.get(), 0, "no byte of the file was read");
  assert_eq!(
    build(
      &push(5, None, tree, &head(1)),
      &Green::new(),
      &store,
      u64::MAX
    ),
    Err(PushRefused::UnknownBase { base: 5, head: 0 })
  );
  let endless = key(b"tree", b"itself");
  store.objects.insert(
    endless,
    Object::Tree(vec![entry(b"again", EntryKind::Directory, endless)]),
  );
  assert!(matches!(
    build(
      &push(0, None, endless, &head(1)),
      &Green::new(),
      &store,
      1 << 10
    ),
    Err(PushRefused::PastRoom { .. })
  ));
  let empty = store.put_blob(b"");
  let many: Vec<TreeEntry> = (0..u16::MAX as usize)
    .map(|n| entry(format!("f{n}").as_bytes(), EntryKind::File, empty))
    .collect();
  let wide = tree_with(&mut store, many);
  assert_eq!(
    build(
      &push(0, None, wide, &head(1)),
      &Green::new(),
      &store,
      u64::MAX
    ),
    Err(PushRefused::TooManyNames {
      names: u16::MAX as usize + 2,
      most: u16::MAX as usize
    })
  );
}

/// A-618. Do: push, over a landed tree, a tree that removes a file the base tree names and the green does not hold (a
/// base the green disagrees with). Expect: the deriver's refusal, typed (`UnlinkMissing`), and nothing landed.
#[test]
fn a_base_the_green_disagrees_with_is_refused_by_the_deriver() {
  let mut store = Store::default();
  let mut green = Green::new();
  let files = files_of(&[("kept", file(b"kept\n"))]);
  let (_, _, _) = land(&mut store, &mut green, None, &files, &head(1));
  let claimed = put(
    &mut store,
    &files_of(&[
      ("kept", file(b"kept\n")),
      ("ghost", file(b"never landed\n")),
    ]),
  );
  let tree = put(&mut store, &files);
  assert_eq!(
    build(
      &push(1, Some(claimed), tree, &head(2)),
      &green,
      &store,
      u64::MAX
    ),
    Err(PushRefused::Compose(DeriveError::UnlinkMissing(
      "ghost".to_owned()
    )))
  );
  holds_exactly(&green, &files, &head(1));
}

/// Format: the paths generated histories write: deep and shallow, beside one another, so a push changes some subtrees and
/// leaves others.
const SLOTS: [&str; 12] = [
  "f", "g", "l", "a/f", "a/g", "a/b/f", "a/b/l", "c/f", "c/d/f", "c/d/e/f", "c/d/e/g", "h",
];

/// Format: the bytes a generated file holds, one of them none.
const CONTENTS: [&[u8]; 4] = [b"one\n", b"two\n", b"", b"a longer line of bytes\n"];

/// Format: the targets a generated link holds.
const TARGETS: [&str; 2] = ["f", "../c/f"];

/// What one slot becomes at a step: removed (0), a file of [`CONTENTS`] (1 to 4, with its executable bit), or a link to
/// a target of [`TARGETS`] (5 and 6).
fn apply(files: &Files, step: &[Option<(u8, bool)>]) -> Files {
  let mut next = files.clone();
  for (slot, change) in SLOTS.iter().zip(step) {
    let Some((choice, executable)) = change else {
      continue;
    };
    let path = (*slot).to_owned();
    match choice {
      0 => {
        next.remove(&path);
      }
      1..=4 => {
        next.insert(
          path,
          Node::File(CONTENTS[usize::from(*choice) - 1].to_vec(), *executable),
        );
      }
      _ => {
        next.insert(
          path,
          Node::Link(TARGETS[usize::from(*choice) - 5].to_owned()),
        );
      }
    }
  }
  next
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-618 (§4.17.1 items 2 to 4; the oracle and the client-fidelity test). Do: run histories of up to eight pushes,
  /// each changing, adding or removing any of twelve paths, deep and shallow, as files of four contents (one empty) with
  /// or without the executable bit, or as links of two targets; each pushed over the last landed tree. Expect: each
  /// build the oracle's work byte for byte, with exactly the reads a walk by identity makes; a path changing kind
  /// refused naming one of the paths that do, the green unchanged; every other push accepted, leaving the green holding
  /// exactly what was pushed, and its head.
  #[test]
  fn every_push_is_the_oracles_work_and_leaves_the_client_where_it_is(
    steps in proptest::collection::vec(
      proptest::collection::vec(proptest::option::of((0u8..7, any::<bool>())), SLOTS.len()),
      1..8,
    )
  ) {
    let mut store = Store::default();
    let mut green = Green::new();
    let mut landed = Files::new();
    let mut landed_tree = None;
    for (n, step) in steps.iter().enumerate() {
      let files = apply(&landed, step);
      let tree = put(&mut store, &files);
      let head = head(n as u64 + 1);
      let push = push(green.head(), landed_tree, tree, &head);
      let built = build(&push, &green, &store, u64::MAX);
      match oracle(&store, &green, &push) {
        Expected::KindChanged(paths) => {
          let Err(PushRefused::KindChanged { path }) = built else {
            panic!("expected a kind change at one of {paths:?}, built {built:?}");
          };
          prop_assert!(paths.contains(&path), "{} is not one of {:?}", path, paths);
        }
        Expected::Work(increment, reads) => {
          let built = built.unwrap();
          prop_assert_eq!(&built.increment, &increment);
          prop_assert_eq!(built.reads, reads);
          let version = green.head() + 1;
          prop_assert_eq!(green.submit(&built.increment), Outcome::Accepted { version });
          holds_exactly(&green, &files, &head);
          landed = files;
          landed_tree = Some(tree);
        }
      }
    }
  }
}
