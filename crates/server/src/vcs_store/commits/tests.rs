//! A-664 part 3, its commit: each version a work submits to a git repository's green becomes a commit the server makes,
//! recorded with the version as a push's objects are and admitted to the repository's store, so a stock client's clone
//! fetches it: each test reads the commit as a client does, through A-682's lender driven by protocol v2's `ls-refs`
//! and `fetch`, its tree read from the fetched pack alone; and a store made again from the log holds every made commit
//! again.

use std::collections::BTreeMap;

use slates_db::catalog::RepositoryId;
use slates_git::Admission;
use slates_git::derived::TreeCounters;
use slates_git::object_name::{Kind, ObjectFormat, ObjectId};
use slates_git::objects::tree::{Mode, Tree};
use slates_git::unpack::{self, NoIndex};
use slates_git::upload::Asked;
use slates_ipc::protocol::WorkOp;
use slates_vfs::clock::{Clock, HostClock};

use super::{MADE, UNMADE};
use crate::state::ShardState;
use crate::vcs_store::green_key;
use crate::vcs_store::paths::tests::{
  Op, branch, count, create, git_repository, version, version_saying,
};
use crate::vcs_store::record::Record;
use crate::vcs_store::tests::{
  FIRST_DATE, assert_made_again_whole, commit_of, hex, owner, push_through, push_unsettled,
  records, tree_of,
};
use crate::vcs_store::upload::tests::{answered, fetch, lines_of, ls_refs, pack_of};
use crate::verbs::to_db_volume;

/// Shape: the room a clone's answers, pack and trees are read in: far past the tests' few small objects.
const ROOM: u64 = 1 << 20;
/// Shape: a file's mode with its owner's execute bit.
const EXECUTABLE: u32 = 0o755;
/// Format: the nanoseconds in a second.
const NANOS_PER_SECOND: i64 = 1_000_000_000;
/// Format: the repositories' object format, as `git_repository` makes them.
const FORMAT: ObjectFormat = ObjectFormat::Sha1;
/// Format: the git identity a submit's session holds in these tests (`crate::identities::authored`), as git writes it.
const IDENTITY: &str = "A U Thor <author@example.com>";

/// The files of a commit's tree as `git ls-tree -r` lists them: each path with its mode and its blob's bytes.
type Listed = BTreeMap<String, (Mode, Vec<u8>)>;

/// A fetched pack's objects by id: each one's type and content.
type Fetched = BTreeMap<ObjectId, (Kind, Vec<u8>)>;

/// A branch as a stock client clones it.
#[derive(Debug)]
struct Cloned {
  /// The commit `ls-refs` lists the branch at.
  head: ObjectId,
  /// The commit's text.
  text: String,
  /// Its tree's files, read from the fetched pack alone.
  files: Listed,
  /// The objects the fetch's pack held.
  fetched: usize,
}

/// What a stock client cloning `branch` of `repository` gets (module docs): `ls-refs` names the branch's commit, a
/// `fetch` of it with `done` answers a whole pack, and the commit and its tree are read from that pack.
fn cloned(state: &mut ShardState, repository: RepositoryId, branch: &str) -> Cloned {
  let listed = answered(state, (repository, &owner()), Asked::Request(&ls_refs()));
  assert_eq!(listed.1, None, "ls-refs refused");
  let name = format!("refs/heads/{branch}");
  let head = lines_of(&listed.0)
    .iter()
    .find_map(|line| listed_at(line, &name))
    .unwrap_or_else(|| panic!("{name} not listed: {:?}", lines_of(&listed.0)));
  let asked = fetch(&[head], &[]);
  let fetched = answered(state, (repository, &owner()), Asked::Request(&asked));
  assert_eq!(fetched.1, None, "fetch refused");
  let objects = objects_in(&pack_of(&fetched.0));
  let (kind, text) = objects.get(&head).cloned().expect("the commit fetched");
  assert_eq!(kind, Kind::Commit);
  let text = String::from_utf8(text).expect("a commit's text");
  let mut files = Listed::new();
  list(&objects, (tree_named(&text), ""), &mut files);
  Cloned {
    head,
    text,
    files,
    fetched: objects.len(),
  }
}

