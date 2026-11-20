//! The oracle and the determinism gate (A-664): generated histories of versions over nested and wide directories, roots,
//! first parents and merges' second parents, files of every class, versions a client pushed and versions the server
//! makes, and one hostile change in some. Each version's path map, trees and commit are derived as it is recorded,
//! through A-647's mechanism. Every version's trees are held to the trees built from scratch from the version's whole
//! path map and to the naive recomputation's id; each derivation's directories and hashed blobs are counted against its
//! change's own; each commit is held to the client's or to the naive `git commit-tree -m`; each hostile change is
//! refused by its kind with the store as it was. Then the same history is derived again, lazily and newest first, in
//! another store, and every value and every node it names is the same, byte for byte.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::TestRunner;
use slates_vcs::derived::RefusedOf;
use slates_vcs::derived::paths::{Change, Entry, PathMap};
use slates_vcs::{
  ContentId, ContentStore, Derivations, DerivedRefused, DerivedType, Fill, Mode as Computed,
  Progress, Read,
};

use super::super::{Commits, Trees, TreesRefused};
use super::{Commit, FORMATS, Files, History, WINDOW, held, naive_commit, naive_tree, store};
use crate::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use crate::objects::fsck::FsckId;

/// A fill's answer and the steps it took.
type Answer<T> = (Result<ContentId, RefusedOf<T>>, u64);

/// Drives `fill` to its end, a step at a time; its answer, the fill released.
fn drive<T: DerivedType<Source = History>>(
  mut fill: Fill<u64>,
  derivations: &mut Derivations,
  ty: &mut T,
  history: &History,
  store: &mut ContentStore,
) -> Answer<T> {
  let mut steps = 0_u64;
  let answer = loop {
    steps += 1;
    assert!(steps < 1_000_000, "a fill that never ends");
    match fill.step(derivations, ty, history, store) {
      Ok(Progress::More) => {}
      Ok(Progress::Done(root)) => break Ok(root),
      Err(refused) => break Err(refused),
    }
  };
  fill.release(store).unwrap();
  (answer, steps)
}

/// `version`'s value under the eager `ty`, filled as it is recorded.
pub(super) fn fill_recorded<T: DerivedType<Source = History>>(
  derivations: &mut Derivations,
  ty: &mut T,
  history: &History,
  store: &mut ContentStore,
  version: u64,
) -> Answer<T> {
  match derivations.recorded(store, ty, version) {
    Ok(Some(fill)) => drive(fill, derivations, ty, history, store),
    Ok(None) => panic!("an eager type fills each version as it is recorded"),
    Err(refused) => (Err(refused), 0),
  }
}

/// `version`'s value under `ty`, read, and filled if it must be.
fn read_value<T: DerivedType<Source = History>>(
  derivations: &mut Derivations,
  ty: &mut T,
  history: &History,
  store: &mut ContentStore,
  version: u64,
) -> Answer<T> {
  match derivations.read(store, ty, history, version) {
    Ok(Read::Ready(root)) => (Ok(root), 0),
    Ok(Read::Fill(fill)) => drive(fill, derivations, ty, history, store),
    Err(refused) => (Err(refused), 0),
  }
}

/// Shape: the paths a recording sets and removes: names that bytes and git order differently (`a`, `a.b`, `a-b`, `a0`
/// and the directory `src/a` beside `src/a.rs`), nested directories, and no path a directory of another.
const PATHS: [&str; 14] = [
  "README",
  "a",
  "a.b",
  "a-b",
  "a0",
  "src/main.rs",
  "src/lib.rs",
  "src/a/b/c.rs",
  "src/a/d",
  "src/a.rs",
  "docs/x.md",
  "docs/y/z.md",
  "Makefile",
  "e/f/g/h/i",
];

/// Shape: the wide directory's subdirectories, `wide/s<n>/f`: past the 29 a directory's node names in the tests' window.
const WIDE: usize = 40;

/// The contents a recording's files hold: an empty file, two lines, a binary file, and one past three windows.
fn contents() -> Vec<Vec<u8>> {
  let long: Vec<u8> = (0..3000_u32)
    .map(|at| u8::try_from(at % 253).unwrap())
    .collect();
  vec![
    Vec::new(),
    b"one\n".to_vec(),
    b"two\n".to_vec(),
    vec![0, 1, 0xff, 0x0a, 0],
    long,
  ]
}

