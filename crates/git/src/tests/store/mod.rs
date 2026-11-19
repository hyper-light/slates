//! The git object store (A-614, §4.17.2 piece 5 part 1): git's naming with its refusal in git's words, positions as runs,
//! rows read back, commit rows, D(k) for fast-forwards, creates, rewrites and deletes, a version read as a tree, and
//! every refusal leaving nothing behind; `oracle` holds the whole store to a map model over generated histories.

use std::convert::Infallible;

use slates_vcs::{ContentStore, Naming};

use crate::negotiation::Filter;
use crate::object_name::{CollisionAttack, Hasher, Kind, ObjectFormat, ObjectId, name_of};
use crate::objects::tree::{Entry, Mode, compare, write_entries};
use crate::store::learn::{Clock, Exchange};
use crate::store::rows::{self, CommitRow, ObjectRow};
use crate::store::{
  Answered, COLLISION_WORDS, Entries, Found, GitNaming, Incoming, Move, NameRefused, Positions,
  Progress, Push, PushRefused, Repository, StoreRefused, Turn, Upload, UploadCounters, Versions,
};

mod encoding;
mod fragments;
mod hook_view;
mod oracle;
mod protect;
mod reach;
mod upload;

/// Shape: the window the tests' content store holds content in.
const WINDOW: usize = 1 << 10;

/// Shape: the tests' content budget, room to spare.
const BUDGET: u64 = 1 << 26;

/// The tests' clock: a nanosecond a reading.
struct Ticks(u64);

impl Clock for Ticks {
  fn now_ns(&mut self) -> u64 {
    self.0 += 1;
    self.0
  }
}

/// Shape: the tests' exchange, a byte saved worth a nanosecond.
const EXCHANGE: Exchange = Exchange { ns: 1, bytes: 1 };

/// git's own t0013 vector, `shattered-1.pdf`, cut to the 320 bytes that hold both colliding blocks (A-600's fixture).
const SHATTERED_1: &[u8] = include_bytes!("../../../fixtures/objects/shattered-1.prefix.bin");

/// An object the tests made.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Made {
  kind: Kind,
  id: ObjectId,
  content: Vec<u8>,
}

fn made(format: ObjectFormat, kind: Kind, content: Vec<u8>) -> Made {
  let id = name_of(format, kind, &content).unwrap();
  Made { kind, id, content }
}

fn blob(format: ObjectFormat, bytes: &[u8]) -> Made {
  made(format, Kind::Blob, bytes.to_vec())
}

/// A tree of `entries`, written in git's order.
fn tree(format: ObjectFormat, entries: &[(Mode, &[u8], ObjectId)]) -> Made {
  let mut list: Vec<Entry<'_>> = entries
    .iter()
    .map(|(mode, name, id)| Entry::new(*mode, name, id.as_bytes()))
    .collect();
  list.sort_by(compare);
  let mut content = Vec::new();
  write_entries(&list, &mut content);
  made(format, Kind::Tree, content)
}

/// A commit of `tree` over `parents`, authored and committed at `seconds`.
fn commit(format: ObjectFormat, tree: ObjectId, parents: &[ObjectId], seconds: u64) -> Made {
  let mut text = format!("tree {tree}\n");
  for parent in parents {
    text.push_str(&format!("parent {parent}\n"));
  }
  text.push_str(&format!(
    "author A U Thor <author@example.com> {seconds} +0000\ncommitter C O Mitter <committer@example.com> {seconds} +0000\n\nmessage\n"
  ));
  made(format, Kind::Commit, text.into_bytes())
}

/// The tests' stand-in for a pack reader: objects handed in in order, each with its id claimed or not.
struct Source {
  objects: Vec<Made>,
  at: usize,
  claim: bool,
  /// The chain depth each object reports: a client that deltified its pack this deep.
  depth: u64,
}

impl Source {
  fn new(objects: Vec<Made>) -> Source {
    Source {
      objects,
      at: 0,
      claim: true,
      depth: 2,
    }
  }
}

impl Entries for Source {
  type Refused = Infallible;

  fn next_entry(&mut self) -> Option<Result<Incoming<'_>, Infallible>> {
    let object = self.objects.get(self.at)?;
    self.at += 1;
    Some(Ok(Incoming {
      kind: object.kind,
      claimed: self.claim.then_some(object.id),
      content: &object.content,
      depth: self.depth,
    }))
  }
}

