//! Derived data held to its definition (A-647): every value equals a naive recomputation from the root, each
//! derivation enters only its change's directories, each fill derives exactly the versions it had to, one unit a step,
//! a value is immutable under its type and version, and every refusal is typed.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::paths::{Change, Entry, PathChanges, PathMap, PathsRefused};
use super::{Derivations, DerivedRefused, Fill, Graph, Mode, Progress, Read};
use crate::content::{
  ContentId, ContentRefused, ContentStore, DerivedNode, Form, IDENTITY_BYTES, NAMES_COUNT_BYTES,
};
use crate::index::Inserted;

/// Shape: the store's window in these tests: a branch of every child (527 bytes) fits, and the lazy declaration splits
/// by it.
const STORE_WINDOW: usize = 1024;

/// Shape: the eager declaration's window: a leaf holds fifteen of the wide directory's names, so its forty split it, and
/// a name of 700 bytes is past it.
const EAGER_WINDOW: usize = 640;

/// Shape: the lazy declaration's window, the store's: the 700-byte name fits it.
const LAZY_WINDOW: usize = 1024;

/// Shape: the store's budget, room for every history the oracle makes.
const BUDGET: u64 = 1 << 26;

/// Shape: the distinct file contents the oracle's files name.
const BLOBS: usize = 6;

/// Shape: the long name's length: past the eager window alone, within the lazy one.
const LONG_NAME: usize = 700;

/// Format: what a leaf of one entry named `n` bytes takes beside them: the node's count of names, its tag, the
/// entry's identity, its name's length, its kind and its class.
const ONE_ENTRY_LEAF: usize = NAMES_COUNT_BYTES + 1 + IDENTITY_BYTES + 4 + 1 + 1;

/// A history of versions, each with its parents and its change.
#[derive(Debug, Default)]
struct History {
  versions: BTreeMap<u64, (Vec<u64>, Change)>,
}

impl Graph for History {
  type Version = u64;

  fn parents(&self, version: u64) -> Option<Vec<u64>> {
    self
      .versions
      .get(&version)
      .map(|(parents, _)| parents.clone())
  }

  fn name(&self, version: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&version.to_be_bytes());
  }
}

impl PathChanges for History {
  fn change(&self, version: u64) -> Option<&Change> {
    self.versions.get(&version).map(|(_, change)| change)
  }
}

impl History {
  fn record(&mut self, version: u64, parents: &[u64], change: Change) {
    self.versions.insert(version, (parents.to_vec(), change));
  }
}

/// A map of files as the model keeps it.
type Files = BTreeMap<Vec<u8>, Entry>;

/// A fill's answer as the tests read it.
type Answer = Result<ContentId, DerivedRefused<u64, PathsRefused>>;

/// Drives `fill` to its end, a step at a time, holding every step to one unit: one version found kept, walked through or
/// derived. The answer and the steps taken.
fn drive(
  fill: &mut Fill<u64>,
  derivations: &mut Derivations,
  ty: &mut PathMap<History>,
  history: &History,
  store: &mut ContentStore,
) -> (Answer, u64) {
  let mut steps = 0_u64;
  loop {
    let before = derivations.counters();
    let progress = fill.step(derivations, ty, history, store);
    steps += 1;
    let after = derivations.counters();
    let units = (after.walked - before.walked)
      + (after.derived - before.derived)
      + (after.found - before.found)
      + (after.rewalked - before.rewalked);
    match progress {
      Ok(Progress::More) => assert_eq!(units, 1, "a step is one unit"),
      Ok(Progress::Done(root)) => {
        assert_eq!(units, 1, "a step is one unit");
        return (Ok(root), steps);
      }
      Err(refused) => return (Err(refused), steps),
    }
    assert!(steps < 1_000_000, "a fill that never ends");
  }
}

/// The value of `version` under `ty`, read and filled to its end.
fn read_to_end(
  derivations: &mut Derivations,
  ty: &mut PathMap<History>,
  history: &History,
  store: &mut ContentStore,
  version: u64,
) -> (Answer, u64) {
  match derivations.read(store, ty, history, version) {
    Ok(Read::Ready(root)) => (Ok(root), 0),
    Ok(Read::Fill(mut fill)) => {
      let answer = drive(&mut fill, derivations, ty, history, store);
      fill.release(store).unwrap();
      answer
    }
    Err(refused) => (Err(refused), 0),
  }
}

/// The directories a derivation of `change` enters: the root and every directory on its paths; none for no change.
fn reached(change: &Change) -> u64 {
  if change.is_empty() {
    return 0;
  }
  let mut directories: BTreeSet<&[u8]> = BTreeSet::from([&b""[..]]);
  for path in change.keys() {
    for (at, byte) in path.iter().enumerate() {
      if *byte == b'/' {
        directories.insert(&path[..at]);
      }
    }
  }
  directories.len() as u64
}

/// Whether `path` is a path of a map: names separated by `/`, none empty, `.` or `..`.
fn model_path(path: &[u8]) -> bool {
  path
    .split(|byte| *byte == b'/')
    .all(|name| !name.is_empty() && name != b"." && name != b"..")
}