/// The commit an `ls-refs` line lists `name` at, if it lists `name`.
fn listed_at(line: &[u8], name: &str) -> Option<ObjectId> {
  let line = std::str::from_utf8(line).ok()?.trim_end();
  let mut words = line.split(' ');
  let id = words.next()?;
  if words.next()? != name {
    return None;
  }
  ObjectId::from_hex(FORMAT, id.as_bytes()).ok()
}

/// Every object of `pack`, read whole as git's index-pack reads one, by id.
fn objects_in(pack: &[u8]) -> Fetched {
  let unpacked =
    unpack::read_whole(pack, FORMAT, &NoIndex, &mut Admission::new(ROOM)).expect("a whole pack");
  unpacked
    .objects
    .into_iter()
    .map(|object| (object.id, (object.kind, object.content)))
    .collect()
}

/// Lists the tree `tree` beneath `prefix` into `files`, as `git ls-tree -r` does, from the fetched objects alone.
fn list(objects: &Fetched, (tree, prefix): (ObjectId, &str), files: &mut Listed) {
  let (kind, bytes) = objects.get(&tree).expect("the tree fetched");
  assert_eq!(*kind, Kind::Tree);
  let parsed = Tree::parse(bytes, FORMAT, &mut Admission::new(ROOM)).expect("a tree");
  for entry in parsed.entries() {
    let path = format!("{prefix}{}", String::from_utf8_lossy(entry.name));
    let id = entry.object_id(FORMAT).expect("an entry's id");
    if entry.is_dir() {
      list(objects, (id, &format!("{path}/")), files);
      continue;
    }
    let mode = entry.standard_mode().expect("a mode git writes");
    let (kind, blob) = objects.get(&id).expect("the blob fetched");
    assert_eq!(*kind, Kind::Blob);
    files.insert(path, (mode, blob.clone()));
  }
}

/// The tree a commit's text names.
fn tree_named(text: &str) -> ObjectId {
  let hex = text
    .lines()
    .find_map(|line| line.strip_prefix("tree "))
    .expect("a tree line");
  ObjectId::from_hex(FORMAT, hex.as_bytes()).expect("a tree id")
}

/// The parents a commit's text names, in order.
fn parents_of(text: &str) -> Vec<ObjectId> {
  text
    .lines()
    .filter_map(|line| line.strip_prefix("parent "))
    .map(|hex| ObjectId::from_hex(FORMAT, hex.as_bytes()).expect("a parent id"))
    .collect()
}

/// The author's date a commit's text names, in seconds.
fn date_of(text: &str) -> u64 {
  let author = text
    .lines()
    .find_map(|line| line.strip_prefix("author "))
    .expect("an author line");
  let mut words = author.rsplit(' ');
  assert_eq!(words.next(), Some("+0000"), "{author}");
  words
    .next()
    .and_then(|seconds| seconds.parse().ok())
    .expect("a date")
}

/// The text of the commit git's `commit-tree -m` writes of `tree` over `parents`, by the tests' identity at `seconds`
/// in UTC, with `message` completed by a newline.
fn made_text(tree: ObjectId, parents: &[ObjectId], seconds: u64, message: &str) -> String {
  let mut text = format!("tree {}\n", hex(tree));
  for parent in parents {
    text.push_str(&format!("parent {}\n", hex(*parent)));
  }
  text.push_str(&format!(
    "author {IDENTITY} {seconds} +0000\ncommitter {IDENTITY} {seconds} +0000\n\n{message}\n"
  ));
  text
}

