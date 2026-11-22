//! The git object store held to a map model over generated histories (A-614, §4.17.2 piece 5 part 1): commits made on
//! a ref's head, on any earlier commit (a rewrite) or as roots, refs moved to other refs' heads, tags at any earlier
//! commit (annotated, a tag object pushed, or lightweight, the ref at the commit: A-614 amended, a create admitted with
//! no walk and derived on first read), deletes and refused pushes, in both object formats, and the store made again from
//! its pushes as a restart's replay makes it. The model is a map from each object to what it names; reachability is its plain
//! closure, D(k) the difference of two closures, generation numbers its own recursion. After every step: each ref's
//! versions, heads and D(k), its reached set, every commit's row, no row for an object never committed, and every path
//! at each ref's head read as the model's files say.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use slates_vcs::ContentStore;

use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::tree::Mode;
use crate::store::{Found, Repository};
use crate::unpack::{NoIndex, read_whole};

use super::{
  EXCHANGE, Made, Ticks, blob, brought_by, commit, ids, made, pack_of, push_and_move, sealed,
  store, tree,
};

/// Shape: the paths the generated histories edit: two at the top, two in `d`, one in `d/e`, one in `f`.
const PATHS: [&[u8]; 6] = [b"a", b"b", b"d/x", b"d/y", b"d/e/z", b"f/w"];

/// Shape: what a file may hold: few contents, so commits, trees and refs share objects.
const CONTENTS: [&[u8]; 3] = [b"one\n", b"two\n", b"three\n"];

/// Shape: the refs the histories move.
pub(super) const GREENS: u64 = 3;

/// What a new commit is made on.
#[derive(Clone, Copy, Debug)]
pub(super) enum Base {
  /// The ref's head: a fast-forward.
  Head,
  /// Any commit made so far: a rewrite, or a fast-forward when it is the head.
  Commit(usize),
  /// Nothing: a root.
  Root,
}

#[derive(Clone, Debug)]
pub(super) enum Op {
  /// A new commit on `base`, each edit setting a path to a content or removing it, pushed and the ref moved to it;
  /// `refuse` admits the push and abandons it instead.
  Commit {
    green: u64,
    base: Base,
    edits: Vec<(usize, Option<usize>)>,
    refuse: bool,
  },
  /// `green` moved to `other`'s head, or made there, with nothing pushed.
  At { green: u64, other: u64 },
  /// `green` deleted.
  Delete { green: u64 },
  /// `green` moved to, or made at, a tag of a commit made so far (any of them, so often an old one): an annotated tag
  /// object pushed, or the commit itself where `annotated` is false.
  Tag {
    green: u64,
    at: usize,
    annotated: bool,
  },
}

pub(super) fn op() -> impl Strategy<Value = Op> {
  let edit = (0..PATHS.len(), proptest::option::of(0..CONTENTS.len()));
  (
    (0u8..11, 0..GREENS, 0..GREENS, 0u8..5, any::<usize>()),
    (
      proptest::collection::vec(edit, 0..4),
      proptest::bool::weighted(0.15),
    ),
  )
    .prop_map(|((selector, green, other, base, at), (edits, refuse))| {
      let base = match base {
        0..3 => Base::Head,
        3 => Base::Commit(at),
        _ => Base::Root,
      };
      match selector {
        0..6 => Op::Commit {
          green,
          base,
          edits,
          refuse,
        },
        6..8 => Op::At { green, other },
        8 => Op::Delete { green },
        _ => Op::Tag {
          green,
          at,
          annotated: refuse || at % 2 == 0,
        },
      }
    })
}

