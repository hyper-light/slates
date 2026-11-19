//! A-666 part 3 held to its definition (module docs): every blame equals a replay of the whole history by the rule, each
//! line through the first candidate whose diff keeps it; every attribution names a line of the same text where it says
//! it was introduced; the diff keeps a longest common subsequence; every derivation reads its changed files and their
//! candidates' alone; a line both parents of a merge changed is the merge's own unless it recorded a parent's text; the
//! format's bytes; hostile bytes and refusals, typed.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::TestRunner;

use super::{Blame, BlameRefused, Span, blame, decode_spans, encode_spans, matched, read_node};
use crate::content::{ContentId, ContentRefused, ContentStore};
use crate::derived::history::{PathSources, Sources};
use crate::derived::paths::{Change, Entry, PathChanges, PathsRefused};
use crate::derived::{Derivations, DerivedType, Fill, Graph, Mode, Progress, Read};

/// Shape: the store's budget in these tests: room for every history and question they make.
const BUDGET: u64 = 1 << 30;

/// Shape: the store's window and the type's.
const WINDOW: usize = 1024;

/// Shape: the most versions a generated history holds.
const MAX_VERSIONS: usize = 160;

/// Shape: the most branches a generated history keeps open.
const MAX_TIPS: usize = 5;

/// Shape: the distinct lines a generated file draws from: few, so lines repeat and a diff has ties to break.
const TOKENS: usize = 6;

/// Shape: the most lines a generated file holds, so every file fits one window.
const MAX_LINES: usize = 24;

/// Shape: the first-parent steps back a generated copy may take its source from.
const COPY_BACK: usize = 5;

/// Shape: the long file's lines, past the window many times over.
const LONG_LINES: usize = 2_000;

/// Shape: the versions that each edit one line of the long file.
const LONG_EDITS: u64 = 300;

/// A history: each version's parents, change and renames and copies.
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

fn index(version: u64) -> usize {
  usize::try_from(version).unwrap()
}

fn number(index: usize) -> u64 {
  u64::try_from(index).unwrap()
}

/// A version's files: each path's bytes.
type Files = BTreeMap<Vec<u8>, Vec<u8>>;

/// A file's lines, by definition: each up to and including a newline, the last without one.
fn lines(bytes: &[u8]) -> Vec<&[u8]> {
  bytes.split_inclusive(|byte| *byte == b'\n').collect()
}

/// The bytes of lines of `tokens`, the last without its newline when `open`.
fn text(tokens: &[usize], open: bool) -> Vec<u8> {
  let mut bytes: Vec<u8> = tokens
    .iter()
    .flat_map(|token| format!("line {token}\n").into_bytes())
    .collect();
  if open {
    bytes.pop();
  }
  bytes
}

/// A history being built, with each version's files; every file's bytes are its content's.
struct Builder {
  history: History,
  maps: Vec<Files>,
  scratch: ContentStore,
}

impl Builder {
  fn new() -> Builder {
    Builder {
      history: History::default(),
      maps: Vec::new(),
      scratch: ContentStore::new(BUDGET, WINDOW).unwrap(),
    }
  }

  /// The content of `bytes` as the store names it.
  fn content(&self, bytes: &[u8]) -> ContentId {
    self.scratch.object_id(bytes)
  }

  /// Records a version of `parents` whose files are `map`, its change stated against its first parent's.
  fn record(&mut self, parents: &[u64], map: Files, sources: Sources<u64>) -> u64 {
    let base = parents
      .first()
      .map(|parent| self.maps[index(*parent)].clone())
      .unwrap_or_default();
    let mut change = Change::new();
    for path in base.keys().filter(|path| !map.contains_key(*path)) {
      change.insert(path.clone(), None);
    }
    for (path, bytes) in &map {
      if base.get(path) != Some(bytes) {
        let entry = Entry {
          class: 0,
          content: self.content(bytes),
        };
        change.insert(path.clone(), Some(entry));
      }
    }
    self.history.changes.push(change);
    self.history.parents.push(parents.to_vec());
    self.history.sources.push(sources);
    self.maps.push(map);
    number(self.maps.len() - 1)
  }

  fn map(&self, version: u64) -> Files {
    self.maps[index(version)].clone()
  }

  /// A version on `parent` (or a root) setting each path to its text, or removing it (`None`).
  fn edit(&mut self, parent: Option<u64>, edits: &[(&str, Option<&str>)]) -> u64 {
    let mut map = parent.map(|parent| self.map(parent)).unwrap_or_default();
    for (path, text) in edits {
      match text {
        Some(text) => map.insert(path.as_bytes().to_vec(), text.as_bytes().to_vec()),
        None => map.remove(path.as_bytes()),
      };
    }
    self.record(parent.as_slice(), map, Sources::new())
  }