/// Asserts `cloned` is the commit git writes of its tree over `parents`, dated within `dated`, saying `message`.
fn assert_made(cloned: &Cloned, parents: &[ObjectId], dated: (u64, u64), message: &str) {
  let seconds = date_of(&cloned.text);
  assert!(
    (dated.0..=dated.1).contains(&seconds),
    "dated {seconds}, outside {dated:?}"
  );
  let tree = tree_named(&cloned.text);
  assert_eq!(cloned.text, made_text(tree, parents, seconds, message));
}

/// The host's wall clock now, in whole seconds since the epoch.
fn wall_seconds() -> u64 {
  u64::try_from(HostClock::new().wall_ns().div_euclid(NANOS_PER_SECOND)).expect("after the epoch")
}

/// A work's `mkdir` of `path`.
fn mkdir(path: &str) -> Op {
  Op::Declare(WorkOp::Mkdir {
    path: path.to_owned(),
  })
}

/// A work's `chmod 0755` of `path`.
fn executable(path: &str) -> Op {
  Op::Declare(WorkOp::SetMode {
    path: path.to_owned(),
    mode: EXECUTABLE,
  })
}

/// `files` as a listing: each path with its mode and bytes.
fn listing(files: &[(&str, Mode, &[u8])]) -> Listed {
  files
    .iter()
    .map(|(path, mode, bytes)| ((*path).to_owned(), (*mode, bytes.to_vec())))
    .collect()
}

/// The made commits counted, and the versions whose commit was not made.
fn counts(state: &ShardState) -> (u64, u64) {
  (count(state, MADE), count(state, UNMADE))
}

/// A-664 part 3 (its commit; §4.17.1 "A push is a work" items 3 and 4). Do: in a work on a git repository's root green
/// (`main`), make `d/a` and `f` and make `d/a` executable; submit it with a message, as a session holding a git identity;
/// then clone `main` as a stock client does. Expect: `main` listed at a commit the server made, with no parent; its tree,
/// read from the fetched pack alone, holding `d/a` `100755` and `f` `100644` with their bytes; its author and committer
/// the session's identity dated at the submit's instant, in UTC; its message the submit's, completed with a newline as
/// `git commit-tree -m` completes it; the pack holding exactly the commit, its two trees and its two blobs; one commit
/// made and none refused. Red before the commit was made: `main` was listed at nothing.
#[test]
fn t_a664_p3_a_submitted_version_is_a_commit_a_clone_fetches() {
  let (clone, made, dated) = crate::daemon::audit_on_shard(|state| {
    let (repository, green) = git_repository(state, "made");
    let before = wall_seconds();
    let ops = vec![
      mkdir("/d"),
      create("/d/a", b"alpha"),
      create("/f", b"hello"),
      executable("/d/a"),
    ];
    version(state, green, 1, ops);
    let after = wall_seconds();
    (
      cloned(state, repository, "main"),
      counts(state),
      (before, after),
    )
  });
  let expected = listing(&[
    ("d/a", Mode::Executable, b"alpha"),
    ("f", Mode::File, b"hello"),
  ]);
  assert_eq!(clone.files, expected);
  assert_made(&clone, &[], dated, "version 1");
  assert_eq!(clone.fetched, 5);
  assert_eq!(made, (1, 0));
}

/// The trees the store keeps for the head of the green keyed `key`: its root tree's id.
fn kept_tree(state: &ShardState, repository: RepositoryId, key: u64) -> Option<ObjectId> {
  let (content, git) = state.vcs_stores.git_ref(repository)?;
  let head = git.heads.get(&key)?;
  git.trees.as_ref()?.id(content, head.trees).ok()
}

/// What the repository's trees have made.
fn tree_counters(state: &ShardState, repository: RepositoryId) -> TreeCounters {
  state
    .vcs_stores
    .git_ref(repository)
    .and_then(|(_, git)| git.trees.as_ref().map(|trees| trees.counters()))
    .expect("the repository's trees")
}