/// Shape: the identities the server's commits are made of.
const IDENTITIES: [(&str, &str); 3] = [
  ("A U Thor", "author@example.com"),
  ("\u{c1}d\u{e1} L\u{fc}ndh\u{e9}", "ada@example.com"),
  ("C O Mitter", ""),
];

/// Shape: the messages they carry: none, one line not ended, a subject and a body, blank lines at the end, UTF-8.
const MESSAGES: [&str; 5] = [
  "",
  "one line",
  "a subject\n\na body\n",
  "ends in blank lines\n\n\n",
  "na\u{ef}ve \u{2713}",
];

/// Shape: the hostile kinds a recording draws from 0 up to its selector's range: below it, one hostile change; past it,
/// none.
const HOSTILE_KINDS: u8 = 8;

/// One version to record: its parents (each a pick among the usable versions, or none), its sets (a path, a content and
/// a class) and removals, a run over the wide directory (none, sets or removals; its first and its length), how its
/// commit is had (pushed, or made of an identity and a message), and a hostile change (a kind past the hostile ones is
/// none).
#[derive(Clone, Debug)]
struct Recording {
  first: (bool, usize),
  second: (bool, usize),
  sets: Vec<(usize, usize, u8)>,
  removes: Vec<usize>,
  run: (u8, usize, usize),
  commit: (bool, usize, usize),
  hostile: (u8, usize),
}

/// A recording, generated as plain tuples (no `Arc`-backed `prop_oneof!`, R2): a hostile change in about one in four.
fn recording() -> impl Strategy<Value = Recording> {
  (
    (proptest::bool::weighted(0.9), 0_usize..64),
    (proptest::bool::weighted(0.2), 0_usize..64),
    proptest::collection::vec((0..PATHS.len(), 0_usize..5, 0_u8..4), 0..8),
    proptest::collection::vec(0_usize..64, 0..4),
    (0_u8..4, 0..WIDE, 1..WIDE),
    (
      proptest::bool::weighted(0.3),
      0..IDENTITIES.len(),
      0..MESSAGES.len(),
    ),
    (0_u8..HOSTILE_KINDS * 4, 0_usize..64),
  )
    .prop_map(
      |(first, second, sets, removes, run, commit, hostile)| Recording {
        first,
        second,
        sets,
        removes,
        run,
        commit,
        hostile,
      },
    )
}

/// What the run met, every case summed: its evidence that every path of the types was taken.
#[derive(Clone, Debug, Default)]
struct Census {
  versions: u64,
  merges: u64,
  pushed: u64,
  made: u64,
  lists: u64,
  widest: u64,
  most_directories: u64,
  refused: BTreeSet<u8>,
}

impl Census {
  fn add(&mut self, other: &Census) {
    self.versions += other.versions;
    self.merges += other.merges;
    self.pushed += other.pushed;
    self.made += other.made;
    self.lists += other.lists;
    self.widest = self.widest.max(other.widest);
    self.most_directories = self.most_directories.max(other.most_directories);
    self.refused.extend(&other.refused);
  }

  /// Every kind of version and refusal was met, lists were made, and a derivation made six directories or more.
  fn assert_met(&self) {
    assert!(
      self.merges > 0 && self.pushed > 0 && self.made > 0,
      "{self:?}"
    );
    assert!(
      self.lists > 0 && self.widest > 29 && self.most_directories >= 6,
      "{self:?}"
    );
    assert_eq!(self.refused.len(), usize::from(HOSTILE_KINDS), "{self:?}");
  }
}

/// A usable version as the model keeps it: its files, its commit's id, and its values' roots.
#[derive(Clone, Debug)]
struct Model {
  files: Files,
  commit: ObjectId,
  tree: ContentId,
  commit_root: ContentId,
}

/// One case's run in one format.
struct Run {
  format: ObjectFormat,
  store: ContentStore,
  history: History,
  derivations: Derivations,
  paths: PathMap<History>,
  trees: Trees<History>,
  commits: Commits<History>,
  models: BTreeMap<u64, Model>,
  usable: Vec<u64>,
  contents: Vec<Vec<u8>>,
  not_a_file: ContentId,
  census: Census,
}

/// The directories a derivation of `change` makes again: the root and every directory on its paths; none for no change.
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