/// A file of `files` that is also a directory of another: the first such, by path.
fn model_shadowed(files: &Files) -> Option<Vec<u8>> {
  files.keys().find_map(|path| {
    path
      .iter()
      .enumerate()
      .filter(|(_, byte)| **byte == b'/')
      .map(|(at, _)| path[..at].to_vec())
      .find(|prefix| files.contains_key(prefix))
  })
}

/// A name of `files`, a file's or a directory's, whose entry alone is past `window`.
fn model_past_window(files: &Files, window: usize) -> Option<Vec<u8>> {
  files.keys().find_map(|path| {
    path
      .split(|byte| *byte == b'/')
      .find(|name| ONE_ENTRY_LEAF + name.len() > window)
      .map(<[u8]>::to_vec)
  })
}

/// The model's rule: the parent's files with the change's paths set and removed, or the refusal it earns.
fn model_apply(parent: &Files, change: &Change, window: usize) -> Result<Files, PathsRefused> {
  if let Some(path) = change.keys().find(|path| !model_path(path)) {
    return Err(PathsRefused::BadPath { path: path.clone() });
  }
  let mut files = parent.clone();
  for (path, set) in change {
    match set {
      Some(entry) => {
        files.insert(path.clone(), *entry);
      }
      None if files.remove(path).is_none() => {
        return Err(PathsRefused::RemovedAbsent { path: path.clone() });
      }
      None => {}
    }
  }
  if let Some(path) = model_shadowed(&files) {
    return Err(PathsRefused::Shadowed { path });
  }
  if let Some(name) = model_past_window(&files, window) {
    return Err(PathsRefused::EntryPastWindow { name });
  }
  Ok(files)
}

/// The paths the oracle's changes draw from: shallow files, nested directories, names that sort beside a directory's,
/// and a wide directory whose forty names split its trie under the eager window.
fn universe() -> Vec<Vec<u8>> {
  let mut paths: Vec<Vec<u8>> = [
    "a", "b", "c", "a.b", "d/e", "d/f", "d/g/h", "d/g/i", "d/g.h", "j/k/l/m", "j/k/n",
  ]
  .iter()
  .map(|path| path.as_bytes().to_vec())
  .collect();
  paths.extend((0..40).map(|at| format!("w/{at:02}").into_bytes()));
  paths
}

/// The element of `among` that `at` picks, or `None` when there is none.
fn pick_from<T: Clone>(among: &[T], at: usize) -> Option<T> {
  (!among.is_empty()).then(|| among[at % among.len()].clone())
}

/// The directories of `files`: every proper prefix of a path.
fn directories_of(files: &Files) -> Vec<Vec<u8>> {
  let mut directories = BTreeSet::new();
  for path in files.keys() {
    for (at, byte) in path.iter().enumerate() {
      if *byte == b'/' {
        directories.insert(path[..at].to_vec());
      }
    }
  }
  directories.into_iter().collect()
}

/// Adds the one hostile operation `kind` names to a valid `change` over `parent`: a removal of an absent path, a file
/// beneath a file, a file over a directory, a bad path, or a name past the eager window; any other kind adds none.
fn hostile(parent: &Files, mut change: Change, (kind, at): (u8, usize), entry: Entry) -> Change {
  let result = model_apply(parent, &change, LAZY_WINDOW).unwrap();
  let absent: Vec<Vec<u8>> = universe()
    .into_iter()
    .filter(|path| !parent.contains_key(path))
    .collect();
  let files: Vec<Vec<u8>> = result.keys().cloned().collect();
  let bad: [&[u8]; 4] = [b"", b"d//x", b"./a", b"j/.."];
  let added = match kind {
    7 => pick_from(&absent, at).map(|path| (path, None)),
    8 => pick_from(&files, at).map(|path| ([&path[..], &b"/z"[..]].concat(), Some(entry))),
    9 => pick_from(&directories_of(&result), at).map(|path| (path, Some(entry))),
    10 => Some((bad[at % bad.len()].to_vec(), Some(entry))),
    11 => Some((
      [&b"q/"[..], &vec![b'n'; LONG_NAME][..]].concat(),
      Some(entry),
    )),
    _ => None,
  };
  if let Some((path, set)) = added {
    change.insert(path, set);
  }
  change
}

/// One version to record: its parents (each a pick among the versions both declarations derive, or none), its file
/// removals and sets (a path, a content and a class), and a hostile operation.
#[derive(Clone, Debug)]
struct Recording {
  first: (bool, usize),
  second: (bool, usize),
  removes: Vec<usize>,
  sets: Vec<(usize, usize, u8)>,
  hostile: (u8, usize),
}

/// A step of the oracle.
#[derive(Clone, Debug)]
enum Step {
  /// Record a version; the eager declaration's value is filled as it is recorded.
  Record(Recording),
  /// Read a version through the lazy declaration, filling what it must.
  Read(usize),
}

