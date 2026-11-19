//! A-666 part 2 held to its definition (module docs): every answer equals git's default-mode walk over generated
//! histories with renames, copies, removals and merges, newest first; every derivation judges exactly its change's paths
//! and the directories on them; every question reads at most one node more than the versions it lists; a value is the
//! same however its history was filled; the format's bytes; hostile bytes and every refusal, typed.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::TestRunner;

use super::{
  HistoryRefused, LogCost, Logged, Made, Node, PathHistory, PathSources, Sources, decode, key, log,
  read_value,
};
use crate::content::{ContentId, ContentRefused, ContentStore};
use crate::derived::paths::{Change, Entry, PathChanges, PathMap, PathsRefused};
use crate::derived::{Derivations, DerivedType, Fill, Graph, Mode, Progress, Read};

/// Shape: the store's budget in these tests: room for every history and question they make.
const BUDGET: u64 = 1 << 30;

/// Shape: the store's window and the type's: every directory of these histories fits one leaf, so a lookup reads one
/// trie node a name of the path's key.
const WINDOW: usize = 1024;

/// Shape: the distinct contents the files hold.
const BLOBS: usize = 6;

/// Shape: the most versions a generated history holds.
const MAX_VERSIONS: usize = 240;

/// Shape: the most branches a generated history keeps open.
const MAX_TIPS: usize = 6;

/// Shape: the longest run of edits to the hot file one step makes: long against a cut question's limit, so a walk that
/// read the whole history before cutting would pass the cut bound.
const HOT_RUN: usize = 24;

/// Shape: the most parents a generated version has: an octopus merge of three.
const MAX_PARENTS: u64 = 3;

/// Shape: the first-parent steps back a generated copy may take its source from.
const COPY_BACK: usize = 6;

/// Shape: the hot file, changed in long runs.
const HOT: &[u8] = b"h";

/// Shape: the long history's versions; the hot file changes in one of every [`HOT_EVERY`] of them.
const LONG_HISTORY: u64 = 20_000;

/// Shape: the long history's stride between the hot file's changes.
const HOT_EVERY: u64 = 20;

/// A history: each version's parents, change and renames and copies, versions numbered in the order they were made.
#[derive(Clone, Debug, Default)]
struct History {
  parents: Vec<Vec<u64>>,
  changes: Vec<Change>,
  sources: Vec<Sources<u64>>,
}

impl Graph for History {
  type Version = u64;

  fn parents(&self, version: u64) -> Option<Vec<u64>> {
    self.parents.get(usize::try_from(version).ok()?).cloned()
  }

  fn name(&self, version: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&version.to_be_bytes());
  }
}

impl PathChanges for History {
  fn change(&self, version: u64) -> Option<&Change> {
    self.changes.get(usize::try_from(version).ok()?)
  }
}

impl PathSources for History {
  fn sources(&self, version: u64) -> Option<&Sources<u64>> {
    self.sources.get(usize::try_from(version).ok()?)
  }
}

impl History {
  fn len(&self) -> usize {
    self.parents.len()
  }
}

/// A version's index.
fn index(version: u64) -> usize {
  usize::try_from(version).unwrap()
}

/// A version's number.
fn number(index: usize) -> u64 {
  u64::try_from(index).unwrap()
}

/// The version a logged entry's name bytes name.
fn version_of(name: &[u8]) -> usize {
  index(u64::from_be_bytes(name.try_into().unwrap()))
}

/// The bytes of blob `at`.
fn blob(at: usize) -> Vec<u8> {
  format!("blob {at}").into_bytes()
}

/// A file of blob `at`, class zero.
fn file(at: usize) -> Entry {
  Entry {
    class: 0,
    content: ContentId::of(&blob(at % BLOBS)),
  }
}

/// A version's files by path.
type Files = BTreeMap<Vec<u8>, Entry>;

/// `rest` beneath `prefix` (either may be empty).
fn join(prefix: &[u8], rest: &[u8]) -> Vec<u8> {
  match (prefix.is_empty(), rest.is_empty()) {
    (true, _) => rest.to_vec(),
    (_, true) => prefix.to_vec(),
    _ => [prefix, b"/", rest].concat(),
  }
}

/// Whether `file` lies beneath the directory `path`; every file lies beneath the root.
fn under(path: &[u8], file: &[u8]) -> bool {
  path.is_empty() || (file.starts_with(path) && file.get(path.len()) == Some(&b'/'))
}

/// `path`'s directories: its proper prefixes.
fn prefixes(path: &[u8]) -> Vec<Vec<u8>> {
  path
    .iter()
    .enumerate()
    .filter(|(_, byte)| **byte == b'/')
    .map(|(at, _)| path[..at].to_vec())
    .collect()
}

/// Removes what stands at `path`: its file, or every file beneath it.
fn remove_tree(map: &mut Files, path: &[u8]) {
  map.retain(|file, _| file != path && !under(path, file));
}

/// Sets `path` to `entry`, removing what stood at it or beneath it and any file at a directory on it.
fn set_file(map: &mut Files, path: &[u8], entry: Entry) {
  remove_tree(map, path);
  for prefix in prefixes(path) {
    map.remove(&prefix);
  }
  map.insert(path.to_vec(), entry);
}

/// The files at or beneath `path`, by their path relative to it (the file at `path` itself: the empty path).
fn subtree(map: &Files, path: &[u8]) -> Vec<(Vec<u8>, Entry)> {
  map
    .iter()
    .filter_map(|(file, entry)| {
      if file == path {
        Some((Vec::new(), *entry))
      } else if under(path, file) {
        Some((file[path.len() + 1..].to_vec(), *entry))
      } else {
        None
      }
    })
    .collect()
}

/// Every file and directory standing in `map`, the root aside.
fn present(map: &Files) -> Vec<Vec<u8>> {
  let mut paths = BTreeSet::new();
  for file in map.keys() {
    paths.extend(prefixes(file));
    paths.insert(file.clone());
  }
  paths.into_iter().collect()
}

/// Whether `path` can be made in `map`: nothing stands at it and no file at a directory on it.
fn free(map: &Files, path: &[u8]) -> bool {
  held(map, path) == Held::Absent
    && prefixes(path)
      .iter()
      .all(|prefix| !map.contains_key(prefix))
}

/// The change from `old` to `new`.
fn diff(old: &Files, new: &Files) -> Change {
  let mut change = Change::new();
  for path in old.keys() {
    if !new.contains_key(path) {
      change.insert(path.clone(), None);
    }
  }
  for (path, entry) in new {
    if old.get(path) != Some(entry) {
      change.insert(path.clone(), Some(*entry));
    }
  }
  change
}

/// The paths from a moved tree's top down to `rest`, relative to the top: the top (empty), each directory on `rest`,
/// and `rest`.
fn steps(rest: &[u8]) -> Vec<Vec<u8>> {
  let mut steps = vec![Vec::new()];
  if !rest.is_empty() {
    steps.extend(prefixes(rest));
    steps.push(rest.to_vec());
  }
  steps
}

/// Every path the generated histories use: names `a`, `b` and `c` down to three deep, and the hot file.
fn universe() -> Vec<Vec<u8>> {
  let names: [&[u8]; 3] = [b"a", b"b", b"c"];
  let mut paths: Vec<Vec<u8>> = names.iter().map(|name| name.to_vec()).collect();
  let mut level = paths.clone();
  for _ in 1..3 {
    let next: Vec<Vec<u8>> = level
      .iter()
      .flat_map(|prefix| names.iter().map(|name| join(prefix, name)))
      .collect();
    paths.extend(next.iter().cloned());
    level = next;
  }
  paths.push(HOT.to_vec());
  paths
}