fn store() -> ContentStore {
  ContentStore::new(BUDGET, WINDOW).unwrap()
}

/// A sealed push of `objects`.
fn sealed(repository: &Repository, store: &mut ContentStore, objects: Vec<Made>) -> Push {
  let mut push = Push::new(repository.format());
  push
    .admit_all(store, &repository.naming(), &mut Source::new(objects))
    .unwrap();
  repository.seal(store, &mut push).unwrap();
  push
}

/// Pushes `objects` and moves each green to its head in one commit; the versions made, or the refusal after the push
/// was abandoned.
fn push_and_move(
  repository: &mut Repository,
  store: &mut ContentStore,
  objects: Vec<Made>,
  moves: &[(u64, Option<ObjectId>)],
) -> Result<Vec<u64>, StoreRefused> {
  let mut push = sealed(repository, store, objects);
  let admitted: Result<Vec<Move>, StoreRefused> = moves
    .iter()
    .map(|(green, to)| repository.admit(store, &push, *green, *to))
    .collect();
  let committed = admitted.and_then(|admitted| repository.commit(store, &mut push, &admitted));
  if committed.is_err() {
    push.abandon(store).unwrap();
  }
  committed
}

/// The tests' answer to a fetch (A-639): its runs and objects, the client's capabilities, and its filter.
#[derive(Clone, Debug)]
struct Answer {
  runs: Vec<Versions>,
  objects: Vec<u64>,
  thin: bool,
  ofs: bool,
  filter: Option<Filter>,
}

impl Answer {
  /// `objects` one by one, for a client that reads offset deltas and asked no thin pack.
  fn objects(objects: &Positions) -> Answer {
    Answer {
      runs: Vec::new(),
      objects: objects.iter().collect(),
      thin: false,
      ofs: true,
      filter: None,
    }
  }
}

impl Answered for Answer {
  fn runs(&self) -> &[Versions] {
    &self.runs
  }

  fn objects(&self) -> &[u64] {
    &self.objects
  }

  fn thin(&self) -> bool {
    self.thin
  }

  fn ofs_delta(&self) -> bool {
    self.ofs
  }

  fn admits(&self, kind: Kind, size: u64) -> bool {
    self
      .filter
      .as_ref()
      .is_none_or(|filter| filter.admits(kind, size, 0))
  }
}

/// Shape: the turns a test's stream may take before the test calls it stuck.
const TURNS: u64 = 1 << 24;

/// The pack that answers `answer`, streamed in turns of `quantum` nanoseconds of the tests' clock, and what the stream
/// did; a refusal fails the test.
fn streamed(
  repository: &Repository,
  store: &ContentStore,
  answer: Answer,
  quantum: u64,
) -> (Vec<u8>, UploadCounters) {
  let mut upload = Upload::new(answer, repository);
  let mut clock = Ticks(0);
  let mut pack = Vec::new();
  for _ in 0..TURNS {
    let turn = Turn {
      clock: &mut clock,
      exchange: EXCHANGE,
      quantum_ns: quantum,
    };
    if upload.advance(repository, store, turn, &mut pack).unwrap() == Progress::Done {
      return (pack, upload.counters());
    }
  }
  panic!("the stream took {TURNS} turns");
}

/// The pack of `objects` one by one, in one turn.
fn pack_of(repository: &Repository, store: &ContentStore, objects: &Positions) -> Vec<u8> {
  streamed(repository, store, Answer::objects(objects), u64::MAX).0
}

/// D(k) of `green`'s `versions`, joined.
fn brought_by(
  repository: &Repository,
  green: u64,
  versions: impl IntoIterator<Item = usize>,
) -> Positions {
  versions.into_iter().fold(Positions::new(), |all, version| {
    all.union(repository.brought(green, version).unwrap())
  })
}

/// The ids of a set of positions.
fn ids(repository: &Repository, store: &ContentStore, positions: &Positions) -> Vec<ObjectId> {
  let mut ids: Vec<ObjectId> = positions
    .iter()
    .map(|position| repository.id_at(store, position).unwrap())
    .collect();
  ids.sort();
  ids
}

