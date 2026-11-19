//! The git object store held to a map model over generated histories (A-614, §4.17.2 piece 5 part 1): commits made on
//! a ref's head, on any earlier commit (a rewrite) or as roots, refs moved to other refs' heads, deletes and refused
//! pushes, in both object formats. The model is a map from each object to what it names; reachability is its plain
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

use super::{EXCHANGE, Made, Ticks, blob, commit, ids, push_and_move, sealed, store, tree};

/// Shape: the paths the generated histories edit: two at the top, two in `d`, one in `d/e`, one in `f`.
const PATHS: [&[u8]; 6] = [b"a", b"b", b"d/x", b"d/y", b"d/e/z", b"f/w"];

/// Shape: what a file may hold: few contents, so commits, trees and refs share objects.
const CONTENTS: [&[u8]; 3] = [b"one\n", b"two\n", b"three\n"];

/// Shape: the refs the histories move.
const GREENS: u64 = 3;

/// What a new commit is made on.
#[derive(Clone, Copy, Debug)]
enum Base {
  /// The ref's head: a fast-forward.
  Head,
  /// Any commit made so far: a rewrite, or a fast-forward when it is the head.
  Commit(usize),
  /// Nothing: a root.
  Root,
}

#[derive(Clone, Debug)]
enum Op {
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
}

fn op() -> impl Strategy<Value = Op> {
  let edit = (0..PATHS.len(), proptest::option::of(0..CONTENTS.len()));
  (
    (0u8..9, 0..GREENS, 0..GREENS, 0u8..5, any::<usize>()),
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
        _ => Op::Delete { green },
      }
    })
}

/// The model (module docs).
struct Model {
  format: ObjectFormat,
  objects: BTreeMap<ObjectId, Made>,
  /// What each object names: a tree's entries, a commit's tree and then its parents.
  names: BTreeMap<ObjectId, Vec<ObjectId>>,
  generation: BTreeMap<ObjectId, u64>,
  dates: BTreeMap<ObjectId, u64>,
  /// Each commit's files: path to blob.
  files: BTreeMap<ObjectId, BTreeMap<Vec<u8>, ObjectId>>,
  commits: Vec<ObjectId>,
  indexed: BTreeSet<ObjectId>,
  /// Each ref's versions' heads.
  heads: BTreeMap<u64, Vec<Option<ObjectId>>>,
  seconds: u64,
}