/// What stands at a path, by definition: nothing, a file, or a directory of the files beneath it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Held {
  Absent,
  File(Entry),
  Directory(Files),
}

/// What stands at `path` in `map`; the root is a directory while a file stands.
fn held(map: &Files, path: &[u8]) -> Held {
  if !path.is_empty()
    && let Some(entry) = map.get(path)
  {
    return Held::File(*entry);
  }
  let beneath: Files = map
    .iter()
    .filter(|(file, _)| under(path, file))
    .map(|(file, entry)| (file.clone(), *entry))
    .collect();
  if beneath.is_empty() {
    Held::Absent
  } else {
    Held::Directory(beneath)
  }
}

/// What a listing says a version left.
fn made_of(held: &Held) -> Made {
  match held {
    Held::Absent => Made::Absent,
    Held::File(entry) => Made::File(*entry),
    Held::Directory(_) => Made::Directory,
  }
}

/// A history being built, with each version's files.
#[derive(Default)]
struct Builder {
  history: History,
  maps: Vec<Files>,
}

impl Builder {
  /// Records a version of `parents` whose files are `map`, its change stated against its first parent's.
  fn record(&mut self, parents: &[u64], map: Files, sources: Sources<u64>) -> u64 {
    let base = parents
      .first()
      .map(|parent| self.maps[index(*parent)].clone())
      .unwrap_or_default();
    self.history.changes.push(diff(&base, &map));
    self.history.parents.push(parents.to_vec());
    self.history.sources.push(sources);
    self.maps.push(map);
    number(self.maps.len() - 1)
  }

  fn map(&self, version: u64) -> Files {
    self.maps[index(version)].clone()
  }

  /// A version on `parent` (or a root) setting each `(path, Some(blob))` and removing each `(path, None)`.
  fn edit(&mut self, parent: Option<u64>, edits: &[(&str, Option<usize>)]) -> u64 {
    let mut map = parent.map(|parent| self.map(parent)).unwrap_or_default();
    for (path, blob) in edits {
      match blob {
        Some(at) => set_file(&mut map, path.as_bytes(), file(*at)),
        None => remove_tree(&mut map, path.as_bytes()),
      }
    }
    self.record(parent.as_slice(), map, Sources::new())
  }

  /// What copying what stood at `from` in `source` to `to` on `parent` makes (a rename when `remove`): the files, and
  /// every path it makes with its source.
  fn brought(
    &self,
    parent: u64,
    (from, source): (&[u8], u64),
    to: &[u8],
    remove: bool,
  ) -> (Files, Sources<u64>) {
    let mut map = self.map(parent);
    if remove {
      remove_tree(&mut map, from);
    }
    let mut sources = Sources::new();
    for (rest, entry) in subtree(&self.map(source), from) {
      map.insert(join(to, &rest), entry);
      for step in steps(&rest) {
        sources.insert(join(to, &step), (join(from, &step), source));
      }
    }
    (map, sources)
  }

  /// A version on `parent` copying what stood at `from` in `source` to `to` (a rename when `remove`), every path it
  /// makes recorded with its source.
  fn bring(&mut self, parent: u64, (from, source): (&[u8], u64), to: &[u8], remove: bool) -> u64 {
    let (map, sources) = self.brought(parent, (from, source), to, remove);
    self.record(&[parent], map, sources)
  }

  /// A merge of `parents` whose files are the first parent's with `edits`.
  fn merge(&mut self, parents: &[u64], edits: &[(&str, Option<usize>)]) -> u64 {
    let mut map = self.map(parents[0]);
    for (path, blob) in edits {
      match blob {
        Some(at) => set_file(&mut map, path.as_bytes(), file(*at)),
        None => remove_tree(&mut map, path.as_bytes()),
      }
    }
    self.record(parents, map, Sources::new())
  }
}

/// A generated step: a selector and three picks (see [`Grower::apply`]).
type Op = (u8, usize, usize, usize);

/// A step, as plain tuples (no `Arc`-backed `prop_oneof!`, R2).
fn op() -> impl Strategy<Value = Op> {
  (0_u8..20, any::<usize>(), any::<usize>(), any::<usize>())
}

/// Grows a history from generated steps: edits anywhere (a file made where a directory stood and back), long runs of
/// edits to the hot file, renames of files and directories (some edited as they move), copies from the first parent and
/// from older versions, merges of two and three with each differing path kept, taken or resolved anew, criss-cross
/// merges, unrelated roots, and removals of whole directories.
struct Grower {
  built: Builder,
  tips: Vec<u64>,
  universe: Vec<Vec<u8>>,
}

impl Grower {
  fn new() -> Grower {
    let mut built = Builder::default();
    let mut first = Files::new();
    first.insert(b"a".to_vec(), file(0));
    first.insert(HOT.to_vec(), file(1));
    let root = built.record(&[], first, Sources::new());
    Grower {
      built,
      tips: vec![root],
      universe: universe(),
    }
  }

  /// A file of a blob and a class the pick chooses.
  fn entry(pick: usize) -> Entry {
    Entry {
      class: u8::try_from(pick / BLOBS % 2).unwrap(),
      content: ContentId::of(&blob(pick % BLOBS)),
    }
  }

  fn room(&self) -> usize {
    MAX_VERSIONS.saturating_sub(self.built.maps.len())
  }

  fn tip(&self, pick: usize) -> (usize, u64) {
    let at = pick % self.tips.len();
    (at, self.tips[at])
  }

  fn pick_path(&self, pick: usize) -> Vec<u8> {
    self.universe[pick % self.universe.len()].clone()
  }

  fn edit(&mut self, one: usize, two: usize, three: usize) {
    let (at, tip) = self.tip(one);
    let mut map = self.built.map(tip);
    let path = self.pick_path(two);
    if map.contains_key(&path) && three % 3 == 0 {
      map.remove(&path);
    } else {
      set_file(&mut map, &path, Grower::entry(three));
    }
    self.tips[at] = self.built.record(&[tip], map, Sources::new());
  }

  fn hot(&mut self, one: usize, length: usize) {
    let (at, mut tip) = self.tip(one);
    for _ in 0..(length % HOT_RUN + 1).min(self.room()) {
      let mut map = self.built.map(tip);
      let next = if map.get(HOT) == Some(&file(2)) {
        file(3)
      } else {
        file(2)
      };
      set_file(&mut map, HOT, next);
      tip = self.built.record(&[tip], map, Sources::new());
    }
    self.tips[at] = tip;
  }

  /// Moves (a rename) or copies `from` in `source` to a free `to` on the tip, when it can be made there; `edit`, when
  /// given, edits the first file it moves in the same version.
  fn bring(
    &mut self,
    (at, tip): (usize, u64),
    (from, source): (&[u8], u64),
    to: &[u8],
    (rename, edit): (bool, Option<Entry>),
  ) {
    let mut next = self.built.map(tip);
    if rename {
      remove_tree(&mut next, from);
    }
    if to == from || under(from, to) || under(to, from) || !free(&next, to) {
      return;
    }
    let (mut map, sources) = self.built.brought(tip, (from, source), to, rename);
    if let Some(entry) = edit
      && let Some(path) = map
        .keys()
        .find(|path| path.as_slice() == to || under(to, path))
        .cloned()
    {
      map.insert(path, entry);
    }
    self.tips[at] = self.built.record(&[tip], map, sources);
  }

  fn rename(&mut self, one: usize, two: usize, three: usize) {
    let (at, tip) = self.tip(one);
    let candidates = present(&self.built.map(tip));
    if candidates.is_empty() {
      return;
    }
    let from = candidates[two % candidates.len()].clone();
    let to = self.pick_path(three);
    let edit = (three % 2 == 1).then(|| Grower::entry(three / 2 + 1));
    self.bring((at, tip), (&from, tip), &to, (true, edit));
  }