fn sorted(mut ids: Vec<ObjectId>) -> Vec<ObjectId> {
  ids.sort();
  ids
}

/// A stand-in for git's SHA-1 that hashes the raw bytes with collision detection on, as git's t0013 does: the only way
/// a published collision reaches the store's naming, since none survives git's object header.
struct RawSha1;

impl Naming for RawSha1 {
  type Id = ObjectId;
  type Refused = NameRefused;

  fn name(&self, _kind: u8, _context: &[u8], bytes: &[u8]) -> Result<ObjectId, NameRefused> {
    let mut hasher = Hasher::raw(ObjectFormat::Sha1);
    hasher.update(bytes);
    hasher.finish().map_err(NameRefused::CollisionAttack)
  }
}

/// A-614, §4.17.2 "The repository in git's terms" item 1. Do: name a blob through git's naming in both formats, a
/// number no type has, and push git's own SHAttered vector through a naming that hashes raw bytes with detection on.
/// Expect: the names `name_of` gives; the number refused in index-pack's words; the vector refused `NotNamed` with the
/// collision attack and git's own words naming the plain SHA-1, nothing stored and nothing held.
#[test]
fn git_names_each_object_and_a_collision_attack_is_refused_in_gits_words() {
  for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
    let naming = GitNaming { format };
    let named = naming.name(Kind::Blob.pack_number(), b"", b"hello\n");
    assert_eq!(named, Ok(name_of(format, Kind::Blob, b"hello\n").unwrap()));
  }
  let unknown = GitNaming {
    format: ObjectFormat::Sha1,
  }
  .name(9, b"", b"");
  assert_eq!(unknown, Err(NameRefused::UnknownKind { kind: 9 }));
  assert_eq!(
    NameRefused::UnknownKind { kind: 9 }.words(),
    "unknown object type 9"
  );
  let mut store = store();
  let mut push = Push::new(ObjectFormat::Sha1);
  let incoming = Incoming {
    kind: Kind::Blob,
    claimed: None,
    content: SHATTERED_1,
    depth: 0,
  };
  let refused: Result<_, PushRefused<Infallible>> = push.admit(&mut store, &RawSha1, incoming);
  let Err(PushRefused::NotNamed { reason }) = refused else {
    panic!("not refused by its naming");
  };
  let NameRefused::CollisionAttack(CollisionAttack { sha1 }) = reason else {
    panic!("not refused as an attack: {reason:?}");
  };
  assert_eq!(reason.words(), format!("{COLLISION_WORDS}{sha1}"));
  assert!(store.is_empty() && push.is_empty());
}

/// A-614, §4.17.2 item 3 (a set of positions kept compressed). Do: make a set of scattered positions, read it, and join
/// it with another. Expect: runs where positions touch, membership exact at every edge, the count and the order kept,
/// and a union whose touching runs become one.
#[test]
fn positions_are_kept_as_runs() {
  let set = Positions::of(vec![5, 3, 4, 10, 11, 1, 4]);
  assert_eq!((set.runs(), set.len()), (3, 6));
  assert_eq!(set.iter().collect::<Vec<_>>(), vec![1, 3, 4, 5, 10, 11]);
  let members: Vec<bool> = (0..13).map(|position| set.contains(position)).collect();
  assert_eq!(
    members,
    vec![
      false, true, false, true, true, true, false, false, false, false, true, true, false
    ]
  );
  let joined = set.union(&Positions::of(vec![2, 6, 7, 12]));
  assert_eq!((joined.runs(), joined.len()), (2, 10));
  assert!(Positions::new().is_empty() && !joined.is_empty());
}

/// A commit's row, as the tests write one.
fn commit_row() -> ObjectRow {
  ObjectRow {
    kind: Kind::Commit,
    len: 210,
    position: 7,
    green: 3,
    version: 2,
    commit: Some(CommitRow {
      tree: 5,
      date: 1_700_000_000,
      generation: 4,
      parents: vec![1, 6],
    }),
  }
}