/// The model (module docs).
pub(super) struct Model {
  pub(super) format: ObjectFormat,
  pub(super) objects: BTreeMap<ObjectId, Made>,
  /// What each object names: a tree's entries, a commit's tree and then its parents.
  names: BTreeMap<ObjectId, Vec<ObjectId>>,
  generation: BTreeMap<ObjectId, u64>,
  dates: BTreeMap<ObjectId, u64>,
  /// Each commit's files: path to blob.
  files: BTreeMap<ObjectId, BTreeMap<Vec<u8>, ObjectId>>,
  commits: Vec<ObjectId>,
  indexed: BTreeSet<ObjectId>,
  /// Each ref's versions' heads.
  pub(super) heads: BTreeMap<u64, Vec<Option<ObjectId>>>,
  /// What each ref's first version (no version before it with a head) takes from the version it was made at (A-850):
  /// that version's reach, less its own tags where its head is not the first version's.
  bases: BTreeMap<(u64, usize), BTreeSet<ObjectId>>,
  /// For each commit a version's head peels to, the first such version, in the order the versions were made.
  peeled: BTreeMap<ObjectId, (u64, usize)>,
  seconds: u64,
}

impl Model {
  pub(super) fn new(format: ObjectFormat) -> Model {
    Model {
      format,
      objects: BTreeMap::new(),
      names: BTreeMap::new(),
      generation: BTreeMap::new(),
      dates: BTreeMap::new(),
      files: BTreeMap::new(),
      commits: Vec::new(),
      indexed: BTreeSet::new(),
      heads: BTreeMap::new(),
      bases: BTreeMap::new(),
      peeled: BTreeMap::new(),
      seconds: 0,
    }
  }

  fn record(&mut self, made: Made, names: Vec<ObjectId>) -> ObjectId {
    let id = made.id;
    self.names.entry(id).or_insert(names);
    self.objects.entry(id).or_insert(made);
    id
  }

  /// The tree of `files` (paths relative to it), each subtree made first.
  fn tree(&mut self, files: &BTreeMap<Vec<u8>, ObjectId>) -> ObjectId {
    let mut entries: Vec<(Mode, Vec<u8>, ObjectId)> = Vec::new();
    let mut directories: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, ObjectId>> = BTreeMap::new();
    for (path, id) in files {
      match path.iter().position(|byte| *byte == b'/') {
        Some(at) => {
          let inner = directories.entry(path[..at].to_vec()).or_default();
          inner.insert(path[at + 1..].to_vec(), *id);
        }
        None => entries.push((Mode::File, path.clone(), *id)),
      }
    }
    for (name, inner) in directories {
      let id = self.tree(&inner);
      entries.push((Mode::Directory, name, id));
    }
    let borrowed: Vec<(Mode, &[u8], ObjectId)> = entries
      .iter()
      .map(|(mode, name, id)| (*mode, name.as_slice(), *id))
      .collect();
    let made = tree(self.format, &borrowed);
    let names = entries.iter().map(|(_, _, id)| *id).collect();
    self.record(made, names)
  }

  /// A new commit on `base` with `edits` applied to its files.
  fn make_commit(&mut self, base: Option<ObjectId>, edits: &[(usize, Option<usize>)]) -> ObjectId {
    let mut files = base
      .and_then(|base| self.files.get(&base).cloned())
      .unwrap_or_default();
    for (path, content) in edits {
      let path = PATHS[*path].to_vec();
      match content {
        Some(content) => {
          let id = self.record(blob(self.format, CONTENTS[*content]), Vec::new());
          files.insert(path, id);
        }
        None => {
          files.remove(&path);
        }
      }
    }
    let root = self.tree(&files);
    self.seconds += 1;
    let parents: Vec<ObjectId> = base.into_iter().collect();
    let generation = parents
      .iter()
      .map(|parent| self.generation[parent])
      .max()
      .unwrap_or(0)
      + 1;
    let mut names = vec![root];
    names.extend(&parents);
    let id = self.record(commit(self.format, root, &parents, self.seconds), names);
    self.generation.insert(id, generation);
    self.dates.insert(id, self.seconds);
    self.files.insert(id, files);
    self.commits.push(id);
    id
  }

  fn head(&self, green: u64) -> Option<ObjectId> {
    self
      .heads
      .get(&green)
      .and_then(|heads| heads.last().copied().flatten())
  }