  fn copy(&mut self, one: usize, two: usize, three: usize) {
    let (at, tip) = self.tip(one);
    let mut source = tip;
    for _ in 0..two % COPY_BACK {
      match self.built.history.parents[index(source)].first() {
        Some(parent) => source = *parent,
        None => break,
      }
    }
    let candidates = present(&self.built.map(source));
    if candidates.is_empty() {
      return;
    }
    let from = candidates[three % candidates.len()].clone();
    let to = self.pick_path(three / 7);
    self.bring((at, tip), (&from, source), &to, (false, None));
  }

  /// Two distinct open branches at distinct versions, when there are two.
  fn two(&self, one: usize, two: usize) -> Option<(usize, usize)> {
    let count = self.tips.len();
    let first = one % count;
    let second = (first + 1 + two % count.max(2).saturating_sub(1)) % count;
    let distinct = count >= 2 && first != second && self.tips[first] != self.tips[second];
    distinct.then_some((first, second))
  }

  /// A merge's files over `ours`: each path where `theirs` differs kept as ours, taken as theirs, or set to a fresh
  /// file, as `bits` pick, two bits a path.
  fn merged(ours: &Files, theirs: &Files, bits: usize) -> Files {
    let differing: BTreeSet<Vec<u8>> = ours
      .keys()
      .chain(theirs.keys())
      .filter(|path| ours.get(*path) != theirs.get(*path))
      .cloned()
      .collect();
    let mut result = ours.clone();
    for (at, path) in differing.iter().enumerate() {
      match bits.rotate_right(u32::try_from(2 * (at % 32)).unwrap()) & 3 {
        1 => match theirs.get(path) {
          Some(entry) => set_file(&mut result, path, *entry),
          None => remove_tree(&mut result, path),
        },
        2 => set_file(&mut result, path, Grower::entry(bits.wrapping_add(at))),
        _ => {}
      }
    }
    result
  }

  fn merge(&mut self, one: usize, two: usize, three: usize) {
    let Some((first, second)) = self.two(one, two) else {
      return;
    };
    let (ours, theirs) = (self.tips[first], self.tips[second]);
    let map = Grower::merged(&self.built.map(ours), &self.built.map(theirs), three);
    self.tips[first] = self.built.record(&[ours, theirs], map, Sources::new());
    if three % 5 == 0 {
      self.tips.remove(second);
    }
  }

  fn octopus(&mut self, one: usize, three: usize) {
    let count = self.tips.len();
    if count < 3 {
      return;
    }
    let first = one % count;
    let parents: Vec<u64> = (0..3).map(|at| self.tips[(first + at) % count]).collect();
    let distinct: BTreeSet<u64> = parents.iter().copied().collect();
    if distinct.len() < 3 {
      return;
    }
    let two = Grower::merged(
      &self.built.map(parents[0]),
      &self.built.map(parents[1]),
      three,
    );
    let map = Grower::merged(&two, &self.built.map(parents[2]), three / 3);
    self.tips[first] = self.built.record(&parents, map, Sources::new());
  }

  fn criss_cross(&mut self, one: usize, two: usize) {
    if self.room() < 2 {
      return;
    }
    let Some((first, second)) = self.two(one, two) else {
      return;
    };
    let (a, b) = (self.tips[first], self.tips[second]);
    let ab = Grower::merged(&self.built.map(a), &self.built.map(b), one);
    let ba = Grower::merged(&self.built.map(b), &self.built.map(a), two);
    self.tips[first] = self.built.record(&[a, b], ab, Sources::new());
    self.tips[second] = self.built.record(&[b, a], ba, Sources::new());
  }

  fn fork(&mut self, one: usize) {
    if self.tips.len() < MAX_TIPS {
      self.tips.push(number(one % self.built.maps.len()));
    }
  }

  fn root(&mut self, three: usize) {
    if self.tips.len() < MAX_TIPS {
      let mut map = Files::new();
      map.insert(self.pick_path(three), Grower::entry(three));
      let root = self.built.record(&[], map, Sources::new());
      self.tips.push(root);
    }
  }

  fn remove(&mut self, one: usize, two: usize) {
    let (at, tip) = self.tip(one);
    let mut map = self.built.map(tip);
    let candidates = present(&map);
    if candidates.is_empty() {
      return;
    }
    remove_tree(&mut map, &candidates[two % candidates.len()]);
    self.tips[at] = self.built.record(&[tip], map, Sources::new());
  }

  fn apply(&mut self, (kind, one, two, three): Op) {
    if self.room() == 0 {
      return;
    }
    match kind {
      0..=4 => self.edit(one, two, three),
      5 | 6 => self.hot(one, three),
      7 | 8 => self.rename(one, two, three),
      9 | 10 => self.copy(one, two, three),
      11..=13 => self.merge(one, two, three),
      14 => self.octopus(one, three),
      15 => self.criss_cross(one, two),
      16 => self.fork(one),
      17 => self.root(three),
      _ => self.remove(one, two),
    }
  }
}

/// The history the steps grow, with each version's files, and its open branches' tips.
fn grown(ops: &[Op]) -> (Builder, Vec<u64>) {
  let mut grower = Grower::new();
  for op in ops {
    grower.apply(*op);
  }
  (grower.built, grower.tips)
}

/// The naive model, from the history alone: each version's parents, files, sources and generation.
struct Model {
  parents: Vec<Vec<usize>>,
  maps: Vec<Files>,
  sources: Vec<Sources<u64>>,
  generation: Vec<u64>,
}

/// A listing: each version listed, the path it was listed at, and what it left there.
type Listing = Vec<(usize, Vec<u8>, Made)>;

impl Model {
  fn of(built: &Builder) -> Model {
    let parents: Vec<Vec<usize>> = built
      .history
      .parents
      .iter()
      .map(|parents| parents.iter().map(|parent| index(*parent)).collect())
      .collect();
    let mut generation: Vec<u64> = Vec::with_capacity(parents.len());
    for of in &parents {
      let greatest = of.iter().map(|parent| generation[*parent]).max();
      generation.push(greatest.map_or(1, |greatest| greatest + 1));
    }
    Model {
      parents,
      maps: built.maps.clone(),
      sources: built.history.sources.clone(),
      generation,
    }
  }

  fn len(&self) -> usize {
    self.parents.len()
  }

  /// git's default mode walked from `version` at `path` (module docs), continued at each recorded source with `follow`:
  /// every version listed, newest first; and the versions the walk visited.
  fn log(&self, version: usize, path: &[u8], follow: bool) -> (Listing, u64) {
    let mut pending = vec![(version, path.to_vec())];
    let mut seen = BTreeSet::new();
    let mut listed = Listing::new();
    let mut visits = 0_u64;
    while let Some((at, path)) = pending.pop() {
      if !seen.insert((at, path.clone())) {
        continue;
      }
      visits += 1;
      let now = held(&self.maps[at], &path);
      let parents = &self.parents[at];
      if let Some(parent) = parents
        .iter()
        .find(|parent| held(&self.maps[**parent], &path) == now)
      {
        pending.push((*parent, path));
        continue;
      }
      if parents.is_empty() && now == Held::Absent {
        continue;
      }
      pending.extend(parents.iter().map(|parent| (*parent, path.clone())));
      if follow && let Some((from_path, from)) = self.sources[at].get(&path) {
        pending.push((index(*from), from_path.clone()));
      }
      listed.push((at, path, made_of(&now)));
    }
    listed.sort_by(|one, two| {
      (Reverse(self.generation[one.0]), one.0, &one.1).cmp(&(
        Reverse(self.generation[two.0]),
        two.0,
        &two.1,
      ))
    });
    (listed, visits)
  }
}