/// A-614, §4.17.2 item 2. Do: write a blob's row and a commit's, read them back, and read hostile payloads: cut, an
/// unknown type, a byte left over, a commit's part after a blob's type; then order keys. Expect: each row as written;
/// every hostile payload refused; every object row's key before every position row's, and position rows in order.
#[test]
fn an_object_row_reads_back_and_a_hostile_one_is_refused() {
  let blob_row = ObjectRow {
    kind: Kind::Blob,
    commit: None,
    ..commit_row()
  };
  for row in [blob_row.clone(), commit_row()] {
    assert_eq!(rows::decode(&rows::encode(&row)), Some(row));
  }
  let encoded = rows::encode(&commit_row());
  assert_eq!(rows::decode(&encoded[..encoded.len() - 1]), None);
  let mut unknown = encoded.clone();
  unknown[0] = 9;
  let mut longer = encoded.clone();
  longer.push(0);
  let mut blob_with_commit = encoded.clone();
  blob_with_commit[0] = Kind::Blob.pack_number();
  assert!(
    [unknown, longer, blob_with_commit]
      .iter()
      .all(|payload| rows::decode(payload).is_none())
  );
  let id = name_of(ObjectFormat::Sha1, Kind::Blob, b"").unwrap();
  assert!(rows::object_key(&id) < rows::position_key(0));
  assert!(rows::position_key(255) < rows::position_key(256));
}

/// A small history: `c1` (a root), `c2` and `c3` over it, and `merge` over both; returned in the order a pack sends
/// commits, children first.
struct History {
  objects: Vec<Made>,
  c1: Made,
  c2: Made,
  c3: Made,
  merge: Made,
  t1: Made,
  t2: Made,
}

fn history(format: ObjectFormat) -> History {
  let (a, b) = (blob(format, b"a\n"), blob(format, b"b\n"));
  let t1 = tree(format, &[(Mode::File, b"a", a.id)]);
  let t2 = tree(
    format,
    &[(Mode::File, b"a", a.id), (Mode::File, b"b", b.id)],
  );
  let c1 = commit(format, t1.id, &[], 100);
  let c2 = commit(format, t2.id, &[c1.id], 200);
  let c3 = commit(format, t1.id, &[c1.id], 300);
  let merge = commit(format, t2.id, &[c2.id, c3.id], 400);
  let objects = vec![
    merge.clone(),
    c3.clone(),
    c2.clone(),
    c1.clone(),
    t2.clone(),
    t1.clone(),
    b,
    a,
  ];
  History {
    objects,
    c1,
    c2,
    c3,
    merge,
    t1,
    t2,
  }
}

/// A commit's part of its row, read back with its tree and parents as ids.
fn commit_of(
  repository: &Repository,
  store: &ContentStore,
  id: &ObjectId,
) -> (ObjectId, Vec<ObjectId>, u64, u64) {
  let commit = repository.object(store, id).unwrap().commit.unwrap();
  let tree = repository.id_at(store, commit.tree).unwrap();
  let parents = commit
    .parents
    .iter()
    .map(|parent| repository.id_at(store, *parent).unwrap())
    .collect();
  (tree, parents, commit.date, commit.generation)
}

/// A-614, §4.17.2 item 2. Do: push a merge history children first, as a pack sends it, in both formats. Expect: every
/// commit's row with its tree, its parents in its own order, its committer's date and its generation number (one for
/// the root, one more than the greatest parent's), each object a position of its own, and every row naming the green
/// and version that brought it.
#[test]
fn a_pushs_commits_get_rows_with_their_tree_parents_date_and_generation() {
  for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
    let mut store = store();
    let mut repository = Repository::new(format);
    let made = history(format);
    let versions = push_and_move(
      &mut repository,
      &mut store,
      made.objects.clone(),
      &[(1, Some(made.merge.id))],
    )
    .unwrap();
    assert_eq!(versions, vec![0]);
    assert_eq!(
      commit_of(&repository, &store, &made.c1.id),
      (made.t1.id, vec![], 100, 1)
    );
    assert_eq!(
      commit_of(&repository, &store, &made.c3.id),
      (made.t1.id, vec![made.c1.id], 300, 2)
    );
    let merge = commit_of(&repository, &store, &made.merge.id);
    assert_eq!(merge, (made.t2.id, vec![made.c2.id, made.c3.id], 400, 3));
    let rows: Vec<ObjectRow> = made
      .objects
      .iter()
      .map(|object| repository.object(&store, &object.id).unwrap())
      .collect();
    let positions = Positions::of(rows.iter().map(|row| row.position).collect());
    assert_eq!((positions.len(), positions.runs()), (8, 1));
    assert!(rows.iter().all(|row| (row.green, row.version) == (1, 0)));
    assert_eq!(repository.counters().positions, 8);
    repository.release(&mut store).unwrap();
    assert!(store.is_empty() && store.used() == 0);
  }
}