/// The blobs a derivation of `change` hashes: one for each file it sets that is no submodule link.
fn hashed(change: &Change) -> u64 {
  change
    .values()
    .filter(|set| set.is_some_and(|entry| entry.class != 3))
    .count() as u64
}

/// The directories on the paths of `files`: each path's proper prefixes.
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

impl Run {
  fn new(format: ObjectFormat) -> Run {
    let mut store = store();
    let not_a_file = store.put_derived(&[], b"no file's bytes").unwrap().identity;
    Run {
      format,
      paths: PathMap::declare(WINDOW, Computed::Eager).unwrap(),
      trees: Trees::declare(format, WINDOW, Computed::Eager).unwrap(),
      commits: Commits::declare(format, WINDOW, Computed::Eager),
      store,
      history: History::default(),
      derivations: Derivations::new(),
      models: BTreeMap::new(),
      usable: Vec::new(),
      contents: contents(),
      not_a_file,
      census: Census::default(),
    }
  }

  /// The path map's entry of a file of `class`, its content `pick` (a submodule link's, the format's id).
  fn entry(&mut self, class: u8, pick: usize) -> (Entry, Vec<u8>) {
    let bytes = match class {
      3 => vec![0x5a; self.format.raw_len()],
      _ => self.contents[pick % self.contents.len()].clone(),
    };
    let content = self.store.put_object(&bytes).unwrap();
    (Entry { class, content }, bytes)
  }

  /// The parents a recording picks among the usable versions: none for a root, a second for a merge.
  fn parents(&self, recording: &Recording) -> Vec<u64> {
    let pick = |at: usize| self.usable[at % self.usable.len()];
    let mut parents = Vec::new();
    if recording.first.0 && !self.usable.is_empty() {
      parents.push(pick(recording.first.1));
      let second = pick(recording.second.1);
      if recording.second.0 && second != parents[0] {
        parents.push(second);
      }
    }
    parents
  }

  /// Records one version: a hostile change refused, or the change made and every value held to its model.
  fn record(&mut self, recording: &Recording) {
    let version = self.history.versions.len() as u64;
    let parents = self.parents(recording);
    let before = parents
      .first()
      .map(|parent| self.models[parent].files.clone())
      .unwrap_or_default();
    match self.hostile(recording, &before) {
      Some((change, refusal)) => self.refused(version, &parents, change, refusal),
      None => {
        let (change, after) = self.change(recording, &before);
        self.made(version, &parents, (change, after), recording);
      }
    }
  }

  /// A recording's change over `before`, and the files after it: its removals, then its sets, then its run.
  fn change(&mut self, recording: &Recording, before: &Files) -> (Change, Files) {
    let mut change = Change::new();
    let mut after = before.clone();
    let present: Vec<Vec<u8>> = before.keys().cloned().collect();
    for pick in &recording.removes {
      if let Some(path) = present.get(pick % present.len().max(1)) {
        change.insert(path.clone(), None);
        after.remove(path);
      }
    }
    for (path, pick, class) in &recording.sets {
      self.set(
        PATHS[*path].as_bytes(),
        (*class, *pick),
        (&mut change, &mut after),
      );
    }
    let (kind, first, len) = recording.run;
    for at in (first..first + len).map(|at| at % WIDE) {
      let path = format!("wide/s{at:02}/f").into_bytes();
      match kind {
        1 => self.set(&path, (0, first), (&mut change, &mut after)),
        2 if after.contains_key(&path) => {
          change.insert(path.clone(), None);
          after.remove(&path);
        }
        _ => {}
      }
    }
    (change, after)
  }

  /// Sets `path` to a file of `class` and content `pick` in `change` and `after`.
  fn set(
    &mut self,
    path: &[u8],
    (class, pick): (u8, usize),
    (change, after): (&mut Change, &mut Files),
  ) {
    let (entry, bytes) = self.entry(class, pick);
    change.insert(path.to_vec(), Some(entry));
    after.insert(path.to_vec(), (class, bytes));
  }