impl Model {
  fn new(format: ObjectFormat) -> Model {
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

  fn commit_at(&self, at: usize) -> Option<ObjectId> {
    (!self.commits.is_empty()).then(|| self.commits[at % self.commits.len()])
  }

  /// Everything `head` reaches: the closure of what objects name.
  fn reach(&self, head: Option<ObjectId>) -> BTreeSet<ObjectId> {
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

  /// D(k): what version `version` of `green` reaches and version `version - 1` does not.
  fn brought(&self, green: u64, version: usize) -> Vec<ObjectId> {
    let heads = &self.heads[&green];
    let before = version
      .checked_sub(1)
      .map_or_else(BTreeSet::new, |previous| self.reach(heads[previous]));
    self
      .reach(heads[version])
      .difference(&before)
      .copied()
      .collect()
  }
}

/// The real side.
struct Real {
  store: ContentStore,
  repository: Repository,
  clock: Ticks,
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
    Base::Head => model.head(green),
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
  push_and_move(
    &mut real.repository,
    &mut real.store,
    objects,
    &[(green, Some(head))],
  )
  .unwrap();
  model.heads.entry(green).or_default().push(Some(head));
}

fn at_op(real: &mut Real, model: &mut Model, green: u64, other: u64) {
  let Some(head) = model.head(other).filter(|_| green != other) else {
    return;
  };
  push_and_move(
    &mut real.repository,
    &mut real.store,
    Vec::new(),
    &[(green, Some(head))],
  )
  .unwrap();
  model.heads.entry(green).or_default().push(Some(head));
}

fn delete_op(real: &mut Real, model: &mut Model, green: u64) {
  if model.head(green).is_none() {
    return;
  }
  push_and_move(
    &mut real.repository,
    &mut real.store,
    Vec::new(),
    &[(green, None)],
  )
  .unwrap();
  model.heads.entry(green).or_default().push(None);
}

fn apply(real: &mut Real, model: &mut Model, op: &Op) {
  match op {
    Op::Commit {
      green,
      base,
      edits,
      refuse,
    } => commit_op(real, model, *green, *base, edits, *refuse),
    Op::At { green, other } => at_op(real, model, *green, *other),
    Op::Delete { green } => delete_op(real, model, *green),
  }
  let green = match op {
    Op::Commit { green, .. } | Op::At { green, .. } | Op::Delete { green } => *green,
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

/// Every green's versions read back through A-608's reader from one pack of all their fragments:
/// the objects of their D(k)s, each with the bytes the model made. A pack that spans a rewrite may
/// hold an object twice (a version brought it again); the reader takes both.
fn check_packs(real: &Real, model: &Model) {
  for (green, heads) in &model.heads {
    let last = heads.len() - 1;
    let pack = real
      .repository
      .pack(&real.store, *green, (0, last))
      .unwrap();
    let unpacked =
      read_whole(&pack, model.format, &NoIndex, &mut Admission::new(u64::MAX)).unwrap();
    let read: BTreeMap<ObjectId, Vec<u8>> = unpacked
      .objects
      .into_iter()
      .map(|object| (object.id, object.content))
      .collect();
    let expected: BTreeSet<ObjectId> = (0..heads.len())
      .flat_map(|version| model.brought(*green, version))
      .collect();
    assert_eq!(read.keys().copied().collect::<BTreeSet<_>>(), expected);
    assert!(
      read
        .iter()
        .all(|(id, content)| model.objects[id].content == *content)
    );
  }
}

/// A ref's versions read as the model's: each head and D(k), and the reached set its head's closure.
fn check_green(real: &Real, model: &Model, green: u64) {
  let heads = model.heads.get(&green).cloned().unwrap_or_default();
  assert_eq!(real.repository.versions(green), heads.len());
  for (version, head) in heads.iter().enumerate() {
    assert_eq!(real.repository.head(&real.store, green, version), *head);
    let brought = real.repository.brought(green, version).unwrap();
    assert_eq!(
      ids(&real.repository, &real.store, brought),
      model.brought(green, version)
    );
  }
  if let Some(last) = heads.last() {
    let reached = real.repository.reached(green).unwrap();
    let expected: Vec<ObjectId> = model.reach(*last).into_iter().collect();
    assert_eq!(ids(&real.repository, &real.store, reached), expected);
  }
}

/// The head version's root tree and every path in it read as the model's files.
fn check_paths(real: &Real, model: &Model, green: u64) {
  let Some(head) = model.head(green) else {
    return;
  };
  let version = model.heads[&green].len() - 1;
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

  /// A-614, §4.17.2 items 2 and 3. Do: run generated histories over three refs in either object format: commits on a
  /// ref's head, on any earlier commit and as roots, refs moved to other refs' heads, deletes, and pushes admitted then
  /// abandoned. Expect: after every step every version's head and D(k), every reached set, every commit's row and every
  /// path at every head as the map model computes them; no row for an object no push committed; an abandoned push
  /// changing no byte of the store; and the store empty once the repository is released.
  #[test]
  fn the_git_store_reads_as_its_map_model_over_every_history(sha256 in any::<bool>(), ops in proptest::collection::vec(op(), 1..24)) {
    let format = if sha256 { ObjectFormat::Sha256 } else { ObjectFormat::Sha1 };
    let mut real = Real { store: store(), repository: Repository::new(format), clock: Ticks(0) };
    let mut model = Model::new(format);
    for op in &ops {
      apply(&mut real, &mut model, op);
      check(&real, &model);
    }
    let Real { mut store, repository, .. } = real;
    repository.release(&mut store).unwrap();
    prop_assert!(store.is_empty());
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