  /// The commit `id` peels to: itself, or the one a tag names.
  fn peel(&self, id: ObjectId) -> ObjectId {
    match self.objects.get(&id) {
      Some(made) if made.kind == Kind::Tag => self.names[&id][0],
      _ => id,
    }
  }

  /// An annotated tag of `target`, a commit, recorded.
  fn make_tag(&mut self, target: ObjectId) -> ObjectId {
    self.seconds += 1;
    let text = format!(
      "object {target}\ntype commit\ntag t{}\ntagger A <a@example.com> {} +0000\n\nm\n",
      self.seconds, self.seconds
    );
    self.record(
      made(self.format, Kind::Tag, text.into_bytes()),
      vec![target],
    )
  }

  fn commit_at(&self, at: usize) -> Option<ObjectId> {
    (!self.commits.is_empty()).then(|| self.commits[at % self.commits.len()])
  }

  /// Everything `head` reaches: the closure of what objects name.
  pub(super) fn reach(&self, head: Option<ObjectId>) -> BTreeSet<ObjectId> {
    let mut reached = BTreeSet::new();
    let mut pending: Vec<ObjectId> = head.into_iter().collect();
    while let Some(id) = pending.pop() {
      if reached.insert(id) {
        pending.extend(self.names[&id].iter().copied());
      }
    }
    reached
  }

  /// What `head` reaches that the repository does not hold: the objects a client's pack brings.
  fn unindexed(&self, head: ObjectId) -> Vec<Made> {
    self
      .reach(Some(head))
      .difference(&self.indexed)
      .map(|id| self.objects[id].clone())
      .collect()
  }

  /// `green`'s next version, at `head` (none: a delete), noted with what it takes from the version it was made at where
  /// it is a first version (no version before it with a head), and noted under the commit its head peels to (A-850).
  fn note_version(&mut self, green: u64, head: Option<ObjectId>) {
    let versions = self.heads.entry(green).or_default();
    let version = versions.len();
    let first = versions.last().is_none_or(Option::is_none);
    if let Some(head) = head.filter(|_| first)
      && let Some(base) = self.base_of(green, head)
    {
      self.bases.insert((green, version), base);
    }
    self.heads.entry(green).or_default().push(head);
    if let Some(head) = head {
      let commit = self.peel(head);
      if self.generation.contains_key(&commit) {
        self.peeled.entry(commit).or_insert((green, version));
      }
    }
  }

  /// What a first version of `green` at `head` takes from the version it is made at, by the design's rule (A-850):
  /// another ref's version whose head is `head`, taken whole; else one whose head is the commit `head`'s tag names;
  /// else, back from that commit along its parents, the first commit a version peels to (of any ref, this one's earlier
  /// versions too), the first such version, less its own tags. None where no version is found.
  fn base_of(&self, green: u64, head: ObjectId) -> Option<BTreeSet<ObjectId>> {
    let heading = |id: ObjectId| {
      self
        .heads
        .iter()
        .any(|(other, heads)| *other != green && heads.last().copied().flatten() == Some(id))
    };
    if heading(head) {
      return Some(self.reach(Some(head)));
    }
    let commit = self.peel(head);
    if commit != head && heading(commit) {
      return Some(self.reach(Some(commit)));
    }
    let mut at = Some(commit).filter(|commit| self.generation.contains_key(commit));
    while let Some(id) = at {
      if let Some((other, version)) = self.peeled.get(&id) {
        let base_head = self.heads[other][*version];
        let tags: BTreeSet<ObjectId> = base_head
          .filter(|base_head| self.peel(*base_head) != *base_head)
          .into_iter()
          .collect();
        return Some(self.reach(base_head).difference(&tags).copied().collect());
      }
      // A commit's names are its tree, then its parents: a model commit has one at most.
      at = self.names[&id].get(1).copied();
    }
    None
  }