/// A step, generated as plain tuples and a weighted selector (no `Arc`-backed `prop_oneof!`, R2): record 7, read 3.
fn step() -> impl Strategy<Value = Step> {
  (
    0_u8..10,
    (any::<bool>(), 0_usize..64),
    (proptest::bool::weighted(0.3), 0_usize..64),
    proptest::collection::vec(0_usize..64, 0..10),
    proptest::collection::vec((0_usize..64, 0_usize..BLOBS, 0_u8..3), 0..6),
    (0_u8..14, 0_usize..64),
  )
    .prop_map(|(selector, first, second, removes, sets, hostile)| {
      if selector < 7 {
        Step::Record(Recording {
          first,
          second,
          removes,
          sets,
          hostile,
        })
      } else {
        Step::Read(first.1)
      }
    })
}

/// A version as the model holds it.
struct Model {
  parents: Vec<u64>,
  change: Change,
  eager: Result<Files, PathsRefused>,
  lazy: Result<Files, PathsRefused>,
  /// Whether its lazy value is kept.
  lazy_kept: bool,
}

/// The oracle's run: one store, the history, the values, an eager and a lazy declaration of the path map, the contents
/// files name, and the model of every version.
struct Run {
  store: ContentStore,
  history: History,
  derivations: Derivations,
  eager: PathMap<History>,
  lazy: PathMap<History>,
  blobs: Vec<ContentId>,
  models: Vec<Model>,
}

impl Run {
  fn new() -> Run {
    let mut store = ContentStore::new(BUDGET, STORE_WINDOW).unwrap();
    let blobs = (0..BLOBS)
      .map(|at| store.put(format!("blob {at}").as_bytes()).unwrap())
      .collect();
    Run {
      store,
      history: History::default(),
      derivations: Derivations::new(),
      eager: PathMap::declare(EAGER_WINDOW, Mode::Eager).unwrap(),
      lazy: PathMap::declare(LAZY_WINDOW, Mode::Lazy).unwrap(),
      blobs,
      models: Vec::new(),
    }
  }

  fn apply(&mut self, step: &Step) {
    match step {
      Step::Record(recording) => self.record(recording),
      Step::Read(at) => self.read_lazy(*at),
    }
  }

  /// The version `choice` picks among those both declarations derive.
  fn parent(&self, choice: (bool, usize)) -> Option<usize> {
    let derivable: Vec<usize> = (0..self.models.len())
      .filter(|at| self.models[*at].eager.is_ok() && self.models[*at].lazy.is_ok())
      .collect();
    choice.0.then(|| pick_from(&derivable, choice.1)).flatten()
  }

  /// A valid change over `parent` from `recording`'s removals and sets (each set kept only if the change stays valid),
  /// then its hostile operation.
  fn change(&self, parent: &Files, recording: &Recording) -> Change {
    let present: Vec<Vec<u8>> = parent.keys().cloned().collect();
    let mut change: Change = recording
      .removes
      .iter()
      .filter_map(|at| pick_from(&present, *at).map(|path| (path, None)))
      .collect();
    let universe = universe();
    for (path, blob, class) in &recording.sets {
      let entry = Entry {
        class: *class,
        content: self.blobs[blob % BLOBS],
      };
      let mut trial = change.clone();
      trial.insert(universe[path % universe.len()].clone(), Some(entry));
      if model_apply(parent, &trial, LAZY_WINDOW).is_ok() {
        change = trial;
      }
    }
    let entry = Entry {
      class: 0,
      content: self.blobs[0],
    };
    hostile(parent, change, recording.hostile, entry)
  }

  fn record(&mut self, recording: &Recording) {
    let first = self.parent(recording.first);
    let second = first
      .and(self.parent(recording.second))
      .filter(|second| Some(*second) != first);
    let parent_files = first
      .and_then(|at| self.models[at].eager.clone().ok())
      .unwrap_or_default();
    let change = self.change(&parent_files, recording);
    let version = self.models.len() as u64;
    let parents: Vec<u64> = first
      .into_iter()
      .chain(second)
      .map(|at| at as u64)
      .collect();
    self.history.record(version, &parents, change.clone());
    self.models.push(Model {
      parents,
      eager: model_apply(&parent_files, &change, EAGER_WINDOW),
      lazy: model_apply(&parent_files, &change, LAZY_WINDOW),
      change,
      lazy_kept: false,
    });
    self.fill_eager(version);
    let lazy = self
      .derivations
      .recorded(&mut self.store, &self.lazy, version);
    assert!(
      lazy.unwrap().is_none(),
      "a lazy type computes nothing as a version is recorded"
    );
  }