/// Each record of `repository`'s register past the first `skip`: its moves, and its objects' types in order.
fn recorded(state: &ShardState, repository: RepositoryId, skip: usize) -> Vec<RecordFacts> {
  records(state, repository)
    .iter()
    .skip(skip)
    .map(|bytes| {
      let record = Record::decode(bytes, FORMAT).expect("a record");
      let kinds = record.names.iter().map(|name| name.0).collect();
      (record.moves, kinds)
    })
    .collect()
}

/// A record's moves, and its objects' types in order.
type RecordFacts = (Vec<(u64, u64, Option<ObjectId>)>, Vec<Kind>);

/// A-664 part 3 (its commit; A-662: a made commit recorded as a push's objects are, its store made again from the log).
/// Do: push a commit of `README` to a git repository's root green through A-635's append; submit two versions through
/// works, one rewriting `README` and one making `src/lib.rs`; then make the store again from the log as a recovery
/// does. Expect: the pushed version's git trees are the pushed commit's tree; the first made commit names the pushed one
/// as its parent and the second names the first; each made commit's record holds its one move (the green's key, the
/// version, the commit) and exactly what the repository lacked, in order (the first: its root tree, `README`'s new
/// blob, itself; the second: its root tree, `src`'s, `lib.rs`'s blob, itself), the second's trees derived over its
/// change alone (two directories made, one blob hashed); two commits made, none refused; the store made again equal
/// whole, every record replayed; `main` fetched at the second made commit after it.
#[test]
fn t_a664_p3_made_commits_name_the_one_before_and_are_made_again_from_the_log() {
  crate::daemon::audit_on_shard(|state| {
    let (repository, green) = git_repository(state, "history");
    let root = to_db_volume(green);
    let key = green_key(root);
    let (objects, pushed) = commit_of(&[(b"README".to_vec(), b"one\n".to_vec())], &[], FIRST_DATE);
    let pushed_tree = tree_of(&objects);
    push_through(
      state,
      (repository, root),
      &objects,
      (None, pushed, pushed_tree),
    );
    assert_eq!(kept_tree(state, repository, key), Some(pushed_tree));
    version(state, green, 2, vec![Op::Edit("/README", 0, 4, b"two\n")]);
    let second = cloned(state, repository, "main");
    let before = tree_counters(state, repository);
    let ops = vec![mkdir("/src"), create("/src/lib.rs", b"fn main() {}\n")];
    version(state, green, 3, ops);
    let after = tree_counters(state, repository);
    let third = cloned(state, repository, "main");
    assert_eq!(parents_of(&second.text), vec![pushed]);
    assert_eq!(parents_of(&third.text), vec![second.head]);
    let expected: Vec<RecordFacts> = vec![
      (
        vec![(key, 2, Some(second.head))],
        vec![Kind::Tree, Kind::Blob, Kind::Commit],
      ),
      (
        vec![(key, 3, Some(third.head))],
        vec![Kind::Tree, Kind::Tree, Kind::Blob, Kind::Commit],
      ),
    ];
    assert_eq!(recorded(state, repository, 1), expected);
    let derived = (
      after.directories - before.directories,
      after.blobs - before.blobs,
    );
    assert_eq!(derived, (2, 1));
    assert_eq!(counts(state), (2, 0));
    assert_made_again_whole(state, repository, &[key]);
    let again = cloned(state, repository, "main");
    assert_eq!((again.head, again.files), (third.head, third.files));
  });
}