  /// A version on `parent` bringing the file `from` of `source` to `to` (a rename when `remove`), with `bytes` instead
  /// of the source's when given.
  fn bring(
    &mut self,
    parent: u64,
    (from, source): (&str, u64),
    to: &str,
    (remove, bytes): (bool, Option<Vec<u8>>),
  ) -> u64 {
    let mut map = self.map(parent);
    if remove {
      map.remove(from.as_bytes());
    }
    let brought = bytes.unwrap_or_else(|| self.maps[index(source)][from.as_bytes()].clone());
    map.insert(to.as_bytes().to_vec(), brought);
    let sources: Sources<u64> =
      [(to.as_bytes().to_vec(), (from.as_bytes().to_vec(), source))].into();
    self.record(&[parent], map, sources)
  }

  /// Every distinct file content the history holds.
  fn contents(&self) -> BTreeSet<Vec<u8>> {
    self
      .maps
      .iter()
      .flat_map(|map| map.values().cloned())
      .collect()
  }
}

/// A generated step: a selector and three picks (see [`Grower::apply`]).
type Op = (u8, usize, usize, usize);

fn op() -> impl Strategy<Value = Op> {
  (0_u8..16, any::<usize>(), any::<usize>(), any::<usize>())
}

/// The paths the generated histories use: three at the root and two beneath a directory.
const PATHS: [&str; 5] = ["a", "b", "c", "e/x", "e/y"];

/// Grows a history of files of few distinct lines from generated steps: lines inserted, removed and replaced, files
/// made and removed, a file's last newline dropped and restored, renames (some edited as they move), copies from the
/// first parent and from older versions, merges of two and three keeping, taking or resolving each differing file
/// line by line, criss-cross merges and unrelated roots.
struct Grower {
  built: Builder,
  tips: Vec<u64>,
}

impl Grower {
  fn new() -> Grower {
    let mut built = Builder::new();
    let first: Files = [(b"a".to_vec(), text(&[0, 1, 2, 3], false))].into();
    let root = built.record(&[], first, Sources::new());
    Grower {
      built,
      tips: vec![root],
    }
  }

  fn room(&self) -> usize {
    MAX_VERSIONS.saturating_sub(self.built.maps.len())
  }

  fn tip(&self, pick: usize) -> (usize, u64) {
    let at = pick % self.tips.len();
    (at, self.tips[at])
  }

  /// `bytes` edited by `pick`: a line inserted, removed or replaced, or the last newline dropped or restored.
  fn edited(bytes: &[u8], pick: usize) -> Vec<u8> {
    let mut tokens: Vec<Vec<u8>> = lines(bytes).iter().map(|line| line.to_vec()).collect();
    let at = (pick / 5) % (tokens.len() + 1);
    let line = format!("line {}\n", pick / 7 % TOKENS).into_bytes();
    match pick % 5 {
      0 if tokens.len() < MAX_LINES => tokens.insert(at, line),
      1 if at < tokens.len() => {
        tokens.remove(at);
      }
      2 if at < tokens.len() => tokens[at] = line,
      3 => match tokens.last_mut() {
        Some(last) if last.ends_with(b"\n") => {
          last.pop();
        }
        Some(last) => last.push(b'\n'),
        None => tokens.push(line),
      },
      _ => tokens.push(line),
    }
    let mut bytes: Vec<u8> = Vec::new();
    for (at, token) in tokens.iter().enumerate() {
      bytes.extend_from_slice(token);
      if at + 1 < tokens.len() && !token.ends_with(b"\n") {
        bytes.push(b'\n');
      }
    }
    bytes
  }

  fn edit(&mut self, one: usize, two: usize, three: usize) {
    let (at, tip) = self.tip(one);
    let mut map = self.built.map(tip);
    let path = PATHS[two % PATHS.len()].as_bytes().to_vec();
    match map.get(&path) {
      Some(_) if three % 11 == 0 => {
        map.remove(&path);
      }
      Some(bytes) => {
        let bytes = Grower::edited(bytes, three);
        map.insert(path, bytes);
      }
      None => {
        map.insert(
          path,
          text(&[three % TOKENS, three / 3 % TOKENS], three % 2 == 0),
        );
      }
    }
    self.tips[at] = self.built.record(&[tip], map, Sources::new());
  }

  /// A file of `version`, by pick, and a path free on the tip, by pick.
  fn pick(
    &self,
    (version, tip): (u64, u64),
    (from, to): (usize, usize),
  ) -> Option<(String, String)> {
    let files: Vec<&Vec<u8>> = self.built.maps[index(version)].keys().collect();
    let from = String::from_utf8(files.get(from % files.len().max(1))?.to_vec()).ok()?;
    let to = PATHS[to % PATHS.len()];
    let free = !self.built.maps[index(tip)].contains_key(to.as_bytes());
    (free && to != from).then(|| (from, to.to_owned()))
  }