/// One store, one history's values and a declaration of the type: eager, every version derived as it is recorded, or
/// lazy, read in a given order.
struct Fixture {
  store: ContentStore,
  derivations: Derivations,
  ty: PathHistory<History>,
  roots: Vec<ContentId>,
  blobs: Vec<ContentId>,
}

/// Drives a fill to its end.
fn drive(
  fill: &mut Fill<u64>,
  derivations: &mut Derivations,
  ty: &mut PathHistory<History>,
  history: &History,
  store: &mut ContentStore,
) -> ContentId {
  loop {
    match fill.step(derivations, ty, history, store).unwrap() {
      Progress::More => {}
      Progress::Done(root) => return root,
    }
  }
}

impl Fixture {
  fn empty(mode: Mode) -> Fixture {
    let mut store = ContentStore::new(BUDGET, WINDOW).unwrap();
    let blobs = (0..BLOBS).map(|at| store.put(&blob(at)).unwrap()).collect();
    Fixture {
      store,
      derivations: Derivations::new(),
      ty: PathHistory::declare(WINDOW, mode).unwrap(),
      roots: Vec::new(),
      blobs,
    }
  }

  /// Every version derived as it is recorded, in order.
  fn eager(history: &History) -> Fixture {
    let mut fixture = Fixture::empty(Mode::Eager);
    for version in 0..number(history.len()) {
      let recorded = fixture
        .derivations
        .recorded(&mut fixture.store, &fixture.ty, version);
      let mut fill = recorded.unwrap().unwrap();
      let root = drive(
        &mut fill,
        &mut fixture.derivations,
        &mut fixture.ty,
        history,
        &mut fixture.store,
      );
      fill.release(&mut fixture.store).unwrap();
      fixture.roots.push(root);
    }
    fixture
  }

  /// Every version read lazily, in `order`, each read filling what it must.
  fn lazy(history: &History, order: &[u64]) -> Fixture {
    let mut fixture = Fixture::empty(Mode::Lazy);
    let mut roots = vec![None; history.len()];
    for version in order {
      let read = fixture
        .derivations
        .read(&mut fixture.store, &fixture.ty, history, *version);
      let root = match read.unwrap() {
        Read::Ready(root) => root,
        Read::Fill(mut fill) => {
          let root = drive(
            &mut fill,
            &mut fixture.derivations,
            &mut fixture.ty,
            history,
            &mut fixture.store,
          );
          fill.release(&mut fixture.store).unwrap();
          root
        }
      };
      roots[index(*version)] = Some(root);
    }
    fixture.roots = roots.into_iter().flatten().collect();
    fixture
  }

  /// `path`'s history at `version`, in full.
  fn log(&mut self, version: u64, path: &str, follow: bool) -> (Vec<Logged>, LogCost) {
    let root = self.roots[index(version)];
    log(&mut self.store, root, path.as_bytes(), follow, u64::MAX).unwrap()
  }

  /// `path`'s history at `version` as (version, path) pairs.
  fn listed(&mut self, version: u64, path: &str, follow: bool) -> Vec<(u64, String)> {
    let (answer, _) = self.log(version, path, follow);
    answer
      .iter()
      .map(|logged| {
        let path = String::from_utf8(logged.path.clone()).unwrap();
        (number(version_of(&logged.version)), path)
      })
      .collect()
  }

  /// Gives every value back: the store is then empty and charges nothing, so no question kept a charge.
  fn finish(self) {
    let Fixture {
      mut store,
      derivations,
      blobs,
      ..
    } = self;
    derivations.release(&mut store).unwrap();
    for blob in blobs {
      store.release(&blob).unwrap();
    }
    assert!(store.is_empty(), "the values freed whole");
    assert_eq!((store.used(), store.charged()), (0, 0));
  }
}

/// What the oracle met, summed over every case: proof that each path it holds to the model ran.
#[derive(Debug, Default)]
struct Census {
  versions: u64,
  merges: u64,
  octopus: u64,
  from_parent: u64,
  from_older: u64,
  made: u64,
  removals: u64,
  sourced: u64,
  diverted: u64,
  questions: u64,
  empty: u64,
  listed_removals: u64,
  listed_merges: u64,
  listed_sources: u64,
  kind_changes: u64,
  cut: u64,
  walks_saved: u64,
  most_reads: u64,
}

impl Census {
  fn add(&mut self, other: &Census) {
    self.versions += other.versions;
    self.merges += other.merges;
    self.octopus += other.octopus;
    self.from_parent += other.from_parent;
    self.from_older += other.from_older;
    self.made += other.made;
    self.removals += other.removals;
    self.sourced += other.sourced;
    self.diverted += other.diverted;
    self.questions += other.questions;
    self.empty += other.empty;
    self.listed_removals += other.listed_removals;
    self.listed_merges += other.listed_merges;
    self.listed_sources += other.listed_sources;
    self.kind_changes += other.kind_changes;
    self.cut += other.cut;
    self.walks_saved += other.walks_saved;
    self.most_reads = self.most_reads.max(other.most_reads);
  }

  /// Each kind met at least once: merges of two and three, renames and copies from the first parent, copies from older
  /// versions, nodes made, removals, sources, paths diverted to a later parent, answers that are empty, list a removal,
  /// a merge, a source's path, a file and a directory at one path, answers cut at their limit, and answers a walk would
  /// have cost four times over.
  fn assert_met(&self) {
    let met = [
      self.merges,
      self.octopus,
      self.from_parent,
      self.from_older,
      self.made,
      self.removals,
      self.sourced,
      self.diverted,
      self.empty,
      self.listed_removals,
      self.listed_merges,
      self.listed_sources,
      self.kind_changes,
      self.cut,
      self.walks_saved,
    ];
    assert!(met.iter().all(|count| *count > 0), "{self:?}");
  }

  fn note_history(&mut self, built: &Builder, fixture: &Fixture) {
    let history = &built.history;
    self.versions += number(history.len());
    for (version, parents) in history.parents.iter().enumerate() {
      self.merges += u64::from(parents.len() >= 2);
      self.octopus += u64::from(parents.len() >= 3);
      let sources = &history.sources[version];
      let first = parents.first().copied();
      let from_parent = sources.values().all(|(_, from)| Some(*from) == first);
      self.from_parent += u64::from(!sources.is_empty() && from_parent);
      self.from_older += u64::from(!from_parent);
    }
    let counters = fixture.ty.counters();
    self.made += counters.made;
    self.removals += counters.removals;
    self.sourced += counters.sourced;
    self.diverted += counters.diverted;
  }

  fn note_answer(&mut self, answer: &[Logged], path: &[u8], model: &Model) {
    self.questions += 1;
    self.empty += u64::from(answer.is_empty());
    self.listed_removals += u64::from(answer.iter().any(|logged| logged.made == Made::Absent));
    self.listed_merges += u64::from(
      answer
        .iter()
        .any(|logged| model.parents[version_of(&logged.version)].len() >= 2),
    );
    self.listed_sources += u64::from(answer.iter().any(|logged| logged.path != path));
    let file = answer
      .iter()
      .any(|logged| matches!(logged.made, Made::File(_)));
    let directory = answer.iter().any(|logged| logged.made == Made::Directory);
    self.kind_changes += u64::from(file && directory);
  }

  fn note_cost(&mut self, cost: &LogCost, (cut, visits): (bool, u64)) {
    self.cut += u64::from(cut);
    self.walks_saved += u64::from(cost.listed > 0 && visits >= 4 * cost.reads);
    self.most_reads = self.most_reads.max(cost.reads);
  }
}