/// A root commit over a wide directory `d` and a file `top`, and a second commit that changes `top` alone.
fn wide(format: ObjectFormat) -> (Vec<Made>, Vec<Made>, Made, Made) {
  let files: Vec<Made> = (0..8u8).map(|at| blob(format, &[b'f', at])).collect();
  let names: Vec<[u8; 1]> = (0..8u8).map(|at| [b'a' + at]).collect();
  let entries: Vec<(Mode, &[u8], ObjectId)> = names
    .iter()
    .zip(&files)
    .map(|(name, file)| (Mode::File, &name[..], file.id))
    .collect();
  let d = tree(format, &entries);
  let (top1, top2) = (blob(format, b"one\n"), blob(format, b"two\n"));
  let root1 = tree(
    format,
    &[(Mode::Directory, b"d", d.id), (Mode::File, b"top", top1.id)],
  );
  let root2 = tree(
    format,
    &[(Mode::Directory, b"d", d.id), (Mode::File, b"top", top2.id)],
  );
  let c1 = commit(format, root1.id, &[], 10);
  let c2 = commit(format, root2.id, &[c1.id], 20);
  let mut first = vec![c1.clone(), root1, d, top1];
  first.extend(files);
  (first, vec![c2.clone(), root2, top2], c1, c2)
}

/// A-614, §4.17.2 item 3. Do: make a ref at a root commit over a wide directory, then fast-forward it, changing one
/// file. Expect: D(0) everything the root reaches; D(1) only the new commit, its root tree and its new file, the
/// unchanged directory skipped whole (the counter moves) and never walked; the move a fast-forward; and the reached set
/// D(0) and D(1) together.
#[test]
fn a_fast_forward_brings_only_what_its_green_had_not_reached() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let (first, second, c1, c2) = wide(format);
  let first_ids = sorted(first.iter().map(|object| object.id).collect());
  let second_ids = sorted(second.iter().map(|object| object.id).collect());
  push_and_move(&mut repository, &mut store, first, &[(1, Some(c1.id))]).unwrap();
  assert_eq!(
    ids(&repository, &store, repository.brought(1, 0).unwrap()),
    first_ids
  );
  let skipped = repository.counters().trees_skipped;
  push_and_move(&mut repository, &mut store, second, &[(1, Some(c2.id))]).unwrap();
  assert_eq!(
    ids(&repository, &store, repository.brought(1, 1).unwrap()),
    second_ids
  );
  assert!(repository.counters().trees_skipped > skipped);
  assert_eq!(repository.counters().fast_forwards, 1);
  let reached = repository
    .brought(1, 0)
    .unwrap()
    .union(repository.brought(1, 1).unwrap());
  assert_eq!(repository.reached(1), Some(&reached));
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// The positions of `objects`' rows.
fn positions_of(repository: &Repository, store: &ContentStore, objects: &[&Made]) -> Positions {
  Positions::of(
    objects
      .iter()
      .map(|object| repository.object(store, &object.id).unwrap().position)
      .collect(),
  )
}