  /// A hostile change over `before` and the refusal it meets, when the recording draws one; the census counts the kind
  /// met (a file over a directory, or beneath a file, needs one to stand, else it is the removal of an absent file).
  fn hostile(&mut self, recording: &Recording, before: &Files) -> Option<(Change, TreesRefused)> {
    let (kind, pick) = recording.hostile;
    if kind >= HOSTILE_KINDS {
      return None;
    }
    let one = self.store.put(b"one\n").unwrap();
    let files: Vec<Vec<u8>> = before.keys().cloned().collect();
    let directories = directories_of(before);
    let set = |path: &[u8], class: u8, content: ContentId| {
      Change::from([(path.to_vec(), Some(Entry { class, content }))])
    };
    let (met, change, refusal) = match kind {
      1 if !directories.is_empty() => {
        let directory = directories[pick % directories.len()].clone();
        let change = set(&directory, 0, one);
        (1, change, TreesRefused::Shadowed { path: directory })
      }
      2 if !files.is_empty() => {
        let file = files[pick % files.len()].clone();
        let change = set(&[file.as_slice(), b"/under"].concat(), 0, one);
        (2, change, TreesRefused::Shadowed { path: file })
      }
      3 => {
        let path = b"nu\0l".to_vec();
        (3, set(&path, 0, one), TreesRefused::BadPath { path })
      }
      4 => {
        let finding = FsckId::HasDotgit;
        let path = b"src".to_vec();
        (
          4,
          set(b"src/.git", 0, one),
          TreesRefused::Fsck { path, finding },
        )
      }
      5 => {
        let path = PATHS[pick % PATHS.len()].as_bytes().to_vec();
        (
          5,
          set(&path, 7, one),
          TreesRefused::UnknownClass { path, class: 7 },
        )
      }
      6 => {
        let short = self.store.put(b"short").unwrap();
        let path = b"m".to_vec();
        (6, set(&path, 3, short), TreesRefused::BadGitlink { path })
      }
      7 => {
        let path = b"d".to_vec();
        (
          7,
          set(&path, 0, self.not_a_file),
          TreesRefused::NotAFile { path },
        )
      }
      _ => {
        let path = format!("absent/{pick}").into_bytes();
        let change = Change::from([(path.clone(), None)]);
        (0, change, TreesRefused::RemovedAbsent { path })
      }
    };
    self.census.refused.insert(met);
    Some((change, refusal))
  }

  /// A hostile change: the path map takes it or not, as its own rules say; the trees refuse it by `refusal`, the store
  /// as it was; and the version is never a parent.
  fn refused(&mut self, version: u64, parents: &[u64], change: Change, refusal: TreesRefused) {
    let commit = Commit::Pushed(ObjectId::null(self.format));
    self.history.record(version, parents, change, commit);
    // The path map's own rules decide whether it takes the change (a NUL, a class or a content is opaque to it); what
    // it answers is A-647's to test, and either way the trees must refuse.
    let (_, _) = fill_recorded(
      &mut self.derivations,
      &mut self.paths,
      &self.history,
      &mut self.store,
      version,
    );
    let before = held(&self.store);
    let (answer, _) = fill_recorded(
      &mut self.derivations,
      &mut self.trees,
      &self.history,
      &mut self.store,
      version,
    );
    let expected = DerivedRefused::Type {
      version,
      refused: refusal,
    };
    assert_eq!(answer, Err(expected));
    assert_eq!(
      held(&self.store),
      before,
      "a refusal leaves the store as it was"
    );
  }

  /// A version made: its path map, its trees and its commit derived, and each held to the model.
  fn made(
    &mut self,
    version: u64,
    parents: &[u64],
    (change, after): (Change, Files),
    recording: &Recording,
  ) {
    let commit = self.commit_of(version, recording);
    self
      .history
      .record(version, parents, change.clone(), commit.clone());
    let (paths, _) = fill_recorded(
      &mut self.derivations,
      &mut self.paths,
      &self.history,
      &mut self.store,
      version,
    );
    let paths = paths.unwrap();
    let before = self.trees.counters();
    let (tree, _) = fill_recorded(
      &mut self.derivations,
      &mut self.trees,
      &self.history,
      &mut self.store,
      version,
    );
    let tree = tree.unwrap();
    let counted = self.trees.counters();
    let moved = (
      counted.directories - before.directories,
      counted.blobs - before.blobs,
    );
    assert_eq!(
      moved,
      (reached(&change), hashed(&change)),
      "its change's own"
    );
    self.census.lists += counted.lists - before.lists;
    self.census.most_directories = self.census.most_directories.max(reached(&change));
    self.same_as_the_path_map_built(paths, tree, &after);
    self.history.trees.insert(version, tree);
    let (commit_root, _) = fill_recorded(
      &mut self.derivations,
      &mut self.commits,
      &self.history,
      &mut self.store,
      version,
    );
    let commit_root = commit_root.unwrap();
    let id = self.same_commit(commit_root, (&commit, parents), &after);
    self.census_of(parents, &after, &commit);
    self.models.insert(
      version,
      Model {
        files: after,
        commit: id,
        tree,
        commit_root,
      },
    );
    self.usable.push(version);
  }

