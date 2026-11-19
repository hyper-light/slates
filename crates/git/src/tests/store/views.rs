//! A-638's views of the store (`crate::store::views`): receive-pack's thin-pack bases and connectivity from the index
//! alone, and a push's trees and blobs above the index for A-618's work, each object lent as the store keeps it when it
//! is one window. The end-to-end tests drive receive-pack and A-618's build through the views over a real store.

use std::convert::Infallible;
use std::io::Write as _;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use slates_merge::push::{
  self as work, EntryKind, ObjectKey, PushObjects, Reads, TreeEntry, Unavailable,
};
use slates_merge::{Green, Outcome};
use slates_vcs::ContentStore;

use super::{IndexedObjects, PushedObjects, ViewCounters};
use crate::Admission;
use crate::delta::{self, Sizes};
use crate::entry::{self, Header, Representation};
use crate::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use crate::pack;
use crate::pktline::{self, Packet};
use crate::receive::Repository as _;
use crate::receive::{self, Advertised, Command, Declined, Judge, Pushed, Reported};
use crate::store::{Incoming, Push, Repository};

/// Shape: the store's window in these tests, short enough that a file of a few hundred bytes spans several windows and a
/// list of their identities.
const WINDOW: usize = 256;
/// Shape: a store budget no test reaches.
const BUDGET: u64 = 1 << 30;
/// Shape: the room a view may keep, and a push's admission, where the room is not what a test is about.
const ROOM: u64 = 1 << 30;
/// Shape: the green a branch's versions are numbered in.
const GREEN: u64 = 1;
/// Shape: a prime below 256, the period of [`patterned`]'s bytes, sharing no factor with the window.
const PERIOD: usize = 251;

/// What receive-pack advertises in these tests: a stock server's capabilities.
const ADVERTISED: Advertised<'static> = Advertised {
  format: ObjectFormat::Sha1,
  atomic: true,
  ofs_delta: true,
  push_options: false,
  session_id: None,
  agent: b"git/2.50.1",
};

/// An empty store of the tests' window, and an empty SHA-1 repository.
fn empty() -> (ContentStore, Repository) {
  (
    ContentStore::new(BUDGET, WINDOW).unwrap(),
    Repository::new(ObjectFormat::Sha1),
  )
}

/// Admits `content` as a `kind` to `push`, whole (no delta chain): its id.
fn admit(
  store: &mut ContentStore,
  repository: &Repository,
  push: &mut Push,
  kind: Kind,
  content: &[u8],
) -> ObjectId {
  let incoming = Incoming {
    kind,
    claimed: None,
    content,
    depth: 0,
  };
  let naming = repository.naming();
  push
    .admit::<_, Infallible>(store, &naming, incoming)
    .unwrap()
    .0
}

/// A tree of `entries`, each its mode as written, its name and its object, in the order given (git's, in every test).
fn tree_bytes(entries: &[(&[u8], &[u8], ObjectId)]) -> Vec<u8> {
  let mut out = Vec::new();
  for (mode, name, id) in entries {
    out.extend_from_slice(mode);
    out.push(b' ');
    out.extend_from_slice(name);
    out.push(0);
    out.extend_from_slice(id.as_bytes());
  }
  out
}

/// A commit of `tree` on `parents`.
fn commit_bytes(tree: ObjectId, parents: &[ObjectId]) -> Vec<u8> {
  let mut text = format!("tree {tree}\n");
  for parent in parents {
    text.push_str(&format!("parent {parent}\n"));
  }
  text.push_str("author A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\nm\n");
  text.into_bytes()
}

/// Commits `push` as [`GREEN`]'s move to `head`, as receive-pack does once the verdict accepts it.
fn commit(store: &mut ContentStore, repository: &mut Repository, mut push: Push, head: ObjectId) {
  repository.seal(store, &mut push).unwrap();
  let moved = repository.admit(store, &push, GREEN, Some(head)).unwrap();
  repository.commit(store, &mut push, &[moved]).unwrap();
}