/// A-614, §4.17.2 item 3 (a set made again from the newest green head below). Do: make ref 1 at a two-commit history,
/// make ref 2 at ref 1's head with nothing pushed, then make ref 3 at a new commit over that head. Expect: ref 2's
/// D(0) and reached set ref 1's, found as a base with no object walked; ref 3's D(0) and reached set ref 1's and the new
/// commit's own three objects, the walk visiting only those.
#[test]
fn a_ref_made_at_another_refs_head_reuses_its_set_without_a_walk() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let (mut objects, second, _, c2) = wide(format);
  objects.extend(second);
  push_and_move(&mut repository, &mut store, objects, &[(1, Some(c2.id))]).unwrap();
  let walked = repository.counters().walked;
  push_and_move(&mut repository, &mut store, Vec::new(), &[(2, Some(c2.id))]).unwrap();
  assert_eq!(repository.counters().walked, walked);
  assert_eq!(
    (repository.reached(2), repository.brought(2, 0)),
    (repository.reached(1), repository.reached(1))
  );
  let top3 = blob(format, b"three\n");
  let root4 = tree(format, &[(Mode::File, b"top", top3.id)]);
  let c4 = commit(format, root4.id, &[c2.id], 40);
  let pushed = vec![c4.clone(), root4.clone(), top3.clone()];
  push_and_move(&mut repository, &mut store, pushed, &[(3, Some(c4.id))]).unwrap();
  assert_eq!(repository.counters().walked, walked + 3);
  assert_eq!(repository.counters().bases_found, 2);
  let own = positions_of(&repository, &store, &[&c4, &root4, &top3]);
  let expected = repository.reached(1).unwrap().union(&own);
  assert_eq!(
    (repository.brought(3, 0), repository.reached(3)),
    (Some(&expected), Some(&expected))
  );
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// Three commits in a line and a fourth over the first, each tree adding a file: `c1` to `c3`, then `c2b` over `c1`.
fn line_and_branch(format: ObjectFormat) -> [Vec<Made>; 4] {
  let (a, b, c, x) = (
    blob(format, b"a"),
    blob(format, b"b"),
    blob(format, b"c"),
    blob(format, b"x"),
  );
  let t1 = tree(format, &[(Mode::File, b"a", a.id)]);
  let t2 = tree(
    format,
    &[(Mode::File, b"a", a.id), (Mode::File, b"b", b.id)],
  );
  let t3 = tree(
    format,
    &[
      (Mode::File, b"a", a.id),
      (Mode::File, b"b", b.id),
      (Mode::File, b"c", c.id),
    ],
  );
  let t2b = tree(
    format,
    &[(Mode::File, b"a", a.id), (Mode::File, b"x", x.id)],
  );
  let c1 = commit(format, t1.id, &[], 1);
  let c2 = commit(format, t2.id, &[c1.id], 2);
  let c3 = commit(format, t3.id, &[c2.id], 3);
  let c2b = commit(format, t2b.id, &[c1.id], 4);
  [
    vec![c1, t1, a],
    vec![c2, t2, b],
    vec![c3, t3, c],
    vec![c2b, t2b, x],
  ]
}