  /// The eager declaration's fill of a version just recorded: one walk and one derivation, the change's directories
  /// alone entered, and the value equal to the naive recomputation; or the model's refusal.
  fn fill_eager(&mut self, version: u64) {
    let before = (self.derivations.counters(), self.eager.counters());
    let mut fill = self
      .derivations
      .recorded(&mut self.store, &self.eager, version)
      .unwrap()
      .expect("an eager type fills a version as it is recorded");
    let (answer, _) = drive(
      &mut fill,
      &mut self.derivations,
      &mut self.eager,
      &self.history,
      &mut self.store,
    );
    fill.release(&mut self.store).unwrap();
    let model = &self.models[version as usize];
    match (&model.eager, answer) {
      (Ok(files), Ok(root)) => {
        let derived = self.derivations.counters().derived - before.0.derived;
        let entered = self.eager.counters().directories - before.1.directories;
        assert_eq!((derived, entered), (1, reached(&model.change)));
        let files = files.clone();
        same_as_naive(&mut self.store, &mut self.eager, root, &files);
      }
      (Err(refused), Err(answer)) => assert_eq!(
        answer,
        DerivedRefused::Type {
          version,
          refused: refused.clone()
        }
      ),
      (expected, answer) => {
        panic!("version {version}: the model says {expected:?}, the fill {answer:?}")
      }
    }
  }

  /// The versions a lazy read of `version` must fill: those reachable from it through versions whose lazy value is not
  /// kept, itself among them unless kept.
  fn underived(&self, version: u64) -> BTreeSet<u64> {
    let mut found = BTreeSet::new();
    let mut pending = vec![version];
    while let Some(at) = pending.pop() {
      let model = &self.models[at as usize];
      if !model.lazy_kept && found.insert(at) {
        pending.extend(&model.parents);
      }
    }
    found
  }

  /// A lazy read: it derives exactly the versions it must, enters exactly their changes' directories, and its value
  /// equals the naive recomputation; or it answers the model's refusal after deriving every version above it.
  fn read_lazy(&mut self, at: usize) {
    if self.models.is_empty() {
      return;
    }
    let version = (at % self.models.len()) as u64;
    let to_fill = self.underived(version);
    let before = (self.derivations.counters(), self.lazy.counters());
    let (answer, _) = read_to_end(
      &mut self.derivations,
      &mut self.lazy,
      &self.history,
      &mut self.store,
      version,
    );
    let derived = self.derivations.counters().derived - before.0.derived;
    let walked = self.derivations.counters().walked - before.0.walked;
    match (self.models[version as usize].lazy.clone(), answer) {
      (Ok(files), Ok(root)) => {
        let entered = self.lazy.counters().directories - before.1.directories;
        let expected: u64 = to_fill
          .iter()
          .map(|at| reached(&self.models[*at as usize].change))
          .sum();
        assert_eq!(
          (derived, walked, entered),
          (to_fill.len() as u64, to_fill.len() as u64, expected)
        );
        self.kept(&to_fill);
        same_as_naive(&mut self.store, &mut self.lazy, root, &files);
      }
      (Err(refused), Err(answer)) => {
        assert_eq!(answer, DerivedRefused::Type { version, refused });
        assert_eq!(derived, to_fill.len() as u64 - 1);
        self.kept(&to_fill.into_iter().filter(|at| *at != version).collect());
      }
      (expected, answer) => {
        panic!("version {version}: the model says {expected:?}, the read {answer:?}")
      }
    }
  }

  fn kept(&mut self, versions: &BTreeSet<u64>) {
    for at in versions {
      self.models[*at as usize].lazy_kept = true;
    }
  }

  /// Gives everything back: once the values are released only the files' contents are held, once each, and once they
  /// are, the store is empty and charges nothing.
  fn finish(mut self) {
    self.derivations.release(&mut self.store).unwrap();
    assert_eq!(self.store.len(), self.blobs.len());
    for blob in &self.blobs {
      assert_eq!(self.store.holders(blob), 1);
      self.store.release(blob).unwrap();
    }
    assert!(self.store.is_empty());
    assert_eq!((self.store.used(), self.store.charged()), (0, 0));
  }
}

/// The value `root` reads as `files` and is the very root the naive recomputation from the root gives: `files` built
/// from scratch.
fn same_as_naive(
  store: &mut ContentStore,
  ty: &mut PathMap<History>,
  root: ContentId,
  files: &Files,
) {
  assert_eq!(&ty.files(store, root).unwrap(), files);
  let naive = ty.build(store, files).unwrap();
  assert_eq!(
    naive, root,
    "the incremental value is the naive one, identity for identity"
  );
  store.release(&naive).unwrap();
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(96), ..ProptestConfig::default() }))]

  /// A-647 (A-645 "Derived data, one mechanism"). Do: record generated histories (roots, first parents, merges' second
  /// parents, removals and sets over nested and wide directories, and one hostile operation in some: a removal of an
  /// absent path, a file beneath a file, a file over a directory, a bad path, a name past the eager window), the eager
  /// declaration filling each version as it is recorded, and read versions through the lazy declaration. Expect: every
  /// value reads as the model's map and is, identity for identity, the map built from scratch (the naive recomputation
  /// from the root); each derivation entered exactly its change's directories (the non-vacuity counter); each fill
  /// derived and walked exactly the versions it had to, one unit a step; every refusal is the model's; and once the
  /// values are released the store holds the files' contents alone.
  #[test]
  fn t_a647_every_value_equals_a_naive_recomputation_from_the_root(steps in proptest::collection::vec(step(), 1..40)) {
    let mut run = Run::new();
    for step in &steps {
      run.apply(step);
    }
    run.finish();
  }
}