/// Commits a tree of the blobs `files` (each a name in git's order and its bytes) as [`GREEN`]'s next version: each
/// blob's id, then the commit's.
fn commit_files(
  store: &mut ContentStore,
  repository: &mut Repository,
  files: &[(&[u8], &[u8])],
) -> (Vec<ObjectId>, ObjectId) {
  let mut push = Push::new(ObjectFormat::Sha1);
  let mut entries = Vec::new();
  for (name, bytes) in files {
    let id = admit(store, repository, &mut push, Kind::Blob, bytes);
    entries.push((&b"100644"[..], *name, id));
  }
  let ids = entries.iter().map(|(_, _, id)| *id).collect();
  let tree = admit(
    store,
    repository,
    &mut push,
    Kind::Tree,
    &tree_bytes(&entries),
  );
  let head = admit(
    store,
    repository,
    &mut push,
    Kind::Commit,
    &commit_bytes(tree, &[]),
  );
  commit(store, repository, push, head);
  (ids, head)
}

/// `count` bytes repeating every [`PERIOD`] bytes, so no two neighbouring windows are alike.
fn patterned(count: usize) -> Vec<u8> {
  (0..count)
    .map(|at| u8::try_from(at % PERIOD).unwrap())
    .collect()
}

/// `n` as a `u64`.
fn wide(n: usize) -> u64 {
  u64::try_from(n).unwrap()
}

/// The key A-618 reads `id` by.
fn key(id: ObjectId) -> ObjectKey {
  ObjectKey::new(id.as_bytes()).unwrap()
}

/// A delta from `source` to `target`: a copy of their common start, then the rest of the target inserted.
fn delta_between(source: &[u8], target: &[u8]) -> Vec<u8> {
  let common = source
    .iter()
    .zip(target)
    .take_while(|(a, b)| a == b)
    .count();
  let mut out = Vec::new();
  let sizes = Sizes {
    source: wide(source.len()),
    target: wide(target.len()),
  };
  delta::write_sizes(sizes, &mut out);
  delta::write_copy(0, wide(common), &mut out).unwrap();
  delta::write_insert(target.get(common..).unwrap(), &mut out);
  out
}

/// A pack of `entries`, each deflated, with git's header and trailer.
fn pack_of(entries: &[(Representation, Vec<u8>)]) -> Vec<u8> {
  let mut bytes = pack::SIGNATURE.to_vec();
  bytes.extend_from_slice(&2_u32.to_be_bytes());
  bytes.extend_from_slice(&u32::try_from(entries.len()).unwrap().to_be_bytes());
  for (representation, data) in entries {
    let header = Header {
      representation: *representation,
      size: wide(data.len()),
    };
    entry::write(header, wide(bytes.len()), &mut bytes).unwrap();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    bytes.extend_from_slice(&encoder.finish().unwrap());
  }
  let checksum = pack::checksum_of(ObjectFormat::Sha1, &bytes).unwrap();
  bytes.extend_from_slice(checksum.as_bytes());
  bytes
}

/// A client's push request of the one command moving `name` from `old` to `new`, then `pack`.
fn request(old: ObjectId, new: ObjectId, name: &[u8], pack: &[u8]) -> Vec<u8> {
  let mut line = format!("{old} {new} ").into_bytes();
  line.extend_from_slice(name);
  line.push(0);
  line.extend_from_slice(b"report-status");
  let mut out = Vec::new();
  pktline::write(Packet::Data(&line), &mut out).unwrap();
  pktline::write(Packet::Flush, &mut out).unwrap();
  out.extend_from_slice(pack);
  out
}

/// A judge that applies every command.
struct Applies;

impl Judge for Applies {
  fn judge(&mut self, _command: &Command, _pushed: &Pushed<'_>) -> Result<Vec<Reported>, Declined> {
    Ok(Vec::new())
  }
}