  /// The trees `tree` are the trees built from scratch from the version's whole path map, `paths`, identity for
  /// identity, and their id the naive recomputation's from the model's files.
  fn same_as_the_path_map_built(&mut self, paths: ContentId, tree: ContentId, files: &Files) {
    let whole = self.paths.files(&self.store, paths).unwrap();
    let built = self.trees.build(&mut self.store, &whole).unwrap();
    assert_eq!(
      built, tree,
      "the derived trees are the trees built from the whole path map"
    );
    assert_eq!(
      self.trees.id(&self.store, tree).unwrap(),
      naive_tree(self.format, files)
    );
    self.store.release(&built).unwrap();
  }

  /// How a recording's version's commit is had.
  fn commit_of(&self, version: u64, recording: &Recording) -> Commit {
    let (pushed, identity, message) = recording.commit;
    if pushed {
      let made = format!("a commit a client made: {version}");
      return Commit::Pushed(name_of(self.format, Kind::Commit, made.as_bytes()).unwrap());
    }
    let (name, email) = IDENTITIES[identity];
    Commit::Server {
      name: name.to_owned(),
      email: email.to_owned(),
      seconds: 1_112_911_993 + version,
      message: MESSAGES[message].to_owned(),
    }
  }

  /// The commit `root` is the client's for a pushed version, and the naive `git commit-tree -m` commit of the version's
  /// tree over its parents' commits for a made one, bytes and id. Its id.
  fn same_commit(
    &self,
    root: ContentId,
    (commit, parents): (&Commit, &[u64]),
    files: &Files,
  ) -> ObjectId {
    let id = self.commits.id(&self.store, root).unwrap();
    let object = self.commits.object(&self.store, root).unwrap();
    match commit {
      Commit::Pushed(pushed) => assert_eq!((id, object), (*pushed, None)),
      Commit::Server {
        name,
        email,
        seconds,
        message,
      } => {
        let parents: Vec<ObjectId> = parents
          .iter()
          .map(|parent| self.models[parent].commit)
          .collect();
        let naive = naive_commit(
          self.format,
          (naive_tree(self.format, files), &parents),
          (name, email, *seconds),
          message,
        );
        assert_eq!((id, object), (naive.0, Some(naive.1)));
      }
    }
    id
  }

  /// Counts what a made version was.
  fn census_of(&mut self, parents: &[u64], files: &Files, commit: &Commit) {
    self.census.versions += 1;
    self.census.merges += u64::from(parents.len() > 1);
    match commit {
      Commit::Pushed(_) => self.census.pushed += 1,
      Commit::Server { .. } => self.census.made += 1,
    }
    let wide = files
      .keys()
      .filter(|path| path.starts_with(b"wide/"))
      .count() as u64;
    self.census.widest = self.census.widest.max(wide);
  }
}