  /// D(k): what version `version` of `green` reaches and version `version - 1` does not; for a first version made at
  /// another ref's version, what it reaches and that version does not (A-850).
  fn brought(&self, green: u64, version: usize) -> Vec<ObjectId> {
    let heads = &self.heads[&green];
    let before = match self.bases.get(&(green, version)) {
      Some(base) => base.clone(),
      None => version
        .checked_sub(1)
        .map_or_else(BTreeSet::new, |previous| self.reach(heads[previous])),
    };
    self
      .reach(heads[version])
      .difference(&before)
      .copied()
      .collect()
  }
}

/// A push committed: its objects, and its moves (each ref's position and where it moved to).
pub(super) type CommittedPush = (Vec<Made>, Vec<(u64, Option<ObjectId>)>);

/// The real side.
pub(super) struct Real {
  pub(super) store: ContentStore,
  pub(super) repository: Repository,
  pub(super) clock: Ticks,
  /// Every push committed, in order: its objects and its moves, what a restart's replay commits again.
  pub(super) pushes: Vec<CommittedPush>,
}

impl Real {
  /// An empty repository of `format` in an empty store.
  pub(super) fn new(format: ObjectFormat) -> Real {
    Real {
      store: store(),
      repository: Repository::new(format),
      clock: Ticks(0),
      pushes: Vec::new(),
    }
  }

  /// `objects` pushed and `moves` committed ([`push_and_move`]), and kept for a replay.
  fn pushed(&mut self, objects: Vec<Made>, moves: &[(u64, Option<ObjectId>)]) {
    self.pushes.push((objects.clone(), moves.to_vec()));
    push_and_move(&mut self.repository, &mut self.store, objects, moves).unwrap();
  }

  /// The store made again from the pushes this one committed, in order, as a restart's replay makes it (A-614
  /// amended): each sealed, admitted and committed with no reader between, so no set admitted with no walk is derived;
  /// and the objects its admissions walked meanwhile.
  pub(super) fn replayed(&self, format: ObjectFormat) -> (Real, u64) {
    let mut again = Real::new(format);
    for (objects, moves) in &self.pushes {
      let mut push = sealed(&again.repository, &mut again.store, objects.clone());
      let admitted: Vec<crate::store::Move> = moves
        .iter()
        .map(|(green, to)| {
          again
            .repository
            .admit(&again.store, &push, *green, *to)
            .unwrap()
        })
        .collect();
      again
        .repository
        .commit(&mut again.store, &mut push, &admitted)
        .unwrap();
    }
    let walked = again.repository.counters().walked;
    again.pushes = self.pushes.clone();
    (again, walked)
  }
}

fn commit_op(
  real: &mut Real,
  model: &mut Model,
  green: u64,
  base: Base,
  edits: &[(usize, Option<usize>)],
  refuse: bool,
) {
  let base = match base {
    Base::Head => model.head(green).map(|head| model.peel(head)),
    Base::Commit(at) => model.commit_at(at),
    Base::Root => None,
  };
  let head = model.make_commit(base, edits);
  let objects = model.unindexed(head);
  if refuse {
    let used = real.store.used();
    let push = sealed(&real.repository, &mut real.store, objects);
    real
      .repository
      .admit(&real.store, &push, green, Some(head))
      .unwrap();
    push.abandon(&mut real.store).unwrap();
    assert_eq!(real.store.used(), used);
    return;
  }
  model.indexed.extend(objects.iter().map(|object| object.id));
  real.pushed(objects, &[(green, Some(head))]);
  model.note_version(green, Some(head));
}

fn at_op(real: &mut Real, model: &mut Model, green: u64, other: u64) {
  let Some(head) = model.head(other).filter(|_| green != other) else {
    return;
  };
  real.pushed(Vec::new(), &[(green, Some(head))]);
  model.note_version(green, Some(head));
}

fn tag_op(real: &mut Real, model: &mut Model, green: u64, at: usize, annotated: bool) {
  let Some(commit) = model.commit_at(at) else {
    return;
  };
  let head = if annotated {
    model.make_tag(commit)
  } else {
    commit
  };
  let objects = model.unindexed(head);
  model.indexed.extend(objects.iter().map(|object| object.id));
  real.pushed(objects, &[(green, Some(head))]);
  model.note_version(green, Some(head));
}