  fn rename(&mut self, one: usize, two: usize, three: usize) {
    let (at, tip) = self.tip(one);
    let Some((from, to)) = self.pick((tip, tip), (two, three)) else {
      return;
    };
    let edited = (three % 2 == 1)
      .then(|| Grower::edited(&self.built.maps[index(tip)][from.as_bytes()], three / 2));
    self.tips[at] = self.built.bring(tip, (&from, tip), &to, (true, edited));
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
    let Some((from, to)) = self.pick((source, tip), (three, three / 5)) else {
      return;
    };
    self.tips[at] = self.built.bring(tip, (&from, source), &to, (false, None));
  }

  /// A merge's files over `ours`: each differing file kept as ours, taken as theirs, or resolved line by line (ours
  /// with theirs' lines after it, or ours edited), as `bits` pick, two bits a file.
  fn merged(ours: &Files, theirs: &Files, bits: usize) -> Files {
    let differing: BTreeSet<&Vec<u8>> = ours
      .keys()
      .chain(theirs.keys())
      .filter(|path| ours.get(*path) != theirs.get(*path))
      .collect();
    let mut result = ours.clone();
    for (at, path) in differing.into_iter().enumerate() {
      let choice = bits.rotate_right(u32::try_from(2 * (at % 32)).unwrap()) & 3;
      match (choice, ours.get(path), theirs.get(path)) {
        (1, _, Some(bytes)) => {
          result.insert(path.clone(), bytes.clone());
        }
        (1, _, None) => {
          result.remove(path);
        }
        (2, Some(mine), Some(yours)) => {
          let mut both = mine.clone();
          if !both.ends_with(b"\n") {
            both.push(b'\n');
          }
          both.extend_from_slice(yours);
          result.insert(path.clone(), both);
        }
        (2, Some(mine), None) => {
          result.insert(path.clone(), Grower::edited(mine, bits.wrapping_add(at)));
        }
        _ => {}
      }
    }
    result
  }