/// A-664 (the oracle; §4.17.2 git piece 13's trees "derived from the version's path map"; A-647's mechanism). Do: record
/// generated histories in both formats (roots, first parents and merges' second parents; sets and removals over nested
/// directories and names that bytes and git order differently; runs over a directory of 40 subdirectories, past what one
/// node names; files of every class, one past three windows; versions a client pushed and versions the server makes;
/// and one hostile change in about one recording in four), each version's path map, trees and commit derived as it is
/// recorded. Expect: every version's trees are, identity for identity, the trees built from scratch from its whole path
/// map, and its tree id the naive recomputation's; each derivation made exactly its change's directories and hashed
/// exactly the files it set (the non-vacuity counters); each commit is the client's or the naive `git commit-tree -m`
/// commit of the version's tree over its parents' commits; each hostile change is refused by its kind, the store as it
/// was; and the census met merges, pushed and made versions, lists, a derivation six directories deep, and every kind.
#[test]
#[allow(
  clippy::print_stderr,
  reason = "the census is the oracle's evidence, printed with every run"
)]
fn t_a664_the_derived_trees_are_the_trees_built_from_the_whole_path_map_at_every_version() {
  let census = RefCell::new(Census::default());
  let config = ProptestConfig {
    cases: slates_test_seeds::cases(48),
    ..ProptestConfig::default()
  };
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(config));
  let result = runner.run(
    &proptest::collection::vec(recording(), 1..32),
    |recordings| {
      for format in FORMATS {
        let mut run = Run::new(format);
        recordings
          .iter()
          .for_each(|recording| run.record(recording));
        census.borrow_mut().add(&run.census);
      }
      Ok(())
    },
  );
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  eprintln!("A-664 oracle census: {census:?}");
  census.assert_met();
}

/// Every node and content reachable from `root` holds the same bytes in `first` and `second`.
fn same_bytes(first: &ContentStore, second: &ContentStore, root: ContentId) {
  let mut pending = vec![root];
  let mut seen = BTreeSet::new();
  while let Some(id) = pending.pop() {
    if !seen.insert(id) {
      continue;
    }
    assert_eq!(
      first.bytes_of(&id),
      second.bytes_of(&id),
      "the same bytes under one identity"
    );
    assert!(first.bytes_of(&id).is_some(), "a node's names are held");
    if let Some(node) = first.derived(&id) {
      pending.extend(node.names());
    }
  }
}

impl Run {
  /// The determinism gate: the run's usable versions derived again in a store of their contents alone, lazily, newest
  /// first, trees and then commits; every value the same root, and every node it names the same bytes.
  fn derived_again(&self) {
    let mut store = store();
    let named = self
      .history
      .versions
      .values()
      .flat_map(|recorded| recorded.change.values().flatten());
    for entry in named {
      let bytes: Vec<u8> = self
        .store
        .windows(&entry.content)
        .flat_map(|window| window.into_owned())
        .collect();
      store.put_object(&bytes).unwrap();
    }
    let mut derivations = Derivations::new();
    let mut trees: Trees<History> = Trees::declare(self.format, WINDOW, Computed::Lazy).unwrap();
    let mut commits: Commits<History> = Commits::declare(self.format, WINDOW, Computed::Lazy);
    for version in self.usable.iter().rev() {
      let (again, _) = read_value(
        &mut derivations,
        &mut trees,
        &self.history,
        &mut store,
        *version,
      );
      same_again((&self.store, &store), again, self.models[version].tree);
    }
    for version in self.usable.iter().rev() {
      let (again, _) = read_value(
        &mut derivations,
        &mut commits,
        &self.history,
        &mut store,
        *version,
      );
      same_again(
        (&self.store, &store),
        again,
        self.models[version].commit_root,
      );
    }
  }
}

/// A value derived again, `again`, is `root`, and every node and content it names holds the same bytes in both stores.
fn same_again<E: std::fmt::Debug + PartialEq>(
  (first, second): (&ContentStore, &ContentStore),
  again: Result<ContentId, E>,
  root: ContentId,
) {
  assert_eq!(again, Ok(root), "a value derived again is the same root");
  same_bytes(first, second, root);
}

/// A-664 (the determinism gate; §4.16 "Apply on holders": every holder recomputes the version from the record). Do:
/// record generated histories in both formats, each version's values derived eagerly as it is recorded, then derive
/// every usable version again in a second store holding only the files' contents, lazily and newest first: the trees,
/// then the commits. Expect: every version's trees and commit the same root identity in both stores, and every node and
/// content they name the same bytes, byte for byte.
#[test]
fn t_a664_every_value_derived_again_elsewhere_is_the_same_byte_for_byte() {
  let config = ProptestConfig {
    cases: slates_test_seeds::cases(24),
    ..ProptestConfig::default()
  };
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(config));
  let result = runner.run(
    &proptest::collection::vec(recording(), 1..24),
    |recordings| {
      for format in FORMATS {
        let mut run = Run::new(format);
        recordings
          .iter()
          .for_each(|recording| run.record(recording));
        run.derived_again();
      }
      Ok(())
    },
  );
  if let Err(failure) = result {
    panic!("{failure}");
  }
}