/// A store, a chain of `len` versions each setting `f<n>` and changing `shared`, the contents they name.
fn chain(len: u64) -> (ContentStore, History, Vec<ContentId>) {
  let mut store = ContentStore::new(BUDGET, STORE_WINDOW).unwrap();
  let blobs: Vec<ContentId> = (0..2)
    .map(|at| store.put(format!("chain {at}").as_bytes()).unwrap())
    .collect();
  let mut history = History::default();
  for version in 0..len {
    let entry = |at: u64| Entry {
      class: 0,
      content: blobs[(at % 2) as usize],
    };
    let change = Change::from([
      (format!("f{version}").into_bytes(), Some(entry(0))),
      (b"dir/shared".to_vec(), Some(entry(version))),
    ]);
    let parents: Vec<u64> = version.checked_sub(1).into_iter().collect();
    history.record(version, &parents, change);
  }
  (store, history, blobs)
}

/// A-647: a lazy type. Do: record a chain of thirty versions, read version 20, then 29, then 29 again. Expect: nothing
/// derived as versions are recorded; the first read walks up to the root and fills down, 21 versions walked and 21
/// derived in 42 steps of one unit; the second walks up only to version 20, the nearest kept ancestor (9 and 9 in 18
/// steps); the third reads the kept value with no fill.
#[test]
fn t_a647_a_lazy_read_walks_to_the_nearest_kept_ancestor_and_fills_down() {
  let (mut store, history, _) = chain(30);
  let mut lazy: PathMap<History> = PathMap::declare(LAZY_WINDOW, Mode::Lazy).unwrap();
  let mut derivations = Derivations::new();
  assert!(
    derivations
      .recorded(&mut store, &lazy, 29)
      .unwrap()
      .is_none()
  );
  let (first, steps) = read_to_end(&mut derivations, &mut lazy, &history, &mut store, 20);
  let counters = derivations.counters();
  assert_eq!((steps, counters.walked, counters.derived), (42, 21, 21));
  let (second, steps) = read_to_end(&mut derivations, &mut lazy, &history, &mut store, 29);
  let counters = derivations.counters();
  assert_eq!((steps, counters.walked, counters.derived), (18, 30, 30));
  let (third, steps) = read_to_end(&mut derivations, &mut lazy, &history, &mut store, 29);
  assert_eq!((third.clone(), steps), (second.clone(), 0));
  assert_ne!(first.unwrap(), second.unwrap());
  derivations.release(&mut store).unwrap();
  assert_eq!(store.charged(), 0, "every fill gave its charge back");
}

/// A-647: an eager type. Do: record a chain of five versions, filling each as it is recorded. Expect: each fill walks
/// the version and derives it, two steps, its parent kept already; a lazy read of each afterwards is ready.
#[test]
fn t_a647_an_eager_type_is_filled_as_each_version_is_recorded() {
  let (mut store, history, _) = chain(5);
  let mut eager: PathMap<History> = PathMap::declare(EAGER_WINDOW, Mode::Eager).unwrap();
  let mut derivations = Derivations::new();
  for version in 0..5 {
    let mut fill = derivations
      .recorded(&mut store, &eager, version)
      .unwrap()
      .unwrap();
    let (answer, steps) = drive(
      &mut fill,
      &mut derivations,
      &mut eager,
      &history,
      &mut store,
    );
    fill.release(&mut store).unwrap();
    assert!(answer.is_ok());
    assert_eq!((steps, derivations.counters().derived), (2, version + 1));
  }
  for version in 0..5 {
    let read = derivations
      .read(&mut store, &eager, &history, version)
      .unwrap();
    assert!(matches!(read, Read::Ready(_)));
  }
}

/// A-647: values are immutable under (type, version). Do: fill version 2, then offer another map as version 2's value,
/// then the same value again. Expect: the other is refused `Diverged`, naming both roots, with the kept value unchanged;
/// the same is kept `Identical`.
#[test]
fn t_a647_a_value_kept_under_its_type_and_version_never_changes() {
  let (mut store, history, blobs) = chain(3);
  let mut lazy: PathMap<History> = PathMap::declare(LAZY_WINDOW, Mode::Lazy).unwrap();
  let mut derivations = Derivations::new();
  let kept = read_to_end(&mut derivations, &mut lazy, &history, &mut store, 2)
    .0
    .unwrap();
  let other_files = Files::from([(
    b"other".to_vec(),
    Entry {
      class: 0,
      content: blobs[0],
    },
  )]);
  let other = lazy.build(&mut store, &other_files).unwrap();
  let refused = derivations.keep(&mut store, &lazy, &history, 2, other);
  let diverged = DerivedRefused::Diverged {
    version: 2,
    kept,
    derived: other,
  };
  assert_eq!(refused, Err(diverged));
  assert_eq!(
    derivations.value(&store, &lazy, &history, 2),
    Ok(Some(kept))
  );
  let again = derivations.keep(&mut store, &lazy, &history, 2, kept);
  assert_eq!(
    (again, derivations.counters().identical),
    (Ok(Inserted::Identical), 1)
  );
  store.release(&other).unwrap();
}