/// A-664 part 3 (its commit; module docs, "Its parent": a branch `CreateBranch` made holds no commit of its own). Do:
/// submit `f` to a git repository's root green (`main`); make branch `topic` at its head; submit `t` to `topic`; clone
/// both. Expect: `topic` at a commit the server made whose parent is `main`'s commit, its tree holding `f` and `t`.
#[test]
fn t_a664_p3_a_branchs_first_made_commit_names_the_commit_it_was_made_at() {
  let (main, topic) = crate::daemon::audit_on_shard(|state| {
    let (repository, green) = git_repository(state, "branched");
    version(state, green, 1, vec![create("/f", b"main")]);
    let topic = branch(state, "branched", "topic");
    version(state, topic, 2, vec![create("/t", b"topic")]);
    (
      cloned(state, repository, "main"),
      cloned(state, repository, "topic"),
    )
  });
  assert_eq!(parents_of(&topic.text), vec![main.head]);
  let expected = listing(&[("f", Mode::File, b"main"), ("t", Mode::File, b"topic")]);
  assert_eq!(topic.files, expected);
}

/// A-664 part 3 (its commit; module docs, "Not made, and counted"). Do: submit a version whose message holds a NUL,
/// which git's strict fsck refuses in a commit (`nulInCommit`); then submit a second version with a plain message, and
/// clone. Expect: the first version accepted, its commit not made and counted, the repository's store holding no version
/// of the green and its register only the commit the submit owed (A-664's live whole builds, part 2: staged before its
/// map was built, settled by the next made commit); the second's commit made as the repository's first, with no parent,
/// its tree holding both versions' files.
#[test]
fn t_a664_p3_a_commit_git_refuses_is_not_made_and_its_successor_holds_both() {
  let (unmade, clone, made) = crate::daemon::audit_on_shard(|state| {
    let (repository, green) = git_repository(state, "refused");
    version_saying(state, (green, 1), vec![create("/f", b"one")], "one\0two");
    let key = green_key(to_db_volume(green));
    let held = state
      .vcs_stores
      .git_ref(repository)
      .map(|(_, git)| git.versions(key).len());
    let unmade = (counts(state), held, records(state, repository).len());
    version(state, green, 2, vec![create("/g", b"two")]);
    (unmade, cloned(state, repository, "main"), counts(state))
  });
  assert_eq!(unmade, ((0, 1), Some(0), 1));
  assert_eq!(parents_of(&clone.text), Vec::<ObjectId>::new());
  let expected = listing(&[("f", Mode::File, b"one"), ("g", Mode::File, b"two")]);
  assert_eq!(clone.files, expected);
  assert_eq!(made, (1, 1));
}

/// A work's `mv` of `from` to `to`.
fn moved(from: &str, to: &str) -> Op {
  Op::Declare(WorkOp::Rename {
    from: from.to_owned(),
    to: to.to_owned(),
  })
}

/// The mode fix (2026-11-20; main's check by use of A-618's finding: the green kept a mode at its path). Do: in one work
/// make `d/a`, make it executable, make `x` and move `d/a` to `x/a`, submit, and clone; then make `e/b` executable in one
/// version and move it to `y/b` in the next, and clone again. Expect: each clone lists each moved file `100755`, as
/// POSIX `rename` and `git mv` keep a file's mode.
#[test]
fn t_a664_p3_an_executable_file_moved_is_100755_in_its_commit() {
  let (first, second) = crate::daemon::audit_on_shard(|state| {
    let (repository, green) = git_repository(state, "moved");
    let ops = vec![
      mkdir("/d"),
      create("/d/a", b"alpha"),
      executable("/d/a"),
      mkdir("/x"),
      moved("/d/a", "/x/a"),
    ];
    version(state, green, 1, ops);
    let first = cloned(state, repository, "main").files;
    let ops = vec![mkdir("/e"), create("/e/b", b"beta"), executable("/e/b")];
    version(state, green, 2, ops);
    version(state, green, 3, vec![mkdir("/y"), moved("/e/b", "/y/b")]);
    (first, cloned(state, repository, "main").files)
  });
  assert_eq!(first, listing(&[("x/a", Mode::Executable, b"alpha")]));
  let expected = listing(&[
    ("x/a", Mode::Executable, b"alpha"),
    ("y/b", Mode::Executable, b"beta"),
  ]);
  assert_eq!(second, expected);
}