fn delete_op(real: &mut Real, model: &mut Model, green: u64) {
  if model.head(green).is_none() {
    return;
  }
  real.pushed(Vec::new(), &[(green, None)]);
  model.note_version(green, None);
}

pub(super) fn apply(real: &mut Real, model: &mut Model, op: &Op) {
  match op {
    Op::Commit {
      green,
      base,
      edits,
      refuse,
    } => commit_op(real, model, *green, *base, edits, *refuse),
    Op::At { green, other } => at_op(real, model, *green, *other),
    Op::Delete { green } => delete_op(real, model, *green),
    Op::Tag {
      green,
      at,
      annotated,
    } => tag_op(real, model, *green, *at, *annotated),
  }
  let green = match op {
    Op::Commit { green, .. }
    | Op::At { green, .. }
    | Op::Delete { green }
    | Op::Tag { green, .. } => *green,
  };
  if let Some(version) = real.repository.versions(green).checked_sub(1) {
    let encoded = real.repository.encode_fragments(
      &mut real.store,
      (green, version),
      &mut real.clock,
      EXCHANGE,
    );
    assert_eq!(encoded, Ok(()));
  }
}

/// Every green's versions read back through A-608's reader from the pack of all their D(k)s, which
/// A-639's stream answers: each object of their union once, a rewrite's repeat sent once, with the
/// bytes the model made.
fn check_packs(real: &Real, model: &Model) {
  for (green, heads) in &model.heads {
    let all = brought_by(&real.repository, &real.store, *green, 0..heads.len());
    let pack = pack_of(&real.repository, &real.store, &all);
    let unpacked =
      read_whole(&pack, model.format, &NoIndex, &mut Admission::new(u64::MAX)).unwrap();
    let expected: BTreeSet<ObjectId> = (0..heads.len())
      .flat_map(|version| model.brought(*green, version))
      .collect();
    assert_eq!(unpacked.objects.len(), expected.len());
    let read: BTreeMap<ObjectId, Vec<u8>> = unpacked
      .objects
      .into_iter()
      .map(|object| (object.id, object.content))
      .collect();
    assert_eq!(read.keys().copied().collect::<BTreeSet<_>>(), expected);
    assert!(
      read
        .iter()
        .all(|(id, content)| model.objects[id].content == *content)
    );
  }
}

/// A ref's versions read as the model's, every one of them (A-647 part 3: each version serves its sets from its own
/// value): its head, its D(k), and its reached set, its head's closure.
fn check_green(real: &Real, model: &Model, green: u64) {
  let heads = model.heads.get(&green).cloned().unwrap_or_default();
  assert_eq!(real.repository.versions(green), heads.len());
  for (version, head) in heads.iter().enumerate() {
    assert_eq!(real.repository.head(&real.store, green, version), *head);
    let brought = real.repository.brought(&real.store, green, version);
    assert_eq!(
      ids(&real.repository, &real.store, &brought.unwrap()),
      model.brought(green, version)
    );
    let reached = real.repository.reached(&real.store, green, version);
    let expected: Vec<ObjectId> = model.reach(*head).into_iter().collect();
    assert_eq!(
      reached.map(|reached| ids(&real.repository, &real.store, &reached)),
      Ok(expected),
      "green {green} version {version}"
    );
  }
}

/// The head version's root tree and every path in it read as the model's files.
fn check_paths(real: &Real, model: &Model, green: u64) {
  let Some(head) = model.head(green) else {
    return;
  };
  let version = model.heads[&green].len() - 1;
  // A tag's head has no tree of its own: a version's tree is its head commit's.
  if model
    .objects
    .get(&head)
    .is_some_and(|made| made.kind == Kind::Tag)
  {
    assert_eq!(real.repository.tree(&real.store, green, version), None);
    return;
  }
  assert_eq!(
    real.repository.tree(&real.store, green, version),
    Some(model.names[&head][0])
  );
  let files = &model.files[&head];
  for path in PATHS {
    let found = real
      .repository
      .entry(&real.store, green, version, path)
      .unwrap();
    let expected = files.get(path).map(|id| Found {
      mode: Mode::File.bits(),
      id: *id,
    });
    assert_eq!(found, expected);
  }
}