/// A-647: the mechanism's refusals. Do: read a version the history lacks, one whose parent it lacks, one that is its own
/// ancestor, and fill under a budget with no room. Expect: `UnknownVersion` naming each missing version, `Cycle`, and
/// the store's `OverBudget`; nothing kept by any.
#[test]
fn t_a647_a_fill_refuses_what_is_no_history_typed() {
  let (mut store, mut history, _) = chain(1);
  history.record(5, &[77], Change::new());
  history.record(10, &[11], Change::new());
  history.record(11, &[10], Change::new());
  let mut lazy: PathMap<History> = PathMap::declare(LAZY_WINDOW, Mode::Lazy).unwrap();
  let mut derivations = Derivations::new();
  let mut answer =
    |version| read_to_end(&mut derivations, &mut lazy, &history, &mut store, version).0;
  assert_eq!(
    answer(99),
    Err(DerivedRefused::UnknownVersion { version: 99 })
  );
  assert_eq!(
    answer(5),
    Err(DerivedRefused::UnknownVersion { version: 77 })
  );
  assert_eq!(answer(10), Err(DerivedRefused::Cycle { version: 10 }));
  assert_eq!(derivations.len(), 0);
  store.set_budget(store.used());
  let refused = derivations.read(&mut store, &lazy, &history, 0);
  assert!(matches!(
    refused,
    Err(DerivedRefused::Content(ContentRefused::OverBudget { .. }))
  ));
}

/// A-647: a fork. Do: fill versions 0 to 3, fork the values, record version 4 over 3, read 3 and 4 in the fork and 4 in
/// the parent, then release both. Expect: the fork finds 3 kept with no derivation and derives only 4; the parent does
/// not see the fork's 4; once both are released only the files' contents are held.
#[test]
fn t_a647_a_fork_shares_the_values_kept_and_keeps_its_own() {
  let (mut store, mut history, blobs) = chain(4);
  let mut lazy: PathMap<History> = PathMap::declare(LAZY_WINDOW, Mode::Lazy).unwrap();
  let mut derivations = Derivations::new();
  read_to_end(&mut derivations, &mut lazy, &history, &mut store, 3)
    .0
    .unwrap();
  let mut fork = derivations.fork(&mut store).unwrap();
  let entry = Entry {
    class: 1,
    content: blobs[1],
  };
  history.record(4, &[3], Change::from([(b"g".to_vec(), Some(entry))]));
  let (three, steps) = read_to_end(&mut fork, &mut lazy, &history, &mut store, 3);
  assert_eq!((three.is_ok(), steps), (true, 0));
  let (four, _) = read_to_end(&mut fork, &mut lazy, &history, &mut store, 4);
  assert_eq!((four.is_ok(), fork.counters().derived), (true, 1));
  assert_eq!(derivations.value(&store, &lazy, &history, 4), Ok(None));
  fork.release(&mut store).unwrap();
  derivations.release(&mut store).unwrap();
  assert_eq!(store.len(), blobs.len());
}

/// A map of `count` files in the directory `w`, and a store to keep it in.
fn wide(count: usize) -> (ContentStore, Files, ContentId) {
  let mut store = ContentStore::new(BUDGET, STORE_WINDOW).unwrap();
  let blob = store.put(b"wide").unwrap();
  let files = (0..count)
    .map(|at| {
      let entry = Entry {
        class: 0,
        content: blob,
      };
      (format!("w/{at:03}").into_bytes(), entry)
    })
    .collect();
  (store, files, blob)
}

/// The nodes from the trie `root` down to the leaf holding `name`, read by the format's own rule (a branch's child for a
/// digit of the name's place), independently of the code that wrote them.
fn trie_depth(store: &ContentStore, root: ContentId, name: &[u8]) -> usize {
  let mut hasher = blake3::Hasher::new_derive_key(
    "slates-vcs 2026-11-19 derived paths: a name's place in its directory",
  );
  hasher.update(name);
  let place = *hasher.finalize().as_bytes();
  let (mut node, mut depth) = (root, 1);
  loop {
    let read = store.derived(&node).unwrap();
    if read.payload()[0] == b'L' {
      return depth;
    }
    let bitmap = u16::from_le_bytes([read.payload()[1], read.payload()[2]]);
    let byte = place[(depth - 1) / 2];
    let digit = if depth % 2 == 1 {
      byte >> 4
    } else {
      byte & 0x0f
    };
    let below = (bitmap & ((1_u16 << digit) - 1)).count_ones() as usize;
    node = read.name(below).unwrap();
    depth += 1;
  }
}