  fn two(&self, one: usize, two: usize) -> Option<(usize, usize)> {
    let count = self.tips.len();
    let first = one % count;
    let second = (first + 1 + two % count.max(2).saturating_sub(1)) % count;
    let distinct = count >= 2 && first != second && self.tips[first] != self.tips[second];
    distinct.then_some((first, second))
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
    if parents.iter().collect::<BTreeSet<_>>().len() < 3 {
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

  fn apply(&mut self, (kind, one, two, three): Op) {
    if self.room() == 0 {
      return;
    }
    match kind {
      0..=5 => self.edit(one, two, three),
      6 | 7 => self.rename(one, two, three),
      8 | 9 => self.copy(one, two, three),
      10..=11 => self.merge(one, two, three),
      12 => self.octopus(one, three),
      13 => self.criss_cross(one, two),
      14 if self.tips.len() < MAX_TIPS => self.tips.push(number(one % self.built.maps.len())),
      _ if self.tips.len() < MAX_TIPS => {
        let map: Files = [(
          PATHS[one % PATHS.len()].as_bytes().to_vec(),
          text(&[two % TOKENS], false),
        )]
        .into();
        let root = self.built.record(&[], map, Sources::new());
        self.tips.push(root);
      }
      _ => {}
    }
  }
}

/// One line's attribution: the version, the path, the line's number there.
type Origin = (usize, Vec<u8>, u32);

/// The model: every version's every file's attribution, computed by replaying the history from its roots by the rule
/// (module docs), with what the replay saw.
#[derive(Default)]
struct Model {
  blames: BTreeMap<(usize, Vec<u8>), Vec<Origin>>,
  /// Lines the rule read: each file it blamed anew and each of its candidates.
  lines_read: u64,
  judged: u64,
  merge_own: u64,
  later: u64,
  through_source: u64,
}

impl Model {
  /// The candidates of `path` in `version` (module docs): each parent's file at the path, then the recorded source.
  fn candidates(&self, built: &Builder, version: usize, path: &[u8]) -> Vec<(usize, Vec<u8>)> {
    let mut candidates: Vec<(usize, Vec<u8>)> = built.history.parents[version]
      .iter()
      .map(|parent| index(*parent))
      .filter(|parent| built.maps[*parent].contains_key(path))
      .map(|parent| (parent, path.to_vec()))
      .collect();
    if let Some((from, source)) = built.history.sources[version].get(path) {
      candidates.push((index(*source), from.clone()));
    }
    candidates
  }

  /// The attribution of `path` in `version` by the rule, from the attributions of earlier versions: the first parent's
  /// with the same bytes, else each line through the first candidate whose diff keeps it (a candidate the same as an
  /// earlier one, bytes and attribution, read once, as the derivation reads one blame once), else the version's own.
  fn attribute(&mut self, built: &Builder, version: usize, path: &[u8]) -> Vec<Origin> {
    let bytes = &built.maps[version][path];
    let parents = &built.history.parents[version];
    if let Some(parent) = parents
      .iter()
      .find(|parent| built.maps[index(**parent)].get(path) == Some(bytes))
    {
      return self.blames[&(index(*parent), path.to_vec())].clone();
    }
    let file = lines(bytes);
    self.lines_read += number(file.len());
    let mut result: Vec<Option<(Origin, usize)>> = vec![None; file.len()];
    let mut seen: Vec<(Vec<u8>, Vec<Origin>)> = Vec::new();
    for (rank, (from, from_path)) in self
      .candidates(built, version, path)
      .into_iter()
      .enumerate()
    {
      let old_bytes = built.maps[from][&from_path].clone();
      let attribution = self.blames[&(from, from_path)].clone();
      let candidate = (old_bytes, attribution);
      if seen.contains(&candidate) {
        continue;
      }
      let old = lines(&candidate.0);
      self.lines_read += number(old.len());
      let numbers = numbers_of(&[&old, &file]);
      for (i, j) in matched(&numbers[0], &numbers[1]).unwrap() {
        if result[j].is_none() {
          result[j] = Some((candidate.1[i].clone(), rank));
        }
      }
      seen.push(candidate);
    }
    let merge = parents.len() >= 2;
    let sourced = built.history.sources[version].contains_key(path);
    let mut origins = Vec::with_capacity(result.len());
    for (at, line) in result.into_iter().enumerate() {
      origins.push(match line {
        Some((origin, rank)) => {
          self.later += u64::from(merge && rank > 0 && rank < parents.len());
          self.through_source += u64::from(sourced && origin.1 != path);
          origin
        }
        None => {
          self.merge_own += u64::from(merge);
          (version, path.to_vec(), u32::try_from(at).unwrap())
        }
      });
    }
    origins
  }

  /// The replay: every version in order, every file it holds.
  fn of(built: &Builder) -> Model {
    let mut model = Model::default();
    for version in 0..built.maps.len() {
      let change = &built.history.changes[version];
      let first = built.history.parents[version]
        .first()
        .map(|parent| index(*parent));
      for path in built.maps[version].keys() {
        let blame = match (first, change.contains_key(path)) {
          (Some(first), false) => model.blames[&(first, path.clone())].clone(),
          _ => {
            model.judged += 1;
            model.attribute(built, version, path)
          }
        };
        model.blames.insert((version, path.clone()), blame);
      }
    }
    model
  }
}

/// Each of `files`' lines numbered among the distinct lines of all of them.
fn numbers_of(files: &[&Vec<&[u8]>]) -> Vec<Vec<u32>> {
  let mut ids: BTreeMap<&[u8], u32> = BTreeMap::new();
  files
    .iter()
    .map(|lines| {
      lines
        .iter()
        .map(|line| {
          let next = u32::try_from(ids.len()).unwrap();
          *ids.entry(line).or_insert(next)
        })
        .collect()
    })
    .collect()
}

/// One store, one history's values and a declaration of the type.
struct Fixture {
  store: ContentStore,
  derivations: Derivations,
  ty: Blame<History>,
  roots: Vec<ContentId>,
  contents: Vec<ContentId>,
}

fn drive(
  fill: &mut Fill<u64>,
  derivations: &mut Derivations,
  ty: &mut Blame<History>,
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
  fn empty(built: &Builder, mode: Mode) -> Fixture {
    let mut store = ContentStore::new(BUDGET, WINDOW).unwrap();
    let contents = built
      .contents()
      .iter()
      .map(|bytes| store.put_object(bytes).unwrap())
      .collect();
    Fixture {
      store,
      derivations: Derivations::new(),
      ty: Blame::declare(WINDOW, mode).unwrap(),
      roots: Vec::new(),
      contents,
    }
  }

  fn eager(built: &Builder) -> Fixture {
    let mut fixture = Fixture::empty(built, Mode::Eager);
    for version in 0..number(built.history.parents.len()) {
      let recorded = fixture
        .derivations
        .recorded(&mut fixture.store, &fixture.ty, version);
      let mut fill = recorded.unwrap().unwrap();
      let root = drive(
        &mut fill,
        &mut fixture.derivations,
        &mut fixture.ty,
        &built.history,
        &mut fixture.store,
      );
      fill.release(&mut fixture.store).unwrap();
      fixture.roots.push(root);
    }
    fixture
  }

  fn lazy(built: &Builder, order: &[u64]) -> Fixture {
    let mut fixture = Fixture::empty(built, Mode::Lazy);
    let mut roots = vec![None; built.history.parents.len()];
    for version in order {
      let read =
        fixture
          .derivations
          .read(&mut fixture.store, &fixture.ty, &built.history, *version);
      let root = match read.unwrap() {
        Read::Ready(root) => root,
        Read::Fill(mut fill) => {
          let root = drive(
            &mut fill,
            &mut fixture.derivations,
            &mut fixture.ty,
            &built.history,
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

  /// The blame of `path` at `version`, one origin a line.
  fn origins(&mut self, version: usize, path: &[u8]) -> Option<Vec<Origin>> {
    let root = self.roots[version];
    let (spans, _) = blame(&mut self.store, root, path).unwrap();
    spans.map(|spans| expand(&spans))
  }

  fn finish(self) {
    let Fixture {
      mut store,
      derivations,
      contents,
      ..
    } = self;
    derivations.release(&mut store).unwrap();
    for content in contents {
      store.release(&content).unwrap();
    }
    assert!(store.is_empty(), "the values freed whole");
    assert_eq!((store.used(), store.charged()), (0, 0));
  }
}

/// Spans as one origin a line.
fn expand(spans: &[Span]) -> Vec<Origin> {
  let mut lines = Vec::new();
  for span in spans {
    let version = index(u64::from_be_bytes(
      span.version.as_slice().try_into().unwrap(),
    ));
    for offset in 0..span.lines {
      lines.push((version, span.path.clone(), span.origin + offset));
    }
  }
  lines
}

/// What the oracle met, summed over every case.
#[derive(Debug, Default)]
struct Census {
  versions: u64,
  merges: u64,
  renames: u64,
  copies_from_older: u64,
  blames: u64,
  kept: u64,
  made: u64,
  merged: u64,
  merge_own: u64,
  later: u64,
  through_source: u64,
  lines: u64,
  most_lines: u64,
}

impl Census {
  fn add(&mut self, other: &Census) {
    self.versions += other.versions;
    self.merges += other.merges;
    self.renames += other.renames;
    self.copies_from_older += other.copies_from_older;
    self.blames += other.blames;
    self.kept += other.kept;
    self.made += other.made;
    self.merged += other.merged;
    self.merge_own += other.merge_own;
    self.later += other.later;
    self.through_source += other.through_source;
    self.lines += other.lines;
    self.most_lines = self.most_lines.max(other.most_lines);
  }

  /// Each kind met: merges, renames, copies from older versions, blames kept from a parent and made, made over two
  /// candidates or more, lines a merge resolved itself, lines kept through a later parent, lines kept through a source.
  fn assert_met(&self) {
    let met = [
      self.merges,
      self.renames,
      self.copies_from_older,
      self.kept,
      self.made,
      self.merged,
      self.merge_own,
      self.later,
      self.through_source,
    ];
    assert!(met.iter().all(|count| *count > 0), "{self:?}");
  }
}

/// Every attribution names a line of the same text where it says it was introduced.
fn check_origins(built: &Builder, (version, path): (usize, &[u8]), origins: &[Origin]) {
  let now = lines(&built.maps[version][path]);
  assert_eq!(now.len(), origins.len());
  for (line, (at, from, origin)) in now.iter().zip(origins) {
    let then = lines(&built.maps[*at][from]);
    assert_eq!(
      then[index(u64::from(*origin))],
      *line,
      "version {version} {path:?} from {at} {from:?}"
    );
  }
}

/// One case of the oracle: a history grown from `ops`, every version's every file blamed and held to the replay, the
/// derivations' reads to the replay's, and the values filled again lazily.
fn case(ops: &[Op]) -> Census {
  let mut grower = Grower::new();
  for op in ops {
    grower.apply(*op);
  }
  let built = grower.built;
  let model = Model::of(&built);
  let mut fixture = Fixture::eager(&built);
  let mut census = Census::default();
  for version in 0..built.maps.len() {
    for path in built.maps[version].keys() {
      let origins = fixture.origins(version, path).unwrap();
      assert_eq!(
        origins,
        model.blames[&(version, path.clone())],
        "version {version} {path:?}"
      );
      check_origins(&built, (version, path.as_slice()), &origins);
      census.blames += 1;
      census.lines += number(origins.len());
      census.most_lines = census.most_lines.max(number(origins.len()));
    }
    assert_eq!(fixture.origins(version, b"absent"), None);
  }
  let counters = fixture.ty.counters();
  assert_eq!(
    (counters.judged, counters.lines_read),
    (model.judged, model.lines_read)
  );
  let order: Vec<u64> = (0..number(built.maps.len())).rev().collect();
  let lazy = Fixture::lazy(&built, &order);
  assert_eq!(
    lazy.roots, fixture.roots,
    "the same value whatever the filling's order"
  );
  lazy.finish();
  census.versions = number(built.maps.len());
  for (version, parents) in built.history.parents.iter().enumerate() {
    census.merges += u64::from(parents.len() >= 2);
    let sources = &built.history.sources[version];
    let moved = |(from, source): &(Vec<u8>, u64)| {
      parents.first() == Some(source) && !built.maps[version].contains_key(from)
    };
    census.renames += u64::from(sources.values().any(moved));
    census.copies_from_older += u64::from(
      sources
        .values()
        .any(|(_, source)| parents.first() != Some(source)),
    );
  }
  census.kept = counters.kept;
  census.made = counters.made;
  census.merged = counters.merged;
  census.merge_own = model.merge_own;
  census.later = model.later;
  census.through_source = model.through_source;
  fixture.finish();
  census
}

/// A-666 part 3 (A-645 "Queries by API"). Do: grow histories of up to 160 versions of files of few distinct lines
/// (lines inserted, removed and replaced, a last newline dropped and restored, files made and removed, renames some
/// edited as they move, copies from the first parent and from older versions, merges of two and three keeping, taking
/// or resolving each file, criss-cross merges, unrelated roots), derive every version's blame eagerly and again lazily
/// from the newest down, and blame every file of every version. Expect: every blame equals the replay's; every line's
/// attribution names a line of its text where it says; the derivations judged the files the changes set and read the
/// lines the replay read, no more; the lazy values are the eager ones; the store is empty after; and the census,
/// printed every run, met every kind.
#[test]
#[allow(
  clippy::print_stderr,
  reason = "the census is the oracle's evidence, printed with every run"
)]
fn t_a666_p3_every_blame_equals_a_replay_of_the_history() {
  let census = RefCell::new(Census::default());
  let config = ProptestConfig {
    cases: slates_test_seeds::cases(32),
    ..ProptestConfig::default()
  };
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(config));
  let result = runner.run(&proptest::collection::vec(op(), 1..64), |ops| {
    census.borrow_mut().add(&case(&ops));
    Ok(())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  eprintln!("blame census: {census:?}");
  census.assert_met();
}

/// The length of a longest common subsequence, by the dynamic program.
fn lcs_len(old: &[u32], new: &[u32]) -> usize {
  let mut row = vec![0_usize; new.len() + 1];
  for a in old {
    let mut diagonal = 0;
    for (j, b) in new.iter().enumerate() {
      let above = row[j + 1];
      row[j + 1] = if a == b {
        diagonal + 1
      } else {
        above.max(row[j])
      };
      diagonal = above;
    }
  }
  row[new.len()]
}

/// A-666 part 3, the diff. Do: diff generated pairs of sequences of up to 60 lines drawn from two to five distinct
/// lines (many ties). Expect: the pairs kept are strictly increasing on both sides and name equal lines, and there are
/// as many as the dynamic program's longest common subsequence.
#[test]
fn t_a666_p3_the_diff_keeps_a_longest_common_subsequence() {
  let config = ProptestConfig {
    cases: slates_test_seeds::cases(512),
    ..ProptestConfig::default()
  };
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(config));
  let side = || (2_u32..6).prop_flat_map(|kinds| proptest::collection::vec(0..kinds, 0..60));
  let result = runner.run(&(side(), side()), |(old, new)| {
    let pairs = matched(&old, &new).unwrap();
    let increasing = pairs
      .windows(2)
      .all(|pair| pair[0].0 < pair[1].0 && pair[0].1 < pair[1].1);
    prop_assert!(increasing, "{pairs:?}");
    prop_assert!(pairs.iter().all(|(i, j)| old[*i] == new[*j]));
    prop_assert_eq!(pairs.len(), lcs_len(&old, &new));
    Ok(())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
}

/// The versions a blame names, a line each.
fn versions(fixture: &mut Fixture, version: u64, path: &str) -> Vec<u64> {
  let origins = fixture.origins(index(version), path.as_bytes()).unwrap();
  origins.iter().map(|(at, _, _)| number(*at)).collect()
}

/// A-666 part 3, D-27. Do: from `f` = a, b, c (version 0), one side changes `a` and `b`, the other `b` and `c`; merge
/// them three times: recording the first side's `b`, the second side's, and a `b` in neither. Expect: in every merge
/// `a` is the first side's and `c` the second's (each kept by its side's diff); `b` is the first side's, the second
/// side's, then the merge's own; a line the merge resolved itself is never given to a parent.
#[test]
fn t_a666_p3_a_line_both_parents_changed_is_the_merges_recorded_result() {
  let mut built = Builder::new();
  let v0 = built.edit(None, &[("f", Some("a\nb\nc\n"))]);
  let v1 = built.edit(Some(v0), &[("f", Some("a1\nb1\nc\n"))]);
  let v2 = built.edit(Some(v0), &[("f", Some("a\nb2\nc2\n"))]);
  let ours = built.edit(Some(v1), &[]);
  let first = built.record(
    &[v1, v2],
    [(b"f".to_vec(), b"a1\nb1\nc2\n".to_vec())].into(),
    Sources::new(),
  );
  let second = built.record(
    &[v1, v2],
    [(b"f".to_vec(), b"a1\nb2\nc2\n".to_vec())].into(),
    Sources::new(),
  );
  let third = built.record(
    &[v1, v2],
    [(b"f".to_vec(), b"a1\nb3\nc2\n".to_vec())].into(),
    Sources::new(),
  );
  let mut fixture = Fixture::eager(&built);
  assert_eq!(versions(&mut fixture, ours, "f"), [v1, v1, v0]);
  assert_eq!(versions(&mut fixture, first, "f"), [v1, v1, v2]);
  assert_eq!(versions(&mut fixture, second, "f"), [v1, v2, v2]);
  assert_eq!(versions(&mut fixture, third, "f"), [v1, third, v2]);
  fixture.finish();
}

/// A-666 part 3. Do: make `f` (three lines), add a line, rename it `g` adding another as it moves, and in a later
/// version copy `g` as it stood after the rename to `h` (an older version, Subversion's copy-from) while `g` changes
/// again. Expect: `g`'s lines keep the versions and the path (`f`) that introduced them, the rename's line is the
/// rename's; `h`'s lines are `g`'s as they stood in the copied version, none of the later change.
#[test]
fn t_a666_p3_renamed_and_copied_lines_keep_their_introducing_version() {
  let mut built = Builder::new();
  let v0 = built.edit(None, &[("f", Some("one\ntwo\nthree\n"))]);
  let v1 = built.edit(Some(v0), &[("f", Some("one\ntwo\nthree\nfour\n"))]);
  let v2 = built.bring(
    v1,
    ("f", v1),
    "g",
    (true, Some(b"zero\none\ntwo\nthree\nfour\n".to_vec())),
  );
  let v3 = built.edit(Some(v2), &[("g", Some("zero\none\ntwo!\nthree\nfour\n"))]);
  let v4 = built.bring(v3, ("g", v2), "h", (false, None));
  let mut fixture = Fixture::eager(&built);
  let g = fixture.origins(index(v3), b"g").unwrap();
  let expected: Vec<Origin> = vec![
    (index(v2), b"g".to_vec(), 0),
    (index(v0), b"f".to_vec(), 0),
    (index(v3), b"g".to_vec(), 2),
    (index(v0), b"f".to_vec(), 2),
    (index(v1), b"f".to_vec(), 3),
  ];
  assert_eq!(g, expected);
  assert_eq!(versions(&mut fixture, v4, "h"), [v2, v0, v0, v0, v1]);
  fixture.finish();
}

/// A-666 part 3. Do: make a file of 2,000 lines, then 300 versions each replacing one line (line 7t mod 2,000 in
/// version t). Expect: the tip's blame gives each line the last version that replaced it, else version 0; and every
/// derivation read the file and its parent's version of it alone, 4,000 lines (the first, 2,000), never the history.
#[test]
fn t_a666_p3_a_long_files_blame_reads_its_parent_alone() {
  let mut built = Builder::new();
  let mut tokens: Vec<String> = (0..LONG_LINES).map(|at| format!("line {at}\n")).collect();
  let mut tip = built.edit(None, &[("long", Some(tokens.concat().as_str()))]);
  let mut expected = vec![0_u64; LONG_LINES];
  for version in 1..=LONG_EDITS {
    let at = index(version * 7) % LONG_LINES;
    tokens[at] = format!("edit {version}\n");
    expected[at] = version;
    tip = built.edit(Some(tip), &[("long", Some(tokens.concat().as_str()))]);
  }
  let mut fixture = Fixture::eager(&built);
  assert_eq!(versions(&mut fixture, tip, "long"), expected);
  let lines = number(LONG_LINES);
  assert_eq!(
    fixture.ty.counters().lines_read,
    lines + LONG_EDITS * 2 * lines
  );
  fixture.finish();
}

/// The identity of a derived node whose bytes are `names` then `payload`, by BLAKE3's key derivation under the store's
/// context for derived nodes.
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

/// A-666 part 3, the format. Do: derive a root version holding the one-line file `f` ("x\n"), and write its bytes by
/// hand: the span (one line, origin 0, version 0's name, path `f`), the blame naming the file and the span, and the
/// tree's leaf naming the blame. Expect: the value's identity is BLAKE3's over the hand-written bytes, node by node.
#[test]
fn t_a666_p3_a_one_line_blame_is_the_formats_bytes() {
  let mut built = Builder::new();
  built.edit(None, &[("f", Some("x\n"))]);
  let fixture = Fixture::eager(&built);
  let mut span = b"\x01\0\0\0\0\0\0\0\x08\0\0\0".to_vec();
  span.extend_from_slice(&0_u64.to_be_bytes());
  span.extend_from_slice(b"\x01\0\0\0f");
  let file = ContentId::of(b"x\n");
  let mut payload = b"B\x01\0\0\0\0\0\0\0".to_vec();
  payload.extend_from_slice(&u64::try_from(span.len()).unwrap().to_le_bytes());
  let blame = identity_of(&[file, ContentId::of(&span)], &payload);
  let tree = identity_of(&[blame], b"L\x01\0\0\0f\0\0");
  assert_eq!(fixture.roots, [tree]);
  fixture.finish();
}

/// A-666 part 3, hostile bytes. Do: decode spans broken one way at a time (a span of no line, of no version, of no
/// path, one continuing the span before it, lines not the blame's, a field cut short) and blames broken (a foreign
/// tag, three names, a payload cut short). Expect: the sound spans read back; every broken form is refused.
#[test]
fn t_a666_p3_hostile_bytes_are_refused_by_the_readers() {
  let span = |lines: u32, version: &[u8], path: &[u8], origin: u32| Span {
    lines,
    version: version.to_vec(),
    path: path.to_vec(),
    origin,
  };
  let sound = [span(2, b"v", b"f", 0), span(1, b"v", b"f", 5)];
  let bytes = encode_spans(&sound).unwrap();
  assert_eq!(decode_spans(&bytes, 3), Some(sound.to_vec()));
  let broken = [
    (vec![span(0, b"v", b"f", 0)], 0),
    (vec![span(1, b"", b"f", 0)], 1),
    (vec![span(1, b"v", b"", 0)], 1),
    (vec![span(1, b"v", b"f", 0), span(1, b"v", b"f", 1)], 2),
    (sound.to_vec(), 4),
  ];
  for (at, (spans, lines)) in broken.iter().enumerate() {
    assert_eq!(
      decode_spans(&encode_spans(spans).unwrap(), *lines),
      None,
      "broken way {at}"
    );
  }
  assert_eq!(decode_spans(&bytes[..bytes.len() - 1], 3), None);
  let mut store = ContentStore::new(BUDGET, WINDOW).unwrap();
  let blob = store.put(b"x").unwrap();
  let blames: [(&[ContentId], &[u8]); 3] = [
    (&[blob, blob], b"X\x01\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"),
    (&[blob, blob, blob], b"B\x01\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"),
    (&[blob, blob], b"B\x01\0\0\0\0\0\0\0\0"),
  ];
  for (names, payload) in blames {
    let node = store.put_derived(names, payload).unwrap().identity;
    assert_eq!(
      read_node(&store, node),
      Err(BlameRefused::Malformed { node })
    );
    store.release(&node).unwrap();
  }
  store.release(&blob).unwrap();
  assert!(store.is_empty());
}

/// The store's contents, its bytes used and its charge.
fn state(store: &ContentStore) -> (usize, u64, u64) {
  (store.len(), store.used(), store.charged())
}

/// A-666 part 3, refusals. Do: derive a version whose rename names a file its source version did not hold, one with
/// too many values, one whose file's content the store does not hold, and one past the store's budget; ask a blame of
/// a root that is no tree. Expect: each refused by its kind, the store as it was.
#[test]
fn t_a666_p3_refusals_by_kind_leave_the_store_as_it_was() {
  let mut built = Builder::new();
  let v0 = built.edit(None, &[("f", Some("x\n"))]);
  let mut fixture = Fixture::eager(&built);
  let before = state(&fixture.store);
  let entry = |bytes: &[u8]| Entry {
    class: 0,
    content: ContentId::of(bytes),
  };
  let derive = |fixture: &mut Fixture, history: &History, version: u64| {
    let inputs = fixture.ty.inputs(history, version).unwrap();
    let values: Vec<ContentId> = inputs
      .iter()
      .map(|input| fixture.roots[index(*input)])
      .collect();
    fixture
      .ty
      .derive(history, &mut fixture.store, version, &values)
  };
  let mut history = built.history.clone();
  history.parents.push(vec![v0]);
  history
    .changes
    .push([(b"g".to_vec(), Some(entry(b"x\n")))].into());
  history
    .sources
    .push([(b"g".to_vec(), (b"nothing".to_vec(), v0))].into());
  assert_eq!(
    derive(&mut fixture, &history, 1),
    Err(BlameRefused::BadSource {
      path: b"g".to_vec()
    })
  );
  let wrong = fixture
    .ty
    .derive(&history, &mut fixture.store, 1, &[fixture.roots[0]; 2]);
  assert_eq!(
    wrong,
    Err(BlameRefused::Inputs {
      expected: 1,
      given: 2
    })
  );
  history.changes[1] = [(b"f".to_vec(), Some(entry(b"never put\n")))].into();
  history.sources[1] = Sources::new();
  let missing = derive(&mut fixture, &history, 1);
  assert_eq!(missing, Err(BlameRefused::Content(ContentRefused::NotHeld)));
  let held = fixture.store.put(b"y\n").unwrap();
  history.changes[1] = [(b"f".to_vec(), Some(entry(b"y\n")))].into();
  let budget = fixture.store.budget();
  fixture.store.set_budget(fixture.store.used());
  let over = derive(&mut fixture, &history, 1);
  assert!(matches!(
    over,
    Err(BlameRefused::Content(ContentRefused::OverBudget { .. }))
  ));
  fixture.store.set_budget(budget);
  fixture.store.release(&held).unwrap();
  assert_eq!(state(&fixture.store), before);
  let blob = fixture.contents[0];
  let not_tree = blame(&mut fixture.store, blob, b"f");
  assert!(matches!(
    not_tree,
    Err(BlameRefused::Paths(PathsRefused::Malformed { .. }))
  ));
  assert_eq!(state(&fixture.store), before);
  fixture.finish();
}