/// A pushed commit of three files on `repository`'s root green, its map left to its build (A-664's live whole builds):
/// the pushed tree.
fn pushed_three(
  state: &mut ShardState,
  (repository, root): (RepositoryId, slates_db::catalog::VolumeId),
) -> ObjectId {
  let files = [
    (b"README".to_vec(), b"one\n".to_vec()),
    (b"a".to_vec(), b"alpha\n".to_vec()),
    (b"b".to_vec(), b"beta\n".to_vec()),
  ];
  let (objects, pushed) = commit_of(&files, &[], FIRST_DATE);
  let tree = tree_of(&objects);
  push_unsettled(state, (repository, root), &objects, (None, pushed, tree));
  tree
}

/// A-664's live whole builds (a push's; main's ruling of 2026-11-20). Do: push a commit of three files to a git
/// repository's root green, whose map the push leaves to its build; read the green's kept tree; drive the build's slices
/// to its end, each slice given no budget (`whole_build::settle`, so the count is the files', whatever the load); read it
/// again. Expect: the push answered with no map kept and the build under way; the build ran in three slices, one a
/// file, and kept the pushed commit's tree, built once.
#[test]
fn t_a664_live_a_pushs_whole_map_is_built_in_slices_after_its_answer() {
  let seen = crate::daemon::audit_on_shard(|state| {
    let (repository, green) = git_repository(state, "sliced");
    let root = to_db_volume(green);
    let key = green_key(root);
    let tree = pushed_three(state, (repository, root));
    let building = crate::vcs_store::paths::whole_build::building;
    let answered = (kept_tree(state, repository, key), building(state, root));
    crate::vcs_store::paths::whole_build::settle(state);
    let built = (
      kept_tree(state, repository, key) == Some(tree),
      building(state, root),
    );
    let slices = count(state, crate::vcs_store::paths::whole_build::SLICES);
    (
      answered,
      built,
      slices,
      count(state, crate::vcs_store::paths::BUILT),
    )
  });
  assert_eq!(seen, ((None, true), (true, false), 3, 1));
}

/// A-664's live whole builds (a submit parked behind one). Do: push a commit of three files to a git repository's root
/// green, its map left to its build; submit a work editing `README` while the build is under way; drive the build to
/// its end. Expect: the submit parked, the green still at the pushed version; once the build ends the submit runs
/// again, the green at its version and its commit made over the map the build kept, its tree another than the
/// pushed one.
#[test]
fn t_a664_live_a_submit_waits_parked_for_its_greens_whole_build() {
  let seen = crate::daemon::audit_on_shard(|state| {
    let (repository, green) = git_repository(state, "parked");
    let root = to_db_volume(green);
    let key = green_key(root);
    let pushed = pushed_three(state, (repository, root));
    let head = |state: &ShardState| state.greens.get(&root).map(|engine| engine.head());
    let ask = crate::vcs_store::paths::tests::ask;
    let made = ask(
      state,
      slates_ipc::protocol::RequestBody::CreateWork {
        green,
        name: "work-parked".to_owned(),
      },
    );
    let slates_ipc::protocol::ReplyBody::WorkCreated { id: work, .. } = made else {
      return (None, None, None, false, 0);
    };
    ask(
      state,
      slates_ipc::protocol::RequestBody::Edit {
        work,
        path: "/README".to_owned(),
        at: 0,
        delete_len: 4,
        bytes: b"two\n".to_vec(),
      },
    );
    recorded_submit(state, crate::identities::authored(work, "edit the readme"));
    let parked = count(state, crate::vcs_store::paths::whole_build::PARKED);
    let during = head(state);
    crate::vcs_store::paths::whole_build::settle(state);
    let after = head(state);
    let tree = kept_tree(state, repository, key);
    (
      during,
      after,
      tree.filter(|tree| *tree != pushed),
      parked == 1,
      counts(state).0,
    )
  });
  assert_eq!(
    (seen.0, seen.1, seen.2.is_some(), seen.3, seen.4),
    (Some(1), Some(2), true, true, 1)
  );
}