/// A-647, the non-vacuity of the increment inside a directory. Do: build a directory of 1000 files, which the eager
/// window splits into a trie, then change one file. Expect: the change puts the root's leaf and, in `w`, only the nodes
/// from its trie's root to the changed name's leaf (read by the format's own rule); the build put every node; the
/// change's value is the build of the changed files.
#[test]
fn t_a647_one_file_changed_in_a_wide_directory_makes_only_its_trie_path() {
  let (mut store, mut files, blob) = wide(1000);
  let mut map: PathMap<History> = PathMap::declare(EAGER_WINDOW, Mode::Eager).unwrap();
  let root = map.build(&mut store, &files).unwrap();
  let built = map.counters();
  let changed = Entry {
    class: 1,
    content: blob,
  };
  let change = Change::from([(b"w/123".to_vec(), Some(changed))]);
  let after = map.apply(&mut store, Some(root), &change).unwrap();
  let applied = map.counters();
  files.insert(b"w/123".to_vec(), changed);
  let w = store.derived(&after).unwrap().name(0).unwrap();
  let path = trie_depth(&store, w, b"123");
  assert!(path >= 3, "the directory is split two levels or more");
  assert_eq!(applied.nodes_put - built.nodes_put, 1 + path as u64);
  assert_eq!(applied.directories - built.directories, 2);
  assert!(built.nodes_put > 10 * (1 + path as u64));
  let naive = map.build(&mut store, &files).unwrap();
  assert_eq!(naive, after);
}

/// A-647, the canonical rule's other half. Do: build a directory of 40 files, which the eager window splits into a
/// trie, then remove 30 of them in one change. Expect: the directory's trie becomes one leaf, and the change's value is
/// the build of the 10 files left: a branch whose entries come to fit one leaf becomes one, or the same map would have
/// two shapes.
#[test]
fn t_a647_a_directory_that_comes_to_fit_one_leaf_becomes_one() {
  let (mut store, files, _) = wide(40);
  let mut map: PathMap<History> = PathMap::declare(EAGER_WINDOW, Mode::Eager).unwrap();
  let root = map.build(&mut store, &files).unwrap();
  let w = store.derived(&root).unwrap().name(0).unwrap();
  assert_eq!(store.derived(&w).unwrap().payload()[0], b'B');
  let change: Change = (10..40)
    .map(|at| (format!("w/{at:03}").into_bytes(), None))
    .collect();
  let after = map.apply(&mut store, Some(root), &change).unwrap();
  let w = store.derived(&after).unwrap().name(0).unwrap();
  assert_eq!(store.derived(&w).unwrap().payload()[0], b'L');
  let left: Files = files
    .into_iter()
    .filter(|(path, _)| path.as_slice() < &b"w/010"[..])
    .collect();
  assert_eq!(map.build(&mut store, &left).unwrap(), after);
}

/// A-647: the path map's refusals. Do: apply changes with a bad path, a removal of an absent path or of a directory, a
/// file beneath a file, a file over a directory, and a name past the window; declare a window smaller than a full
/// branch; derive over a parent that is no map's node. Expect: each refused by kind, naming the path or the name.
#[test]
fn t_a647_the_path_map_refuses_by_kind() {
  let (mut store, files, blob) = wide(2);
  let mut map: PathMap<History> = PathMap::declare(EAGER_WINDOW, Mode::Eager).unwrap();
  let root = map.build(&mut store, &files).unwrap();
  let entry = Entry {
    class: 0,
    content: blob,
  };
  let mut refused = |path: &[u8], set: Option<Entry>| {
    let change = Change::from([(path.to_vec(), set)]);
    map.apply(&mut store, Some(root), &change).unwrap_err()
  };
  let path = |bytes: &[u8]| bytes.to_vec();
  assert_eq!(
    refused(b"w//1", Some(entry)),
    PathsRefused::BadPath {
      path: path(b"w//1")
    }
  );
  assert_eq!(
    refused(b"w/..", Some(entry)),
    PathsRefused::BadPath {
      path: path(b"w/..")
    }
  );
  assert_eq!(
    refused(b"x", None),
    PathsRefused::RemovedAbsent { path: path(b"x") }
  );
  assert_eq!(
    refused(b"w", None),
    PathsRefused::RemovedAbsent { path: path(b"w") }
  );
  assert_eq!(
    refused(b"w/000/z", Some(entry)),
    PathsRefused::Shadowed {
      path: path(b"w/000")
    }
  );
  assert_eq!(
    refused(b"w", Some(entry)),
    PathsRefused::Shadowed { path: path(b"w") }
  );
  let long = vec![b'n'; EAGER_WINDOW];
  assert_eq!(
    refused(&long, Some(entry)),
    PathsRefused::EntryPastWindow { name: long.clone() }
  );
  let small = PathMap::<History>::declare(256, Mode::Lazy).unwrap_err();
  assert_eq!(small, PathsRefused::WindowTooSmall { window: 256 });
  let change = Change::from([(path(b"a"), Some(entry))]);
  let malformed = map.apply(&mut store, Some(blob), &change).unwrap_err();
  assert_eq!(malformed, PathsRefused::Malformed { node: blob });
}