/// A-638 (§4.17.2 "A push" item 3: a thin pack's bases from the repository's index). Do: commit a file of one window,
/// hold another blob in a push's quarantine, and ask receive-pack's view for each, and whether it holds each and the
/// committed commit. Expect: the committed blob lent with its type and exact bytes as the store keeps it, the held-back
/// one neither lent nor held (it has passed no check), and `holds` answering from the row alone, lending nothing.
#[test]
fn a_base_is_lent_from_the_index_alone_as_the_store_keeps_it() {
  let (mut store, mut repository) = empty();
  let small: &[u8] = b"one window of bytes\n";
  let (blobs, head) = commit_files(&mut store, &mut repository, &[(b"small", small)]);
  let blob = blobs[0];
  let mut quarantine = Push::new(ObjectFormat::Sha1);
  let held_back = admit(
    &mut store,
    &repository,
    &mut quarantine,
    Kind::Blob,
    b"not yet checked\n",
  );
  let view = IndexedObjects::new(&repository, &store, ROOM);
  assert!(
    view.holds(&head) && view.holds(&blob),
    "the committed objects"
  );
  assert_eq!(
    view.counters(),
    ViewCounters::default(),
    "holds lends nothing"
  );
  assert_eq!(view.object(&blob), Some((Kind::Blob, small)));
  assert_eq!(
    view.counters().lent_as_kept,
    1,
    "lent as the store keeps it"
  );
  assert_eq!(
    view.object(&held_back),
    None,
    "a quarantined object is no base"
  );
  assert!(!view.holds(&held_back), "nor does it end a walk");
  assert_eq!(view.refused(), None);
  quarantine.abandon(&mut store).unwrap();
}

/// A-638 (module docs: a base longer than a window). Do: commit a file four windows long and more, and ask the view
/// for it twice. Expect: its type and exact bytes both times, assembled once and lent again from what the view kept.
#[test]
fn a_base_longer_than_a_window_is_assembled_once_and_lent_again() {
  let (mut store, mut repository) = empty();
  let long = patterned(WINDOW * 4 + 17);
  let (blobs, _) = commit_files(&mut store, &mut repository, &[(b"long", &long)]);
  let view = IndexedObjects::new(&repository, &store, ROOM);
  let lent = Some((Kind::Blob, long.as_slice()));
  assert_eq!(view.object(&blobs[0]), lent);
  assert_eq!(view.object(&blobs[0]), lent);
  let counters = ViewCounters {
    lent_as_kept: 0,
    assembled: 1,
    lent_again: 1,
  };
  assert_eq!(view.counters(), counters);
}

/// A-638 (module docs: what a view keeps is charged first). Do: ask a view whose room is less than a long base for it,
/// then for a base of one window. Expect: the long one answered as not held and the refusal named, with the room the
/// view had; the short one still lent as the store keeps it, which keeps nothing.
#[test]
fn a_base_past_the_views_room_is_answered_absent_and_its_refusal_named() {
  let (mut store, mut repository) = empty();
  let long = patterned(WINDOW * 4);
  let short: &[u8] = b"short\n";
  let files: [(&[u8], &[u8]); 2] = [(b"long", &long), (b"short", short)];
  let (blobs, _) = commit_files(&mut store, &mut repository, &files);
  let room = wide(long.len()).saturating_sub(1);
  let view = IndexedObjects::new(&repository, &store, room);
  assert_eq!(view.object(&blobs[0]), None, "past the room");
  let refused = view.refused().unwrap();
  assert_eq!(refused.left, room);
  assert!(refused.asked > room, "asked {}", refused.asked);
  assert_eq!(view.object(&blobs[1]), Some((Kind::Blob, short)));
  assert_eq!(view.counters().lent_as_kept, 1);
  assert_eq!(view.counters().assembled, 0);
}