/// The paths a change makes its version judge, by the module's rule: each path of the change, each directory on one,
/// and the root.
fn judged_by_rule(change: &Change) -> usize {
  let mut paths: BTreeSet<Vec<u8>> = BTreeSet::new();
  paths.insert(Vec::new());
  for path in change.keys() {
    paths.extend(prefixes(path));
    paths.insert(path.clone());
  }
  paths.len()
}

/// Every derivation judged exactly the paths of its change and the directories on them (the non-vacuity count: the
/// derivation reaches what its change names and nothing else), and every value's map is its files' map built from
/// scratch.
fn check_values(fixture: &mut Fixture, built: &Builder) {
  let judged: usize = built.history.changes.iter().map(judged_by_rule).sum();
  assert_eq!(fixture.ty.counters().judged, number(judged));
  let mut maps: PathMap<History> = PathMap::declare(WINDOW, Mode::Lazy).unwrap();
  for (at, files) in built.maps.iter().enumerate() {
    let value = read_value(&fixture.store, fixture.roots[at]).unwrap();
    let made = maps.build(&mut fixture.store, files).unwrap();
    assert_eq!(value.map, made, "version {at}'s map is its files'");
    fixture.store.release(&made).unwrap();
  }
}

/// A value is the same however its history was filled: read lazily, first in a stride the case picks, then every
/// version from the newest down, every version's value is the eager one, identity for identity.
fn same_however_filled(history: &History, eager: &Fixture, stride: usize) {
  let count = history.len();
  let step = stride % count;
  let mut order: Vec<u64> = (0..count).map(|at| number((at * step) % count)).collect();
  order.extend((0..number(count)).rev());
  let lazy = Fixture::lazy(history, &order);
  assert_eq!(
    lazy.roots, eager.roots,
    "the same value whatever the filling's order"
  );
  lazy.finish();
}

/// One question: the answer is the model's, cut at the limit, each version's generation the model's; a lookup reads one
/// trie node a name of the path's key at most; a full answer reads at most one node more than it lists, and one cut at
/// its limit at most the nodes its listed ones lead to besides.
fn ask(
  fixture: &mut Fixture,
  model: &Model,
  (version, path, follow, limit): (usize, &[u8], bool, u64),
  census: &mut Census,
) {
  let root = fixture.roots[version];
  let (answer, cost) = log(&mut fixture.store, root, path, follow, limit).unwrap();
  let (mut expected, visits) = model.log(version, path, follow);
  let full = number(expected.len());
  expected.truncate(index(limit.min(full)));
  let got: Listing = answer
    .iter()
    .map(|logged| {
      (
        version_of(&logged.version),
        logged.path.clone(),
        logged.made,
      )
    })
    .collect();
  let asked = format!("version {version}, path {path:?}, follow {follow}, limit {limit}");
  assert_eq!(got, expected, "{asked}");
  let generations = answer
    .iter()
    .all(|logged| logged.generation == model.generation[version_of(&logged.version)]);
  assert!(generations, "{asked}");
  let names = number(key(path).split(|byte| *byte == b'/').count());
  let cut = limit < full;
  let listed = number(answer.len());
  assert_eq!(cost.listed, listed, "{asked}");
  let bound = if cut {
    2 + listed * (1 + MAX_PARENTS)
  } else {
    1 + listed
  };
  assert!(
    cost.lookups <= names && cost.reads <= bound,
    "{cost:?}, bound {bound}, {full} in all: {asked}"
  );
  census.note_answer(&answer, path, model);
  census.note_cost(&cost, (cut, visits));
}

/// One case of the oracle: a history grown from `ops`, every value checked and filled again lazily, and every question
/// asked of each generated triple and of each branch tip's hot file, cut and in full.
fn case(ops: &[Op], questions: &[(usize, usize, usize)]) -> Census {
  let (built, tips) = grown(ops);
  let model = Model::of(&built);
  let mut fixture = Fixture::eager(&built.history);
  let mut census = Census::default();
  census.note_history(&built, &fixture);
  check_values(&mut fixture, &built);
  same_however_filled(&built.history, &fixture, ops.len() * 7 + 1);
  let mut paths = universe();
  paths.push(Vec::new());
  for (x, y, z) in questions {
    let limit = if z / 2 % 3 == 0 {
      number(1 + z / 6 % 3)
    } else {
      u64::MAX
    };
    let question = (
      x % model.len(),
      paths[y % paths.len()].as_slice(),
      z % 2 == 1,
      limit,
    );
    ask(&mut fixture, &model, question, &mut census);
  }
  for tip in tips {
    ask(
      &mut fixture,
      &model,
      (index(tip), HOT, false, 2),
      &mut census,
    );
    ask(
      &mut fixture,
      &model,
      (index(tip), HOT, true, u64::MAX),
      &mut census,
    );
  }
  let brought = built.history.sources.iter().enumerate();
  for (version, sources) in brought.filter(|(_, sources)| !sources.is_empty()) {
    if let Some(path) = sources.keys().next() {
      ask(
        &mut fixture,
        &model,
        (version, path, true, u64::MAX),
        &mut census,
      );
    }
  }
  fixture.finish();
  census
}