/// The root a refused change leaves the store as it was: every hold it took given back.
#[test]
fn t_a647_a_refused_derivation_leaves_the_store_as_it_was() {
  let (mut store, files, blob) = wide(40);
  let mut map: PathMap<History> = PathMap::declare(EAGER_WINDOW, Mode::Eager).unwrap();
  let root = map.build(&mut store, &files).unwrap();
  let before = (
    store.len(),
    store.used(),
    store.holders(&root),
    store.holders(&blob),
  );
  let change = Change::from([
    (
      b"w/005".to_vec(),
      Some(Entry {
        class: 2,
        content: blob,
      }),
    ),
    (
      b"w/039/z".to_vec(),
      Some(Entry {
        class: 0,
        content: blob,
      }),
    ),
  ]);
  let refused = map.apply(&mut store, Some(root), &change).unwrap_err();
  assert_eq!(
    refused,
    PathsRefused::Shadowed {
      path: b"w/039".to_vec()
    }
  );
  assert_eq!(
    (
      store.len(),
      store.used(),
      store.holders(&root),
      store.holders(&blob)
    ),
    before
  );
}

/// A-647, the format's golden: a map of one file is one leaf whose bytes are the format's, written here by hand, and
/// whose identity is BLAKE3's key derivation over them under the derived node's context.
#[test]
fn t_a647_a_one_file_map_is_the_formats_bytes() {
  let mut store = ContentStore::new(BUDGET, STORE_WINDOW).unwrap();
  let blob = store.put(b"hello").unwrap();
  let mut map: PathMap<History> = PathMap::declare(EAGER_WINDOW, Mode::Eager).unwrap();
  let files = Files::from([(
    b"a".to_vec(),
    Entry {
      class: 1,
      content: blob,
    },
  )]);
  let root = map.build(&mut store, &files).unwrap();
  let mut expected = 1_u32.to_le_bytes().to_vec();
  expected.extend_from_slice(&blob.0);
  expected.extend_from_slice(b"L");
  expected.extend_from_slice(&1_u32.to_le_bytes());
  expected.extend_from_slice(b"a");
  expected.extend_from_slice(&[0, 1]);
  assert_eq!(store.get(&root), Some(&expected[..]));
  let mut hasher =
    blake3::Hasher::new_derive_key("slates-vcs 2026-11-19 object store: a derived value's node");
  hasher.update(&expected);
  assert_eq!(root.0, *hasher.finalize().as_bytes());
  assert_eq!(store.form(&root), Some(Form::Derived));
}

/// A-647: a derived node holds what it names. Do: name a window twice and an object's root once, release the caller's
/// holds, then the node. Expect: the node held each name once for each time it named it, kept them alive alone, and
/// freeing it freed all of them; a second put of the same node was held, not stored; a name the store lacks and a table
/// are refused with nothing changed.
#[test]
fn t_a647_a_derived_node_holds_what_it_names_and_frees_it_with_itself() {
  let mut store = ContentStore::new(BUDGET, 256).unwrap();
  let window = store.put(b"window").unwrap();
  let object = store.put_object(&vec![7_u8; 1000]).unwrap();
  let node = store
    .put_derived(&[window, object, window], b"payload")
    .unwrap();
  assert!(node.fresh);
  assert_eq!((store.holders(&window), store.holders(&object)), (3, 2));
  let again = store
    .put_derived(&[window, object, window], b"payload")
    .unwrap();
  assert_eq!((again.fresh, store.holders(&node.identity)), (false, 2));
  let table = store
    .put_tables(&[&b"slvcstb1 not opened"[..]], 0, 0)
    .unwrap()[0]
    .identity;
  let before = store.len();
  let unknown = ContentId([9; 32]);
  assert_eq!(
    store.put_derived(&[unknown], b""),
    Err(ContentRefused::NotHeld)
  );
  assert_eq!(
    store.put_derived(&[table], b""),
    Err(ContentRefused::NotNameable { identity: table })
  );
  assert_eq!(store.len(), before);
  store.release(&table).unwrap();
  store.release(&window).unwrap();
  store.release(&object).unwrap();
  store.release(&node.identity).unwrap();
  store.release(&node.identity).unwrap();
  assert!(store.is_empty());
}

/// A-647, hostile bytes: a derived node read from bytes that are not one is refused, never read past its end.
#[test]
fn t_a647_a_derived_node_read_from_hostile_bytes_is_refused() {
  assert!(DerivedNode::open(b"").is_none());
  assert!(DerivedNode::open(&[1, 0, 0]).is_none());
  assert!(DerivedNode::open(&1_u32.to_le_bytes()).is_none());
  assert!(DerivedNode::open(&u32::MAX.to_le_bytes()).is_none());
  let mut two = 2_u32.to_le_bytes().to_vec();
  two.extend_from_slice(&[0; 63]);
  assert!(DerivedNode::open(&two).is_none());
  two.push(0);
  let node = DerivedNode::open(&two).unwrap();
  assert_eq!((node.len(), node.payload().len()), (2, 0));
}