/// A-614, §4.17.2 item 3 (a rewrite). Do: ref 1 at c1, c2, c3 in turn; ref 2 at c1; then ref 1 rewritten to c2b, a
/// commit over c1. Expect: the rewrite no fast-forward; D(3) only c2b and what it alone names; ref 1's reached set made
/// again from ref 2's (a base found), exactly what c2b reaches, c2's and c3's own objects gone from it.
#[test]
fn a_rewrite_brings_what_its_old_head_did_not_reach_and_makes_its_set_again() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let [first, second, third, rewritten] = line_and_branch(format);
  let heads: Vec<ObjectId> = [&first, &second, &third, &rewritten]
    .iter()
    .map(|objects| objects[0].id)
    .collect();
  for (objects, head) in [first.clone(), second, third].into_iter().zip(&heads) {
    push_and_move(&mut repository, &mut store, objects, &[(1, Some(*head))]).unwrap();
  }
  push_and_move(
    &mut repository,
    &mut store,
    Vec::new(),
    &[(2, Some(heads[0]))],
  )
  .unwrap();
  let before = repository.counters();
  let rewritten_ids = sorted(rewritten.iter().map(|object| object.id).collect());
  push_and_move(
    &mut repository,
    &mut store,
    rewritten.clone(),
    &[(1, Some(heads[3]))],
  )
  .unwrap();
  let after = repository.counters();
  assert_eq!(
    (after.fast_forwards, after.remade, after.bases_found),
    (
      before.fast_forwards,
      before.remade + 1,
      before.bases_found + 1
    )
  );
  assert_eq!(
    ids(&repository, &store, repository.brought(1, 3).unwrap()),
    rewritten_ids
  );
  let reaches = sorted(
    first
      .iter()
      .chain(&rewritten)
      .map(|object| object.id)
      .collect(),
  );
  assert_eq!(
    ids(&repository, &store, repository.reached(1).unwrap()),
    reaches
  );
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// A-614, §4.17.2 item 6 (a deleted ref). Do: make ref 1 at a commit, delete it, then make it again at the same commit.
/// Expect: the deletion a version with no head, nothing brought and nothing reached; made again, D(2) everything its
/// head reaches, and its head and tree read back.
#[test]
fn a_deleted_ref_reaches_nothing_and_made_again_brings_all_its_head_reaches() {
  let format = ObjectFormat::Sha256;
  let mut store = store();
  let mut repository = Repository::new(format);
  let [first, ..] = line_and_branch(format);
  let (head, root) = (first[0].id, first[1].id);
  push_and_move(
    &mut repository,
    &mut store,
    first.clone(),
    &[(1, Some(head))],
  )
  .unwrap();
  assert_eq!(
    push_and_move(&mut repository, &mut store, Vec::new(), &[(1, None)]),
    Ok(vec![1])
  );
  assert_eq!(
    (
      repository.head(&store, 1, 1),
      repository.brought(1, 1).map(Positions::len)
    ),
    (None, Some(0))
  );
  assert!(repository.reached(1).unwrap().is_empty());
  push_and_move(&mut repository, &mut store, Vec::new(), &[(1, Some(head))]).unwrap();
  let all = sorted(first.iter().map(|object| object.id).collect());
  assert_eq!(
    ids(&repository, &store, repository.brought(1, 2).unwrap()),
    all
  );
  assert_eq!(
    (repository.head(&store, 1, 2), repository.tree(&store, 1, 2)),
    (Some(head), Some(root))
  );
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// A commit over `d/e/z` (holding `deep`) and `top`.
fn nested(
  format: ObjectFormat,
  deep: &Made,
  top: &Made,
  parents: &[ObjectId],
  seconds: u64,
) -> Vec<Made> {
  let e = tree(format, &[(Mode::File, b"z", deep.id)]);
  let d = tree(format, &[(Mode::Directory, b"e", e.id)]);
  let root = tree(
    format,
    &[(Mode::Directory, b"d", d.id), (Mode::File, b"top", top.id)],
  );
  let head = commit(format, root.id, parents, seconds);
  vec![head, root, d, e, deep.clone(), top.clone()]
}

/// A-614, §4.17.2 item 5's read (a version's tree, no walk). Do: make a ref at a commit over nested directories, then
/// change one file deep in them; read each version's head and tree, and resolve paths. Expect: each version's head and
/// root tree as pushed; the deep path resolved to the old blob at version 0 and the new at version 1; a directory to
/// its subtree; a missing path, and one through a file, to nothing.
#[test]
fn a_versions_tree_is_read_from_its_rows_and_a_path_resolves_one_tree_a_component() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let (old, new, top) = (
    blob(format, b"old\n"),
    blob(format, b"new\n"),
    blob(format, b"top\n"),
  );
  let first = nested(format, &old, &top, &[], 1);
  let second = nested(format, &new, &top, &[first[0].id], 2);
  push_and_move(
    &mut repository,
    &mut store,
    first.clone(),
    &[(1, Some(first[0].id))],
  )
  .unwrap();
  push_and_move(
    &mut repository,
    &mut store,
    second.clone(),
    &[(1, Some(second[0].id))],
  )
  .unwrap();
  assert_eq!(
    (repository.head(&store, 1, 1), repository.tree(&store, 1, 1)),
    (Some(second[0].id), Some(second[1].id))
  );
  let file = |id: ObjectId| {
    Ok(Some(Found {
      mode: Mode::File.bits(),
      id,
    }))
  };
  assert_eq!(repository.entry(&store, 1, 0, b"d/e/z"), file(old.id));
  assert_eq!(repository.entry(&store, 1, 1, b"/d/e/z"), file(new.id));
  let directory = Found {
    mode: Mode::Directory.bits(),
    id: second[2].id,
  };
  assert_eq!(repository.entry(&store, 1, 1, b"d"), Ok(Some(directory)));
  assert_eq!(repository.entry(&store, 1, 1, b"d/missing"), Ok(None));
  assert_eq!(repository.entry(&store, 1, 1, b"top/inside"), Ok(None));
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// A-614, §4.17.1 "Objects" items 4 and 5. Do: abandon an admitted push, and admit an object under a false id. Expect:
/// the abandoned push leaves the store empty and the repository without a row or a version; the false id refused
/// `IdMismatch` naming the id claimed, with nothing stored.
#[test]
fn an_abandoned_push_leaves_nothing_and_a_false_claim_is_refused() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let repository = Repository::new(format);
  let [first, second, ..] = line_and_branch(format);
  let push = sealed(&repository, &mut store, first.clone());
  let admitted = repository.admit(&store, &push, 1, Some(first[0].id));
  assert!(admitted.is_ok() && !store.is_empty());
  push.abandon(&mut store).unwrap();
  assert!(store.is_empty());
  assert_eq!(
    (
      repository.versions(1),
      repository.object(&store, &first[0].id)
    ),
    (0, None)
  );
  let mut push = Push::new(format);
  let incoming = Incoming {
    kind: second[2].kind,
    claimed: Some(first[2].id),
    content: &second[2].content,
    depth: 0,
  };
  let refused: Result<_, PushRefused<Infallible>> =
    push.admit(&mut store, &repository.naming(), incoming);
  assert_eq!(refused, Err(PushRefused::IdMismatch { id: first[2].id }));
  assert!(store.is_empty() && push.is_empty());
}