/// Every committed object's row as made, a commit's with the model's tree, parents, date and generation; and no row for
/// an object never committed.
fn check_rows(real: &Real, model: &Model) {
  for (id, made) in &model.objects {
    let row = real.repository.object(&real.store, id);
    assert_eq!(row.is_some(), model.indexed.contains(id));
    let Some(row) = row else {
      continue;
    };
    let len = u64::try_from(made.content.len()).unwrap();
    assert_eq!((row.kind, row.len), (made.kind, len));
    if let Some(commit) = row.commit {
      let at = |position: u64| real.repository.id_at(&real.store, position).unwrap();
      let parents: Vec<ObjectId> = commit.parents.iter().map(|parent| at(*parent)).collect();
      assert_eq!(
        (at(commit.tree), parents),
        (model.names[id][0], model.names[id][1..].to_vec())
      );
      assert_eq!(
        (commit.date, commit.generation),
        (model.dates[id], model.generation[id])
      );
    }
  }
}

fn check(real: &Real, model: &Model) {
  for green in 0..GREENS {
    check_green(real, model, green);
    check_paths(real, model, green);
  }
  check_rows(real, model);
  check_packs(real, model);
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(64), ..ProptestConfig::default() }))]

  /// A-614, §4.17.2 items 2 and 3; A-614 amended (main's ruling (A) of 2026-11-21, its condition 1: on generated
  /// histories with branches, lightweight and annotated tags and tags at old commits, a set derived on first read is the
  /// eager walk's, before and after a restart). Do: run generated histories over three refs in either object format:
  /// commits on a ref's head, on any earlier commit and as roots, refs moved to other refs' heads, tags (annotated or
  /// lightweight) at any earlier commit, deletes, and pushes admitted then abandoned, each version's sets derived on
  /// first read; then make the store again from its pushes as a restart's replay does, and derive every version there.
  /// Expect: after every step every version's head and D(k), every reached set (each its head's closure), every
  /// commit's row and every path at every head as the map model computes them; no row for an object no push committed;
  /// an abandoned push changing no byte of the store; the replay walking nothing, and the store it made reading as the
  /// model too; and each store empty once its repository is released.
  #[test]
  fn the_git_store_reads_as_its_map_model_over_every_history(sha256 in any::<bool>(), ops in proptest::collection::vec(op(), 1..24)) {
    let format = if sha256 { ObjectFormat::Sha256 } else { ObjectFormat::Sha1 };
    let mut real = Real::new(format);
    let mut model = Model::new(format);
    for op in &ops {
      apply(&mut real, &mut model, op);
      check(&real, &model);
    }
    let (mut again, walked) = real.replayed(format);
    prop_assert_eq!(walked, 0, "a replay admits every move with no walk");
    crate::served::derive_all(&again.repository, &mut again.store);
    check(&again, &model);
    for Real { mut store, repository, .. } in [real, again] {
      repository.release(&mut store).unwrap();
      prop_assert!(store.is_empty());
    }
  }
}

/// The kinds the generated histories make, so a history with no commit is not mistaken for a check.
#[test]
fn the_models_kinds_are_gits_four() {
  let format = ObjectFormat::Sha1;
  let mut model = Model::new(format);
  let head = model.make_commit(None, &[(4, Some(0))]);
  let kinds: BTreeSet<Kind> = model
    .reach(Some(head))
    .iter()
    .map(|id| model.objects[id].kind)
    .collect();
  assert_eq!(
    kinds,
    BTreeSet::from([Kind::Commit, Kind::Tree, Kind::Blob])
  );
  assert_eq!(model.reach(Some(head)).len(), 5);
}