/// `body` run as a recorded request is (`verbs::run_recorded`): in its own transaction under its completion key, so a
/// submit that waits is deferred with its key, as a client's is.
fn recorded_submit(state: &mut ShardState, body: slates_ipc::protocol::RequestBody) {
  let id = slates_wire::request::RequestId {
    client: 1,
    sequence: 7,
  };
  state.db.begin();
  state.current_request = Some((1, id));
  let reply = crate::verbs::dispatch(state, 1, &owner(), body);
  state.current_request = None;
  let deferred = std::mem::take(&mut state.acceptance_deferred);
  let committed = state.db.commit(&mut state.segment);
  assert!(deferred && committed.is_ok(), "{reply:?}");
}

/// A work making `/README` submitted to `green`'s first version with `message`, its map and commit left to its build
/// (A-664's live whole builds, part 2): the version.
fn first_submit_unsettled(
  state: &mut ShardState,
  green: slates_ipc::protocol::VolumeId,
  message: &str,
) -> u64 {
  let ask = crate::vcs_store::paths::tests::ask;
  let made = ask(
    state,
    slates_ipc::protocol::RequestBody::CreateWork {
      green,
      name: "work-owed".to_owned(),
    },
  );
  let slates_ipc::protocol::ReplyBody::WorkCreated { id: work, .. } = made else {
    return 0;
  };
  ask(
    state,
    slates_ipc::protocol::RequestBody::Edit {
      work,
      path: "/README".to_owned(),
      at: 0,
      delete_len: 0,
      bytes: b"owed\n".to_vec(),
    },
  );
  match ask(state, crate::identities::authored(work, message)) {
    slates_ipc::protocol::ReplyBody::Submitted {
      version: Some(version),
      ..
    } => version,
    _ => 0,
  }
}

/// The commit `repository`'s register owes, read from its newest record.
fn staged_owed(
  state: &ShardState,
  repository: RepositoryId,
) -> Option<crate::vcs_store::owed::Owed> {
  records(state, repository)
    .last()
    .and_then(|bytes| crate::vcs_store::owed::Owed::decode(bytes).ok())
}

/// A-664's live whole builds, part 2 (the owed commit). Do: submit a work making `/README` to a git repository's root
/// green, its first version, whose map is built whole after it; read the register and the counts; drive the build to
/// its end; clone `main`. Expect: the submit accepted as version 1 with its commit owed, staged in the register, none
/// made yet; at the build's end the commit made, listed at `main`, dated at the second the submit staged and carrying
/// its message, with no parent; nothing owed after it.
#[test]
fn t_a664_owed_a_first_submits_commit_is_owed_then_made_at_its_staged_second() {
  let seen = crate::daemon::audit_on_shard(|state| {
    let (repository, green) = git_repository(state, "owed");
    let version = first_submit_unsettled(state, green, "the owed commit");
    let owed = staged_owed(state, repository);
    let before = (version, owed.is_some(), counts(state).0);
    crate::vcs_store::paths::whole_build::settle(state);
    let main = cloned(state, repository, "main");
    let still = state
      .vcs_stores
      .git_ref(repository)
      .map(|(_, git)| git.owed.len());
    (before, owed, main, counts(state).0, still)
  });
  let (before, owed, main, made, still) = seen;
  assert_eq!(before, (1, true, 0));
  let owed = owed.expect("the staged commit");
  assert_eq!(
    (date_of(&main.text), parents_of(&main.text), made, still),
    (owed.seconds, Vec::new(), 1, Some(0))
  );
  assert!(
    main.text.ends_with("\n\nthe owed commit\n"),
    "{}",
    main.text
  );
}