/// A-638 (§4.17.2 "A push" item 3 and piece 6: receive-pack over the store). Do: commit a long file, then receive a
/// push of a commit on it whose pack is thin: two reference deltas on the long blob only the store holds (two edits of
/// it), and the tree and commit whole. Expect: the command applied, both edited blobs made with their exact bytes, the
/// base assembled once and lent again, and the parent commit held without being lent.
#[test]
fn receive_pack_resolves_a_thin_pack_against_the_store() {
  let (mut store, mut repository) = empty();
  let base = patterned(WINDOW * 3 + 5);
  let (blobs, parent) = commit_files(&mut store, &mut repository, &[(b"long", &base)]);
  let edits: Vec<Vec<u8>> = [&b"first edit\n"[..], b"second edit\n"]
    .iter()
    .map(|edit| [base.as_slice(), *edit].concat())
    .collect();
  let ids: Vec<ObjectId> = edits
    .iter()
    .map(|bytes| name_of(ObjectFormat::Sha1, Kind::Blob, bytes).unwrap())
    .collect();
  let tree = tree_bytes(&[
    (b"100644", b"first", ids[0]),
    (b"100644", b"second", ids[1]),
  ]);
  let tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &tree).unwrap();
  let commit = commit_bytes(tree_id, &[parent]);
  let commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
  let on_base = Representation::RefDelta { base: blobs[0] };
  let pack = pack_of(&[
    (on_base, delta_between(&base, &edits[0])),
    (on_base, delta_between(&base, &edits[1])),
    (Representation::Whole(Kind::Tree), tree),
    (Representation::Whole(Kind::Commit), commit),
  ]);
  let bytes = request(parent, commit_id, b"refs/heads/main", &pack);
  let view = IndexedObjects::new(&repository, &store, ROOM);
  let mut admission = Admission::new(ROOM);
  let received =
    receive::receive(&bytes, &ADVERTISED, &view, &mut Applies, &mut admission).unwrap();
  assert_eq!(received.outcomes, [receive::Outcome::Ok(Vec::new())]);
  let unpacked = received.unpacked.unwrap().unwrap();
  for (id, edit) in ids.iter().zip(&edits) {
    let made = unpacked.objects.iter().find(|object| object.id == *id);
    assert_eq!(
      made.map(|object| object.content.as_slice()),
      Some(edit.as_slice())
    );
  }
  let counters = ViewCounters {
    lent_as_kept: 0,
    assembled: 1,
    lent_again: 1,
  };
  assert_eq!(view.counters(), counters);
}

/// The entries the tree of every mode below must read as: each name, its kind in a work, and its object.
fn expected(entries: &[(&[u8], EntryKind, ObjectId)]) -> Vec<TreeEntry> {
  entries
    .iter()
    .map(|(name, kind, id)| TreeEntry {
      name: name.to_vec(),
      kind: *kind,
      id: key(*id),
    })
    .collect()
}

/// A-638 (§4.17.1 "A push is a work": a pushed tree read by id; git's tree walk's modes). Do: commit a tree, then hold
/// in a push's quarantine a tree whose entries carry the modes stock git 2.50.1 was asked to read (100600, 100664,
/// 100700, 100775, a typeless 644, 100644, 100755) beside a symbolic link and the committed tree as a subtree, and read
/// both trees and a blob through the push's view. Expect: each entry's kind as git read its mode (`git ls-tree`, and
/// `git ls-files -s` after `git read-tree`, 2026-11-19: 100644, 100644, 100755, 100755, 160000, 100644, 100755), the
/// committed tree read from the index and the pushed one from the push, and the blob's length and exact bytes; the
/// pushed tree, longer than a window, assembled, and the committed tree and both blobs lent as kept.
#[test]
fn a_pushed_tree_reads_its_modes_as_gits_tree_walk_does() {
  let (mut store, mut repository) = empty();
  let inner: &[u8] = b"inner\n";
  let (blobs, _) = commit_files(&mut store, &mut repository, &[(b"inner", inner)]);
  let inner_tree = tree_bytes(&[(b"100644", b"inner", blobs[0])]);
  let committed_tree = name_of(ObjectFormat::Sha1, Kind::Tree, &inner_tree).unwrap();
  let mut push = Push::new(ObjectFormat::Sha1);
  let blob = admit(&mut store, &repository, &mut push, Kind::Blob, b"x\n");
  let link = admit(&mut store, &repository, &mut push, Kind::Blob, b"target");
  let modes: [(&[u8], &[u8], ObjectId); 9] = [
    (b"100600", b"a-600", blob),
    (b"100664", b"b-664", blob),
    (b"100700", b"c-700", blob),
    (b"100775", b"d-775", blob),
    (b"644", b"e-644-no-type", blob),
    (b"100644", b"f-644", blob),
    (b"100755", b"g-755", blob),
    (b"120000", b"h-link", link),
    (b"40000", b"i-dir", committed_tree),
  ];
  let pushed = admit(
    &mut store,
    &repository,
    &mut push,
    Kind::Tree,
    &tree_bytes(&modes),
  );
  let view = PushedObjects::new(&repository, &store, &push);
  let kinds = [
    EntryKind::File,
    EntryKind::File,
    EntryKind::Executable,
    EntryKind::Executable,
    EntryKind::Submodule,
    EntryKind::File,
    EntryKind::Executable,
    EntryKind::Symlink,
    EntryKind::Directory,
  ];
  let wanted: Vec<(&[u8], EntryKind, ObjectId)> = modes
    .iter()
    .zip(kinds)
    .map(|((_, name, id), kind)| (*name, kind, *id))
    .collect();
  assert_eq!(
    view.tree(key(pushed)),
    Ok(expected(&wanted)),
    "the pushed tree"
  );
  let inner_entry = [(&b"inner"[..], EntryKind::File, blobs[0])];
  assert_eq!(
    view.tree(key(committed_tree)),
    Ok(expected(&inner_entry)),
    "the committed tree"
  );
  assert_eq!(view.blob_len(key(blobs[0])), Ok(wide(inner.len())));
  assert_eq!(view.blob(key(blobs[0])).as_deref(), Ok(inner));
  assert_eq!(view.blob(key(link)).as_deref(), Ok(&b"target"[..]));
  let counters = ViewCounters {
    lent_as_kept: 3,
    assembled: 1,
    lent_again: 0,
  };
  assert_eq!(
    view.counters(),
    counters,
    "the pushed tree spans two windows"
  );
  drop(view);
  push.abandon(&mut store).unwrap();
}