/// A-614. Do: seal a push whose commit names a parent neither it nor the repository holds; admit a move before sealing;
/// seal a push made for the other format. Expect: `Missing` naming the parent; `Unsealed`; `ForeignPush`.
#[test]
fn a_missing_parent_an_unsealed_push_and_a_foreign_push_are_refused() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let repository = Repository::new(format);
  let [first, second, ..] = line_and_branch(format);
  let mut push = Push::new(format);
  push
    .admit_all(
      &mut store,
      &repository.naming(),
      &mut Source::new(second.clone()),
    )
    .unwrap();
  assert_eq!(
    repository.admit(&store, &push, 1, Some(second[0].id)),
    Err(StoreRefused::Unsealed)
  );
  assert_eq!(
    repository.seal(&store, &mut push),
    Err(StoreRefused::Missing { id: first[0].id })
  );
  push.abandon(&mut store).unwrap();
  let mut foreign = Push::new(ObjectFormat::Sha256);
  assert_eq!(
    repository.seal(&store, &mut foreign),
    Err(StoreRefused::ForeignPush)
  );
  assert!(store.is_empty());
}

/// A-614, §4.2. Do: admit two creates of one ref and commit both; then commit a push into a store whose budget has no
/// room left. Expect: the second create refused `Moved` (its ref moved since it was admitted); the commit past the
/// budget refused by the budget, with no row and no version written and the push still holding its objects, and the
/// store back as it was once the push is abandoned.
#[test]
fn a_stale_move_and_a_commit_past_the_budget_change_nothing() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let [first, second, ..] = line_and_branch(format);
  let mut one = sealed(&repository, &mut store, first.clone());
  let mut two = sealed(&repository, &mut store, first.clone());
  let move_one = repository
    .admit(&store, &one, 1, Some(first[0].id))
    .unwrap();
  let move_two = repository
    .admit(&store, &two, 1, Some(first[0].id))
    .unwrap();
  repository
    .commit(&mut store, &mut one, &[move_one])
    .unwrap();
  let stale = repository.commit(&mut store, &mut two, &[move_two]);
  assert_eq!(stale, Err(StoreRefused::Moved { green: 1 }));
  two.abandon(&mut store).unwrap();
  let used = store.used();
  let mut tight = sealed(&repository, &mut store, second.clone());
  let advance = repository
    .admit(&store, &tight, 1, Some(second[0].id))
    .unwrap();
  store.set_budget(store.used());
  let refused = repository.commit(&mut store, &mut tight, &[advance]);
  assert!(matches!(
    refused,
    Err(StoreRefused::Content(_) | StoreRefused::Index(_))
  ));
  assert_eq!(
    (
      repository.versions(1),
      repository.object(&store, &second[0].id)
    ),
    (1, None)
  );
  assert_eq!(tight.len(), second.len());
  tight.abandon(&mut store).unwrap();
  assert_eq!(store.used(), used);
  store.set_budget(BUDGET);
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}