/// A-664's live whole builds, part 2 (main's determinism rule: a replay reads no clock). Do: submit a git repository's
/// root green's first version, its commit owed; make the store again from the log twice, as two restarts before its
/// build ends would; drive the build to its end; clone `main`; then make the store again once more, from a log that
/// holds the made commit, and clone again. Expect: the commit made after the two restarts is the one the staged record
/// names, byte for byte (its text the staged second, message and identity over its tree), and the store made again
/// after it lists `main` at the same commit, holds it whole and owes nothing.
#[test]
fn t_a664_owed_a_commit_owed_across_two_restarts_is_made_byte_for_byte_from_its_record() {
  let seen = crate::daemon::audit_on_shard(|state| {
    let (repository, green) = git_repository(state, "restarted");
    first_submit_unsettled(state, green, "made after two restarts");
    let owed = staged_owed(state, repository);
    crate::vcs_store::rebuild(state, repository);
    crate::vcs_store::rebuild(state, repository);
    crate::vcs_store::paths::whole_build::settle(state);
    let first = cloned(state, repository, "main");
    crate::vcs_store::rebuild(state, repository);
    let again = cloned(state, repository, "main");
    let owing = state
      .vcs_stores
      .git_ref(repository)
      .map(|(_, git)| git.owed.len());
    (owed, first, again, owing)
  });
  let (owed, first, again, owing) = seen;
  let owed = owed.expect("the staged commit");
  let expected = made_text(tree_named(&first.text), &[], owed.seconds, &owed.message);
  assert_eq!(first.text, expected);
  assert_eq!(
    (again.head, again.text, owing),
    (first.head, first.text, Some(0))
  );
}

/// A-664's live whole builds (main's ruling of 2026-11-20: tens of thousands of concurrent pushes, so a busy green's
/// build must end). Do: push a commit of sixty-one files to a git repository's root green, its map left to its build;
/// then fifty pushes, each a commit over the one before changing `README`, with one slice of the build (one file)
/// between each two; drive the build to its end. Expect: no build started again, the build moved to the newest head at
/// its slices' boundaries, built once, and the tree kept the last push's.
#[test]
fn t_a664_live_a_storm_of_fifty_pushes_never_restarts_a_whole_build() {
  let seen = crate::daemon::audit_on_shard(|state| {
    let (repository, green) = git_repository(state, "storm");
    let root = to_db_volume(green);
    let mut files: Vec<(Vec<u8>, Vec<u8>)> = (0..60)
      .map(|serial| {
        (
          format!("f{serial}").into_bytes(),
          format!("file {serial}\n").into_bytes(),
        )
      })
      .collect();
    files.insert(0, (b"README".to_vec(), b"one\n".to_vec()));
    let (objects, mut head) = commit_of(&files, &[], FIRST_DATE);
    let mut tree = tree_of(&objects);
    push_unsettled(state, (repository, root), &objects, (None, head, tree));
    for push in 0..50u64 {
      crate::vcs_store::paths::whole_build::step_once(state);
      if let Some(readme) = files.first_mut() {
        readme.1 = format!("push {push}\n").into_bytes();
      }
      let (objects, next) = commit_of(&files, &[head], FIRST_DATE.saturating_add(push + 1));
      let before = tree;
      tree = tree_of(&objects);
      push_unsettled(
        state,
        (repository, root),
        &objects,
        (Some(before), next, tree),
      );
      head = next;
    }
    crate::vcs_store::paths::whole_build::settle(state);
    let whole_build = |name| count(state, name);
    (
      whole_build(crate::vcs_store::paths::whole_build::RESTARTED),
      whole_build(crate::vcs_store::paths::whole_build::MOVED) > 0,
      whole_build(crate::vcs_store::paths::BUILT),
      kept_tree(state, repository, green_key(root)) == Some(tree),
    )
  });
  assert_eq!(seen, (0, true, 1, true));
}