/// A-666 part 2 (A-645 "Queries by API"). Do: grow histories of up to 240 versions from generated steps (edits anywhere,
/// a file made where a directory stood and back, runs of edits to a hot file, renames of files and directories some
/// edited as they move, copies from the first parent and from older versions, merges of two and three resolving each
/// differing path every way, criss-cross merges, unrelated roots, removals of whole directories), derive every version
/// eagerly and again lazily in another order, and ask `log` and `log --follow` of generated versions and paths, in full
/// and cut at a limit. Expect: every answer equals git's default-mode walk with recorded sources followed, newest
/// first; every derivation judged exactly its change's paths and their directories; every value's map is its files';
/// the lazy values equal the eager ones; every full answer reads at most one node more than it lists and every cut one
/// at most what its listed nodes lead to; the store is empty after; and the census, printed every run, met every kind,
/// among them answers a walk would have cost four times over.
#[test]
#[allow(
  clippy::print_stderr,
  reason = "the census is the oracle's evidence, printed with every run"
)]
fn t_a666_p2_every_history_equals_gits_default_walk_within_its_bound() {
  let census = RefCell::new(Census::default());
  let config = ProptestConfig {
    cases: slates_test_seeds::cases(32),
    ..ProptestConfig::default()
  };
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(config));
  let ops = proptest::collection::vec(op(), 1..48);
  let questions =
    proptest::collection::vec((any::<usize>(), any::<usize>(), any::<usize>()), 8..24);
  let result = runner.run(&(ops, questions), |(ops, questions)| {
    census.borrow_mut().add(&case(&ops, &questions));
    Ok(())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  eprintln!("path history census: {census:?}");
  census.assert_met();
}

/// A-666 part 2. Do: record 20,000 versions in a line, the hot file changed in every 20th, the cold file in 5 after its
/// making, another file in every version; ask `log hot`, `log cold`, and `log hot` cut at 10. Expect: each lists exactly
/// the versions that changed its path, newest first; the full answers read the value and one node a version listed
/// (1,001 and 7 reads), where git's walk visits all 20,000 versions; the cut one reads 12 (the value, its ten, and the
/// parent the tenth put on the frontier).
#[test]
fn t_a666_p2_a_path_is_read_by_its_changes_not_by_the_history() {
  let mut built = Builder::default();
  let mut tip = built.edit(
    None,
    &[("hot", Some(0)), ("cold", Some(0)), ("other", Some(0))],
  );
  let colds = [3_001, 7_002, 11_003, 15_004, 19_005];
  for version in 1..LONG_HISTORY {
    let mut edits = vec![("other", Some(index(version % 2)))];
    if version % HOT_EVERY == 0 {
      edits.push(("hot", Some(index(version / HOT_EVERY % 2 + 2))));
    }
    if colds.contains(&version) {
      edits.push(("cold", Some(index(version % 3 + 1))));
    }
    tip = built.edit(Some(tip), &edits);
  }
  let (_, visits) = Model::of(&built).log(index(tip), b"hot", false);
  assert_eq!(visits, LONG_HISTORY);
  let mut fixture = Fixture::eager(&built.history);
  let (hot, hot_cost) = fixture.log(tip, "hot", false);
  let changed: Vec<u64> = (0..LONG_HISTORY)
    .rev()
    .filter(|version| version % HOT_EVERY == 0)
    .collect();
  let listed: Vec<u64> = hot
    .iter()
    .map(|logged| number(version_of(&logged.version)))
    .collect();
  assert_eq!((listed, hot_cost.reads), (changed, 1_001));
  let (cold, cold_cost) = fixture.log(tip, "cold", false);
  assert_eq!((cold.len(), cold_cost.reads), (6, 7));
  let root = fixture.roots[index(tip)];
  let (cut, cut_cost) = log(&mut fixture.store, root, b"hot", false, 10).unwrap();
  assert_eq!((cut.len(), cut_cost.reads), (10, 12));
  fixture.finish();
}

/// A-666 part 2. Do: make `a`, edit it, rename it `b`, edit it, rename it `dir/c` (a new directory), edit it. Expect:
/// `log dir/c` lists its edit and the rename that made it; `log --follow dir/c` lists every version back to `a`'s
/// making, each at the path it changed then; `log a` lists its removal by the rename, then its history; `log dir` lists
/// the versions that changed beneath it, with no source of its own.
#[test]
fn t_a666_p2_renames_are_followed_only_with_follow() {
  let mut built = Builder::default();
  let v0 = built.edit(None, &[("a", Some(0))]);
  let v1 = built.edit(Some(v0), &[("a", Some(1))]);
  let v2 = built.bring(v1, (b"a", v1), b"b", true);
  let v3 = built.edit(Some(v2), &[("b", Some(2))]);
  let v4 = built.bring(v3, (b"b", v3), b"dir/c", true);
  let v5 = built.edit(Some(v4), &[("dir/c", Some(3))]);
  let mut fixture = Fixture::eager(&built.history);
  let at = |pairs: &[(u64, &str)]| -> Vec<(u64, String)> {
    pairs
      .iter()
      .map(|(version, path)| (*version, (*path).to_owned()))
      .collect()
  };
  assert_eq!(
    fixture.listed(v5, "dir/c", false),
    at(&[(v5, "dir/c"), (v4, "dir/c")])
  );
  let followed = at(&[
    (v5, "dir/c"),
    (v4, "dir/c"),
    (v3, "b"),
    (v2, "b"),
    (v1, "a"),
    (v0, "a"),
  ]);
  assert_eq!(fixture.listed(v5, "dir/c", true), followed);
  let (a, _) = fixture.log(v5, "a", false);
  let made: Vec<Made> = a.iter().map(|logged| logged.made).collect();
  assert_eq!(
    made,
    [Made::Absent, Made::File(file(1)), Made::File(file(0))]
  );
  assert_eq!(
    fixture.listed(v5, "dir", true),
    at(&[(v5, "dir"), (v4, "dir")])
  );
  fixture.finish();
}

/// A-666 part 2. Do: edit `trunk/f` in versions 0, 1 and 2; in version 3 copy `trunk` as it stood in version 1 to
/// `tags/t` (Subversion's copy-from revision); edit `trunk/f` again. Expect: the copy's inputs are its parent and
/// version 1; `log --follow tags/t/f` lists the copy, then `trunk/f`'s history up to version 1 and not version 2's edit;
/// `log --follow tags/t` the same for the directory; a lazy read of version 3 alone fills version 1 first.
#[test]
fn t_a666_p2_a_copy_from_an_older_version_is_followed_there() {
  let mut built = Builder::default();
  let v0 = built.edit(None, &[("trunk/f", Some(0))]);
  let v1 = built.edit(Some(v0), &[("trunk/f", Some(1))]);
  let v2 = built.edit(Some(v1), &[("trunk/f", Some(2))]);
  let v3 = built.bring(v2, (b"trunk", v1), b"tags/t", false);
  let v4 = built.edit(Some(v3), &[("trunk/f", Some(3))]);
  let mut fixture = Fixture::eager(&built.history);
  assert_eq!(fixture.ty.inputs(&built.history, v3), Some(vec![v2, v1]));
  let expect = |path: &str, from: &str| {
    vec![
      (v3, path.to_owned()),
      (v1, from.to_owned()),
      (v0, from.to_owned()),
    ]
  };
  assert_eq!(
    fixture.listed(v4, "tags/t/f", true),
    expect("tags/t/f", "trunk/f")
  );
  assert_eq!(
    fixture.listed(v4, "tags/t", true),
    expect("tags/t", "trunk")
  );
  let lazy = Fixture::lazy(&built.history, &[v3]);
  assert_eq!(lazy.roots, [fixture.roots[index(v3)]]);
  lazy.finish();
  fixture.finish();
}

/// A-666 part 2, git's rule at a merge. Do: from `f`'s making, one side edits `g` and the other `f`; merge them taking
/// the second side's `f`; then two sides that both make `f` the same, merged; then two sides making it differ, merged
/// to a third content. Expect: the first merge is TREESAME to its second parent, so `log f` follows only that side and
/// lists no merge; the second is TREESAME to its first parent, so the second side's identical edit is not listed (git's
/// "follow only one of them"); the third is listed with both sides, newest first, then by name.
#[test]
fn t_a666_p2_a_merge_follows_its_first_treesame_parent() {
  let mut built = Builder::default();
  let v0 = built.edit(None, &[("f", Some(0)), ("g", Some(0))]);
  let v1 = built.edit(Some(v0), &[("g", Some(1))]);
  let v2 = built.edit(Some(v0), &[("f", Some(1))]);
  let v3 = built.merge(&[v1, v2], &[("f", Some(1))]);
  let v4 = built.edit(Some(v0), &[("f", Some(2))]);
  let v5 = built.edit(Some(v0), &[("f", Some(2))]);
  let v6 = built.merge(&[v4, v5], &[]);
  let v7 = built.edit(Some(v0), &[("f", Some(3))]);
  let v8 = built.merge(&[v4, v7], &[("f", Some(4))]);
  let mut fixture = Fixture::eager(&built.history);
  let versions = |fixture: &mut Fixture, at: u64| -> Vec<u64> {
    fixture
      .listed(at, "f", false)
      .into_iter()
      .map(|(version, _)| version)
      .collect()
  };
  assert_eq!(versions(&mut fixture, v3), [v2, v0]);
  assert_eq!(versions(&mut fixture, v6), [v4, v0]);
  assert_eq!(versions(&mut fixture, v8), [v8, v4, v7, v0]);
  assert!(fixture.ty.counters().diverted > 0);
  fixture.finish();
}

/// A-666 part 2. Do: make `d/x` and `d/y`; one side edits `d/x`, the other edits `d/x` the same and `d/y`; merge them
/// taking the second side's `d` whole. Expect: `log d/x` follows the first parent (its `d/x` is the merge's) and `log
/// d` the second (its `d` is the merge's), as git's TREESAME of a directory compares its files; Mononoke's unodes, which
/// compare a directory's children's unodes, would list the merge for `d`.
#[test]
fn t_a666_p2_a_directory_and_a_file_beneath_it_follow_their_own_parents() {
  let mut built = Builder::default();
  let v0 = built.edit(None, &[("d/x", Some(0)), ("d/y", Some(0))]);
  let v1 = built.edit(Some(v0), &[("d/x", Some(1))]);
  let v2 = built.edit(Some(v0), &[("d/x", Some(1)), ("d/y", Some(1))]);
  let v3 = built.merge(&[v1, v2], &[("d/y", Some(1))]);
  let mut fixture = Fixture::eager(&built.history);
  let versions = |fixture: &mut Fixture, path: &str| -> Vec<u64> {
    fixture
      .listed(v3, path, false)
      .into_iter()
      .map(|(version, _)| version)
      .collect()
  };
  assert_eq!(versions(&mut fixture, "d/x"), [v1, v0]);
  assert_eq!(versions(&mut fixture, "d"), [v2, v0]);
  fixture.finish();
}

/// A-666 part 2. Do: make `p`, remove it, make it again, then make it a directory (`p/q`). Expect: `log p` lists all
/// four versions, newest first, each with what it left: a directory, a file, nothing, a file; and the root's history,
/// `log` of the empty path, lists every version that changed anything.
#[test]
fn t_a666_p2_a_removed_path_keeps_its_history_and_may_change_kind() {
  let mut built = Builder::default();
  let v0 = built.edit(None, &[("p", Some(0)), ("r", Some(0))]);
  let v1 = built.edit(Some(v0), &[("p", None)]);
  let v2 = built.edit(Some(v1), &[("p", Some(1))]);
  let v3 = built.edit(Some(v2), &[("p", None), ("p/q", Some(2))]);
  let v4 = built.edit(Some(v3), &[]);
  let mut fixture = Fixture::eager(&built.history);
  let (p, _) = fixture.log(v4, "p", false);
  let made: Vec<(u64, Made)> = p
    .iter()
    .map(|logged| (number(version_of(&logged.version)), logged.made))
    .collect();
  let expected = [
    (v3, Made::Directory),
    (v2, Made::File(file(1))),
    (v1, Made::Absent),
    (v0, Made::File(file(0))),
  ];
  assert_eq!(made, expected);
  let root: Vec<u64> = fixture
    .listed(v4, "", false)
    .into_iter()
    .map(|(version, _)| version)
    .collect();
  assert_eq!(root, [v3, v2, v1, v0]);
  fixture.finish();
}

/// The identity of a derived node whose bytes are `names` then `payload`: BLAKE3's key derivation under the store's
/// context for derived nodes, over the layout written by hand.
fn identity_of(names: &[ContentId], payload: &[u8]) -> ContentId {
  let mut bytes = u32::try_from(names.len()).unwrap().to_le_bytes().to_vec();
  for name in names {
    bytes.extend_from_slice(&name.0);
  }
  bytes.extend_from_slice(payload);
  let mut hasher =
    blake3::Hasher::new_derive_key("slates-vcs 2026-11-19 object store: a derived value's node");
  hasher.update(&bytes);
  ContentId(*hasher.finalize().as_bytes())
}

/// A node's payload: the tag, no parents, no source, `kind`, class zero, generation one, version 0's name.
fn root_node_payload(kind: u8) -> Vec<u8> {
  let mut payload = b"N\0\0\0\0\0".to_vec();
  payload.push(kind);
  payload.push(0);
  payload.extend_from_slice(&1_u64.to_le_bytes());
  payload.extend_from_slice(&8_u32.to_le_bytes());
  payload.extend_from_slice(&0_u64.to_be_bytes());
  payload
}

/// A-666 part 2, the format. Do: derive a root version holding one file `f`, and write its value's bytes by hand: the
/// path map's leaf, the two path nodes (the root's directory and `f`'s file), the history tree's two leaves (the root's,
/// holding `\0` and `f`; `f`'s, holding `\0`), and the value. Expect: the derived value's identity is the one BLAKE3's
/// key derivation gives the hand-written bytes, node by node.
#[test]
fn t_a666_p2_a_one_file_history_is_the_formats_bytes() {
  let mut built = Builder::default();
  built.edit(None, &[("f", Some(0))]);
  let fixture = Fixture::eager(&built.history);
  let blob = ContentId::of(&blob(0));
  let map = identity_of(&[blob], b"L\x01\0\0\0f\0\0");
  let root_node = identity_of(&[], &root_node_payload(2));
  let file_node = identity_of(&[blob], &root_node_payload(1));
  let file_leaf = identity_of(&[file_node], b"L\x01\0\0\0\0\0\0");
  let tree = identity_of(
    &[root_node, file_leaf],
    b"L\x01\0\0\0\0\0\0\x01\0\0\0f\x01\0",
  );
  let mut value_payload = b"H".to_vec();
  value_payload.extend_from_slice(&1_u64.to_le_bytes());
  assert_eq!(fixture.roots, [identity_of(&[map, tree], &value_payload)]);
  fixture.finish();
}

/// A node of every field: two parents, a source, a file.
fn full_node() -> Node {
  Node {
    parents: vec![ContentId::of(b"p1"), ContentId::of(b"p2")],
    source: Some((ContentId::of(b"s"), b"from".to_vec())),
    made: Made::File(file(0)),
    generation: 7,
    name: b"version".to_vec(),
  }
}

/// `full_node` changed by `change`, encoded (the encoder writes what it is given, sound or not).
fn encoded(change: impl FnOnce(&mut Node)) -> (Vec<ContentId>, Vec<u8>) {
  let mut node = full_node();
  change(&mut node);
  node.encode().unwrap()
}

/// The bytes of nodes broken one way each: a foreign tag, cut short inside the generation, a count past its names, a
/// name left over, a source byte and a kind no node has, a directory of a class, a removal with a source or with no
/// parent, a source of no path, generation zero, a name's length past the payload.
fn broken_nodes() -> Vec<(Vec<ContentId>, Vec<u8>)> {
  let (names, payload) = full_node().encode().unwrap();
  let patched = |at: usize, byte: u8| {
    let mut broken = payload.clone();
    broken[at] = byte;
    (names.clone(), broken)
  };
  let mut directory = encoded(|node| node.made = Made::Directory);
  directory.1[7] = 1;
  vec![
    patched(0, b'X'),
    (names.clone(), payload[..9].to_vec()),
    patched(1, 3),
    ([names.clone(), vec![names[0]]].concat(), payload.clone()),
    patched(5, 2),
    patched(6, 3),
    directory,
    encoded(|node| node.made = Made::Absent),
    encoded(|node| {
      node.made = Made::Absent;
      node.source = None;
      node.parents.clear();
    }),
    encoded(|node| node.source = Some((ContentId::of(b"s"), Vec::new()))),
    encoded(|node| node.generation = 0),
    patched(16, 0xff),
  ]
}

/// A-666 part 2, hostile bytes. Do: decode a node of every field, a directory and a removal, then nodes broken one way
/// at a time ([`broken_nodes`]); and values with a foreign tag, three names, generation zero or a long payload.
/// Expect: the sound nodes read back whole; every broken node and value is refused.
#[test]
fn t_a666_p2_hostile_bytes_are_refused_by_the_readers() {
  let sound = [
    full_node(),
    Node {
      made: Made::Directory,
      ..full_node()
    },
    Node {
      made: Made::Absent,
      source: None,
      ..full_node()
    },
  ];
  for node in sound {
    let (names, payload) = node.encode().unwrap();
    assert_eq!(decode(&names, &payload), Some(node));
  }
  for (at, (names, bytes)) in broken_nodes().iter().enumerate() {
    assert_eq!(decode(names, bytes), None, "broken way {at}");
  }
  let mut store = ContentStore::new(BUDGET, WINDOW).unwrap();
  let blob = store.put(b"x").unwrap();
  let values: [(&[ContentId], &[u8]); 4] = [
    (&[blob, blob], b"X\x01\0\0\0\0\0\0\0"),
    (&[blob, blob, blob], b"H\x01\0\0\0\0\0\0\0"),
    (&[blob, blob], b"H\0\0\0\0\0\0\0\0"),
    (&[blob, blob], b"H\x01\0\0\0\0\0\0\0\0"),
  ];
  for (names, payload) in values {
    let node = store.put_derived(names, payload).unwrap().identity;
    assert_eq!(
      read_value(&store, node),
      Err(HistoryRefused::Malformed { node })
    );
    store.release(&node).unwrap();
  }
  store.release(&blob).unwrap();
  assert!(store.is_empty());
}

/// The store's contents, its bytes used and its charge: what a refusal must leave as it was.
fn state(store: &ContentStore) -> (usize, u64, u64) {
  (store.len(), store.used(), store.charged())
}

/// Derives `version` of `built` straight from the eager fixture's values of its inputs.
fn derive(
  fixture: &mut Fixture,
  built: &Builder,
  version: u64,
) -> Result<ContentId, HistoryRefused> {
  let inputs = fixture.ty.inputs(&built.history, version).unwrap();
  let values: Vec<ContentId> = inputs
    .iter()
    .map(|input| fixture.roots[index(*input)])
    .collect();
  fixture
    .ty
    .derive(&built.history, &mut fixture.store, version, &values)
}

/// Records a version of `parents` whose change is `change` and whose sources are `sources`.
fn record_raw(built: &mut Builder, parents: &[u64], change: Change, sources: Sources<u64>) -> u64 {
  built.history.changes.push(change);
  built.history.parents.push(parents.to_vec());
  built.history.sources.push(sources);
  built.maps.push(Files::new());
  number(built.maps.len() - 1)
}

/// A-666 part 2, refusals. Do: over a history of versions 0 (`f` and `d/x`), 1 and 2 (siblings on 0) and 3 (on 1),
/// derive versions on 2 whose sources do not hold four ways (the destination absent, the source absent, a file from a
/// directory, a source from version 3, of the same generation), whose change names the reserved name or removes an
/// absent path, with too many values, and past the store's budget; and ask a question of a path naming the reserved
/// name and of a root that is no value. Expect: each refused by its kind, the store as it was.
#[test]
fn t_a666_p2_refusals_by_kind_leave_the_store_as_it_was() {
  let mut built = Builder::default();
  let v0 = built.edit(None, &[("f", Some(0)), ("d/x", Some(0))]);
  let v1 = built.edit(Some(v0), &[("g", Some(1))]);
  let v2 = built.edit(Some(v0), &[("h", Some(1))]);
  let v3 = built.edit(Some(v1), &[("k", Some(1))]);
  let mut fixture = Fixture::eager(&built.history);
  let source = |path: &str, from: &str, version: u64| -> Sources<u64> {
    [(
      path.as_bytes().to_vec(),
      (from.as_bytes().to_vec(), version),
    )]
    .into()
  };
  let set = |path: &str| -> Change { [(path.as_bytes().to_vec(), Some(file(2)))].into() };
  let bad = |path: &str| HistoryRefused::BadSource {
    path: path.as_bytes().to_vec(),
  };
  let cases = [
    (Change::new(), source("g", "f", v0), bad("g")),
    (set("g"), source("g", "nothing", v0), bad("g")),
    (set("g"), source("g", "d", v0), bad("g")),
    (set("m"), source("m", "k", v3), bad("m")),
    (
      set("a/\0"),
      Sources::new(),
      HistoryRefused::ReservedName {
        path: b"a/\0".to_vec(),
      },
    ),
    (
      [(b"nothing".to_vec(), None)].into(),
      Sources::new(),
      HistoryRefused::Paths(PathsRefused::RemovedAbsent {
        path: b"nothing".to_vec(),
      }),
    ),
  ];
  let before = state(&fixture.store);
  for (change, sources, refused) in cases {
    let version = record_raw(&mut built, &[v2], change, sources);
    assert_eq!(derive(&mut fixture, &built, version), Err(refused));
    assert_eq!(state(&fixture.store), before);
  }
  let roots = [fixture.roots[index(v0)]];
  let wrong = fixture.ty.derive(
    &built.history,
    &mut fixture.store,
    v2,
    &[roots[0], roots[0]],
  );
  assert_eq!(
    wrong,
    Err(HistoryRefused::Inputs {
      expected: 1,
      given: 2
    })
  );
  let version = record_raw(&mut built, &[v2], set("z"), Sources::new());
  fixture.store.set_budget(fixture.store.used());
  let over = derive(&mut fixture, &built, version);
  assert!(matches!(
    over,
    Err(HistoryRefused::Content(ContentRefused::OverBudget { .. }))
  ));
  fixture.store.set_budget(BUDGET);
  assert_eq!(state(&fixture.store), before);
  let root = fixture.roots[index(v2)];
  let reserved = log(&mut fixture.store, root, b"a/\0", false, u64::MAX);
  assert_eq!(
    reserved,
    Err(HistoryRefused::ReservedName {
      path: b"a/\0".to_vec()
    })
  );
  let blob = fixture.blobs[0];
  assert_eq!(
    log(&mut fixture.store, blob, b"f", false, u64::MAX),
    Err(HistoryRefused::Malformed { node: blob })
  );
  assert_eq!(state(&fixture.store), before);
  fixture.finish();
}

/// A-666 part 2, a forged history. Do: put a node of generation 3 whose parent claims generation 5, in a tree under a
/// value of generation 9, and ask `log` of its path. Expect: the parent is refused as malformed (no older than the node
/// naming it), the question's charge given back and the store as it was.
#[test]
fn t_a666_p2_a_forged_history_is_refused_by_the_question() {
  let mut store = ContentStore::new(BUDGET, WINDOW).unwrap();
  let content = store.put(&blob(0)).unwrap();
  let entry = Entry { class: 0, content };
  let put = |store: &mut ContentStore, node: &Node| {
    let (names, payload) = node.encode().unwrap();
    store.put_derived(&names, &payload).unwrap().identity
  };
  let parent = Node {
    parents: Vec::new(),
    source: None,
    made: Made::File(entry),
    generation: 5,
    name: b"older?".to_vec(),
  };
  let parent = put(&mut store, &parent);
  let child = Node {
    parents: vec![parent],
    source: None,
    made: Made::File(entry),
    generation: 3,
    name: b"child".to_vec(),
  };
  let child = put(&mut store, &child);
  let mut maps: PathMap<History> = PathMap::declare(WINDOW, Mode::Lazy).unwrap();
  let tree = maps
    .apply(
      &mut store,
      None,
      &[(
        key(b"f"),
        Some(Entry {
          class: 0,
          content: child,
        }),
      )]
      .into(),
    )
    .unwrap();
  let map = maps.apply(&mut store, None, &Change::new()).unwrap();
  let mut payload = b"H".to_vec();
  payload.extend_from_slice(&9_u64.to_le_bytes());
  let value = store.put_derived(&[map, tree], &payload).unwrap().identity;
  let before = state(&store);
  let answer = log(&mut store, value, b"f", false, u64::MAX);
  assert_eq!(answer, Err(HistoryRefused::Malformed { node: parent }));
  assert_eq!(state(&store), before);
  for held in [value, map, tree, child, parent, content] {
    store.release(&held).unwrap();
  }
  assert!(store.is_empty());
}