/// A-638 (A-618's typed refusals). Do: ask the push's view for a blob as a tree, a tree as a blob (its length and its
/// bytes), an id nothing holds, a key of SHA-256's length in a SHA-1 repository, and a pushed tree whose bytes git's
/// tree walk cannot read. Expect: `NotATree`, `NotABlob` twice, `Missing` twice, and `Malformed`.
#[test]
fn objects_the_view_cannot_give_are_refused_by_kind() {
  let (mut store, repository) = empty();
  let mut push = Push::new(ObjectFormat::Sha1);
  let blob = admit(&mut store, &repository, &mut push, Kind::Blob, b"x\n");
  let tree = admit(
    &mut store,
    &repository,
    &mut push,
    Kind::Tree,
    &tree_bytes(&[(b"100644", b"x", blob)]),
  );
  let unreadable = admit(
    &mut store,
    &repository,
    &mut push,
    Kind::Tree,
    b"100644 no-terminator",
  );
  let nothing = name_of(ObjectFormat::Sha1, Kind::Blob, b"held by no one\n").unwrap();
  let view = PushedObjects::new(&repository, &store, &push);
  assert_eq!(view.tree(key(blob)), Err(Unavailable::NotATree));
  assert_eq!(view.blob_len(key(tree)), Err(Unavailable::NotABlob));
  assert_eq!(view.blob(key(tree)), Err(Unavailable::NotABlob));
  assert_eq!(view.blob(key(nothing)), Err(Unavailable::Missing));
  let wide_key = ObjectKey::new(&[7_u8; 32]).unwrap();
  assert_eq!(view.blob(wide_key), Err(Unavailable::Missing));
  assert_eq!(view.tree(key(unreadable)), Err(Unavailable::Malformed));
  drop(view);
  push.abandon(&mut store).unwrap();
}

/// What [`push_branch`] admitted: the ids a test reads the push by.
struct BranchPush {
  /// The root tree.
  root: ObjectId,
  /// The commit.
  head: ObjectId,
  /// The `docs` tree.
  docs: ObjectId,
}

/// Admits a branch's tree to `push`: `lib` as `src/lib.rs`, fixed `src/main.rs` and `run.sh`, and `guide` as
/// `docs/guide.md`, its commit on `parents`.
fn push_branch(
  store: &mut ContentStore,
  repository: &Repository,
  push: &mut Push,
  lib: &[u8],
  guide: &[u8],
  parents: &[ObjectId],
) -> BranchPush {
  let mut put = |kind, bytes: &[u8]| admit(store, repository, push, kind, bytes);
  let lib_id = put(Kind::Blob, lib);
  let main_id = put(Kind::Blob, b"fn main() {}\n");
  let run_id = put(Kind::Blob, b"#!/bin/sh\nexec ./main\n");
  let guide_id = put(Kind::Blob, guide);
  let src = put(
    Kind::Tree,
    &tree_bytes(&[
      (b"100644", b"lib.rs", lib_id),
      (b"100644", b"main.rs", main_id),
    ]),
  );
  let docs = put(
    Kind::Tree,
    &tree_bytes(&[(b"100644", b"guide.md", guide_id)]),
  );
  let root = put(
    Kind::Tree,
    &tree_bytes(&[
      (b"40000", b"docs", docs),
      (b"100755", b"run.sh", run_id),
      (b"40000", b"src", src),
    ]),
  );
  let head = put(Kind::Commit, &commit_bytes(root, parents));
  BranchPush { root, head, docs }
}

/// A-618's push of `tree` over `green`'s head, whose tree is `base_tree`, moving the head to `head`.
fn work_push<'a>(
  green: &Green,
  base_tree: Option<ObjectId>,
  tree: ObjectId,
  head: &'a ObjectId,
) -> work::Push<'a> {
  work::Push {
    name: *b"A-638 push, read through a store",
    base_version: green.head(),
    base_tree: base_tree.map(key),
    tree: key(tree),
    head: Some(head.as_bytes()),
  }
}

/// A-638 with A-618 (§4.17.1 items 2 to 4: a client's push becomes a work, read through the store). Do: push a first
/// tree to an empty green through the store's view (two directories, an executable, a file longer than a window) and
/// commit it to the repository, then push a second tree that changes one file in `src` and names `docs` by its
/// committed id. Expect: both accepted, as versions 1 and 2; the green holding the client's bytes and modes after
/// each; the first build reading every tree and blob once, through both of the view's paths; and the second reading
/// both sides of the root and `src`, the one changed blob, and `docs` not at all.
#[test]
fn a_push_read_through_the_store_becomes_the_work_holding_the_clients_bytes() {
  let (mut store, mut repository) = empty();
  let mut green = Green::new();
  let guide = patterned(WINDOW * 2 + 9);
  let mut first = Push::new(ObjectFormat::Sha1);
  let one = push_branch(
    &mut store,
    &repository,
    &mut first,
    b"pub fn one() {}\n",
    &guide,
    &[],
  );
  let view = PushedObjects::new(&repository, &store, &first);
  let built = work::build(
    &work_push(&green, None, one.root, &one.head),
    &green,
    &view,
    u64::MAX,
  )
  .unwrap();
  let reads = Reads {
    trees: 3,
    blobs: 4,
    skipped: 0,
  };
  assert_eq!(built.reads, reads);
  assert!(
    view.counters().assembled >= 1 && view.counters().lent_as_kept >= 1,
    "{:?}",
    view.counters()
  );
  let outcome = green.submit(&built.increment);
  assert!(
    matches!(outcome, Outcome::Accepted { version: 1 }),
    "{outcome:?}"
  );
  assert_eq!(green.content("docs/guide.md"), Some(guide.as_slice()));
  assert_eq!(green.mode("run.sh"), Some(0o755));
  drop(view);
  commit(&mut store, &mut repository, first, one.head);
  let mut second = Push::new(ObjectFormat::Sha1);
  let two = push_branch(
    &mut store,
    &repository,
    &mut second,
    b"pub fn two() {}\n",
    &guide,
    &[one.head],
  );
  assert_eq!(two.docs, one.docs, "docs is named by its committed id");
  let view = PushedObjects::new(&repository, &store, &second);
  let push = work_push(&green, Some(one.root), two.root, &two.head);
  let built = work::build(&push, &green, &view, u64::MAX).unwrap();
  let reads = Reads {
    trees: 4,
    blobs: 1,
    skipped: 1,
  };
  assert_eq!(built.reads, reads);
  let outcome = green.submit(&built.increment);
  assert!(
    matches!(outcome, Outcome::Accepted { version: 2 }),
    "{outcome:?}"
  );
  assert_eq!(green.content("src/lib.rs"), Some(&b"pub fn two() {}\n"[..]));
  assert_eq!(green.content("src/main.rs"), Some(&b"fn main() {}\n"[..]));
  assert_eq!(green.content("docs/guide.md"), Some(guide.as_slice()));
  assert_eq!(green.mode("run.sh"), Some(0o755));
  drop(view);
  second.abandon(&mut store).unwrap();
}
