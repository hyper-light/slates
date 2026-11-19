//! Blame as derived data: for each line of each file of a version, the version that introduced it, the path the file
//! had there and the line's number there (A-666 part 3; A-645 "Queries by API"; A-647; §4.17.1 "Reads"). The sources
//! and what was rejected are in `research/path-history.md` §6.
//!
//! **The question.** `git blame` and `hg annotate`: each line of a file at a version, with the version that introduced
//! it. Mercurial's linelog answers it as "the revision that introduced them and the line number in the file at the
//! introducing revision" \[C: Mercurial 7.2.4 `mercurial/linelog.py` 14-19\], and Mononoke's BlameV2 keeps the same as
//! ranges of lines, each with its changeset, its path and its origin offset \[C: `mononoke_types/src/blame_v2.rs`
//! 1224-1235\]. This type keeps it as runs of lines ([`Span`]).
//!
//! **The value** ([`Blame`]): a version's blame tree, a path map whose files are each file's blame, a derived node
//! naming the file's content and its spans (an object of the store, as long as they need) and holding the file's count
//! of lines. A file keeps the blame of the first parent whose file at its path has the same content, as path history
//! keeps a node ([`super::history`]), so a file no change reaches keeps its first parent's blame by path copying.
//!
//! **Built from the parents' blames and one diff with each.** A file the version's change sets is diffed once against
//! each parent's file at its path, in parent order, then against its recorded source (a rename's or a copy's,
//! [`super::history::PathSources`]); a line a diff keeps takes that candidate's attribution, the first that keeps it,
//! and a line none keeps is the version's own. Mononoke's BlameV2 is derived the same way, from the parents' blames and
//! "hunks coming from `diff_hunks`" \[C: `blame_v2.rs` 514-560\]. A derivation reads the file and its candidates'
//! versions of it alone ([`BlameCounters::lines_read`]): no ancestor's content, never a diff of the history. Mercurial's
//! linelog keeps one weave a file that grows in place, over a linear main branch "ignoring p2" \[C:
//! `hgext/fastannotate/__init__.py` 19-22\]; A-647's values are immutable, one a version, over a history of merges.
//!
//! **A merge** (D-27). A line both parents changed is attributed by the merge's own recorded content, never by a merge
//! inferred: the content is diffed against each parent's, and each line takes the first parent whose diff keeps it,
//! with that parent's attribution; a line no parent's diff keeps (the merge's resolution, a text in neither) is the
//! merge's own. Mononoke's merge takes, for each line, "the first one that does *not* match the merge changeset" \[C:
//! `blame_v2.rs` 1605-1640\]. The diff only says which recorded lines are shared; it makes no content.
//!
//! **The diff** ([`matched`]): Myers' O(ND) algorithm in linear space \[A: E. W. Myers, "An O(ND) Difference Algorithm
//! and Its Variations", Algorithmica 1(2):251-266, 1986, §4b\]: the common prefix and suffix taken, then the middle
//! snake of an optimal path found by searching from both ends at once and each side solved again, so the lines kept are
//! a longest common subsequence (the tests hold their count to the dynamic program's). A line is the bytes up to and
//! including a newline, the last without one; each distinct line is numbered once, so the diff compares numbers. The
//! lines, their attributions and the search's arrays are charged to the store and refused past its budget.
//!
//! **Typed refusals** ([`BlameRefused`]): no such version; values that are not the version's inputs; a source that
//! names no file; a blame or a tree that does not read as the type's; a count past its width; and the path map's and
//! the store's own (a file's content not held among them). A refused derivation leaves the store as it was.
//!
//! **Evidence** (`blame/tests.rs`): every blame equals the one a replay of the whole history computes by the rule
//! above, every attribution names a line of the same text where it says it was introduced, the diff keeps a longest
//! common subsequence, every derivation reads its changed files and their candidates' alone, and a line both parents of
//! a merge changed is the merge's own unless the merge recorded a parent's text.
//!
//! **Owed:** the server's verbs over the type (A-662); a diff larger than a slice cut into slices, as A-647's
//! derivations larger than a slice are owed; a blame's spans shared between versions by identity where an edit leaves
//! them (one object a file version today, as Mononoke's).
//!
//! No I/O, no clock, no randomness, no panic.

use std::collections::BTreeMap;
use std::mem::size_of;

use crate::content::{ContentId, ContentRefused, ContentStore};

use super::history::{PathSources, Sources, inputs_of};
use super::paths::{At, Change, Entry, PathMap, PathsRefused, at};
use super::{DerivedType, Mode};

#[cfg(test)]
mod tests;

/// Format: the type's name and its format's version; the window its trees split by follows, eight bytes, big-endian.
const TYPE_NAME: &[u8] = b"slates-vcs blame 1 ";

/// Format: the first byte of a blame's payload.
const BLAME: u8 = b'B';

/// Format: the class of every file of a blame tree, whose content is a file's blame.
const BLAME_CLASS: u8 = 0;

/// Format: the bytes of each of a blame's two counts (its lines, its spans' bytes), little-endian.
const COUNT_BYTES: usize = size_of::<u64>();

/// Format: the bytes of a blame's payload: its tag, then its two counts.
const BLAME_BYTES: usize = COUNT_BYTES.saturating_mul(2).saturating_add(1);

/// Format: the bytes of each of a span's numbers (its lines, its origin) and lengths (its version's, its path's),
/// little-endian.
const FIELD_BYTES: usize = size_of::<u32>();

/// Format: the byte that ends a line.
const NEWLINE: u8 = b'\n';

/// A run of a file's lines a blame attributes alike: `lines` lines introduced by the version named `version`, where
/// the file stood at `path` and they were its lines `origin`, `origin + 1`, ..., counted from zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
  /// The lines in the run.
  pub lines: u32,
  /// The bytes that name the introducing version ([`super::Graph::name`]).
  pub version: Vec<u8>,
  /// The file's path in that version.
  pub path: Vec<u8>,
  /// The first line's number in the file at that version.
  pub origin: u32,
}

/// Why the type refused to derive a version or to answer a question. Each leaves the store as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlameRefused {
  /// The history holds no version the derivation was asked for, or no change or sources for it.
  NoVersion,
  /// The derivation was given `given` values for `expected` inputs.
  Inputs {
    /// The inputs ([`super::DerivedType::inputs`]).
    expected: usize,
    /// The values given.
    given: usize,
  },
  /// The source recorded for `path` names no file at its version.
  BadSource {
    /// The path.
    path: Vec<u8>,
  },
  /// The blame or tree `node` does not read as the type's: its bytes, or spans that do not cover its lines.
  Malformed {
    /// The node.
    node: ContentId,
  },
  /// A file's lines or a span's field would pass `u32::MAX`.
  Exhausted,
  /// The path map refused: a path that is none, a change it refuses, a node of its that does not read.
  Paths(PathsRefused),
  /// The store refused: its budget, a count, a content not held (a file's among them), a node past the window.
  Content(ContentRefused),
}

impl From<ContentRefused> for BlameRefused {
  fn from(refused: ContentRefused) -> BlameRefused {
    BlameRefused::Content(refused)
  }
}

impl From<PathsRefused> for BlameRefused {
  fn from(refused: PathsRefused) -> BlameRefused {
    match refused {
      PathsRefused::Content(refused) => BlameRefused::Content(refused),
      refused => BlameRefused::Paths(refused),
    }
  }
}

/// What the type's derivations have done, for the tests' non-vacuity checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlameCounters {
  /// Values derived.
  pub derived: u64,
  /// Files the derivations' changes set, each judged against the parents.
  pub judged: u64,
  /// Of them, files that took a parent's blame: the first parent whose file had the same content.
  pub kept: u64,
  /// Blames made.
  pub made: u64,
  /// Of them, blames made over two candidates or more: a merge's, or a copy's onto a file that stood.
  pub merged: u64,
  /// Lines read by derivations: each made blame's file's and each of its candidates' (the non-vacuity count: never an
  /// ancestor's, never another file's).
  pub lines_read: u64,
}

impl BlameCounters {
  fn add(self, more: BlameCounters) -> BlameCounters {
    BlameCounters {
      derived: self.derived.saturating_add(more.derived),
      judged: self.judged.saturating_add(more.judged),
      kept: self.kept.saturating_add(more.kept),
      made: self.made.saturating_add(more.made),
      merged: self.merged.saturating_add(more.merged),
      lines_read: self.lines_read.saturating_add(more.lines_read),
    }
  }
}

/// What a question cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlameCost {
  /// Trie nodes read to find the file's blame.
  pub lookups: u64,
  /// Bytes of spans read.
  pub span_bytes: u64,
}

/// The type (module docs), declared over a history `S`.
#[derive(Debug)]
pub struct Blame<S> {
  name: Vec<u8>,
  mode: Mode,
  maps: PathMap<S>,
  counters: BlameCounters,
}

impl<S> Blame<S> {
  /// The type whose trees split by `window`, the store's, computed by `mode`. Refused for a window smaller than a
  /// branch of every child.
  pub fn declare(window: usize, mode: Mode) -> Result<Blame<S>, PathsRefused> {
    let maps = PathMap::declare(window, mode)?;
    let wide = u64::try_from(window).map_err(|_| PathsRefused::WindowTooSmall { window })?;
    let mut name = TYPE_NAME.to_vec();
    name.extend_from_slice(&wide.to_be_bytes());
    Ok(Blame {
      name,
      mode,
      maps,
      counters: BlameCounters::default(),
    })
  }

  /// What its derivations have done.
  pub fn counters(&self) -> BlameCounters {
    self.counters
  }
}

impl<S: PathSources> DerivedType for Blame<S> {
  type Source = S;
  type Refused = BlameRefused;

  fn name(&self) -> &[u8] {
    &self.name
  }

  fn mode(&self) -> Mode {
    self.mode
  }

  fn inputs(&self, source: &S, version: S::Version) -> Option<Vec<S::Version>> {
    inputs_of(source, version)
  }

  fn derive(
    &mut self,
    source: &S,
    store: &mut ContentStore,
    version: S::Version,
    inputs: &[ContentId],
  ) -> Result<ContentId, BlameRefused> {
    let given = given(source, version, inputs)?;
    let change = source.change(version).ok_or(BlameRefused::NoVersion)?;
    let sources = source.sources(version).ok_or(BlameRefused::NoVersion)?;
    let mut work = Work {
      store,
      maps: &mut self.maps,
      owned: Vec::new(),
      charged: 0,
      counters: BlameCounters::default(),
    };
    let made = work.derive((change, sources), &given);
    let (root, counters) = work.settle(made);
    let root = root?;
    self.counters = self.counters.add(counters);
    self.counters.derived = self.counters.derived.saturating_add(1);
    Ok(root)
  }
}

/// What a derivation reads besides its change: its parents' blame trees in order, every input's by version, and the
/// version's name.
struct Given<V> {
  parents: Vec<ContentId>,
  inputs: BTreeMap<V, ContentId>,
  name: Vec<u8>,
}

/// The inputs' trees, against `version`'s inputs.
fn given<S: PathSources>(
  source: &S,
  version: S::Version,
  values: &[ContentId],
) -> Result<Given<S::Version>, BlameRefused> {
  let parents = source.parents(version).ok_or(BlameRefused::NoVersion)?;
  let expected = inputs_of(source, version).ok_or(BlameRefused::NoVersion)?;
  if expected.len() != values.len() {
    return Err(BlameRefused::Inputs {
      expected: expected.len(),
      given: values.len(),
    });
  }
  let inputs: BTreeMap<S::Version, ContentId> =
    expected.into_iter().zip(values.iter().copied()).collect();
  let parents = parents
    .iter()
    .filter_map(|parent| inputs.get(parent).copied())
    .collect();
  let mut name = Vec::new();
  source.name(version, &mut name);
  Ok(Given {
    parents,
    inputs,
    name,
  })
}

/// A file's blame, read: its content, its count of lines, its spans' object and the bytes of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Node {
  content: ContentId,
  lines: u64,
  spans: ContentId,
  span_bytes: u64,
}

/// The blame `id` names: a derived node naming the content and the spans, its payload its tag and two counts.
fn read_node(store: &ContentStore, id: ContentId) -> Result<Node, BlameRefused> {
  let malformed = BlameRefused::Malformed { node: id };
  let Some(node) = store.derived(&id) else {
    return Err(malformed);
  };
  let mut names = node.names();
  let (Some(content), Some(spans), None) = (names.next(), names.next(), names.next()) else {
    return Err(malformed);
  };
  let Some((&BLAME, rest)) = node.payload().split_first() else {
    return Err(malformed);
  };
  let Some((lines, rest)) = rest.split_first_chunk::<COUNT_BYTES>() else {
    return Err(malformed);
  };
  let Ok(span_bytes) = <[u8; COUNT_BYTES]>::try_from(rest) else {
    return Err(malformed);
  };
  Ok(Node {
    content,
    lines: u64::from_le_bytes(*lines),
    spans,
    span_bytes: u64::from_le_bytes(span_bytes),
  })
}

/// The blame of the file at `path` in the blame tree `tree`, and the trie nodes read; `None` when no file stands there.
fn blame_at(
  store: &ContentStore,
  tree: ContentId,
  path: &[u8],
) -> Result<(Option<ContentId>, u64), BlameRefused> {
  let (found, reads) = at(store, tree, path)?;
  match found {
    None | Some(At::Directory(_)) => Ok((None, reads)),
    Some(At::File(Entry {
      class: BLAME_CLASS,
      content,
    })) => Ok((Some(content), reads)),
    Some(At::File(_)) => Err(BlameRefused::Malformed { node: tree }),
  }
}

/// The spans' bytes: for each span its lines, its origin, then its version and its path, each after its length.
fn encode_spans(spans: &[Span]) -> Option<Vec<u8>> {
  let mut bytes = Vec::new();
  for span in spans {
    bytes.extend_from_slice(&span.lines.to_le_bytes());
    bytes.extend_from_slice(&span.origin.to_le_bytes());
    for field in [&span.version, &span.path] {
      bytes.extend_from_slice(&u32::try_from(field.len()).ok()?.to_le_bytes());
      bytes.extend_from_slice(field);
    }
  }
  Some(bytes)
}

/// A length-prefixed field and what follows it.
fn field(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
  let (len, rest) = bytes.split_first_chunk::<FIELD_BYTES>()?;
  rest.split_at_checked(usize::try_from(u32::from_le_bytes(*len)).ok()?)
}

/// Whether `next` continues `last`: the same version and path, its origin where `last`'s run ends.
fn joins(last: &Span, next: &Span) -> bool {
  last.version == next.version
    && last.path == next.path
    && last.origin.checked_add(last.lines) == Some(next.origin)
}

/// The spans of a blame of `lines` lines from their bytes; `None` for bytes in any other form: a span of no line, of
/// no version or path, one that continues the span before it (the spans are the longest runs), or spans whose lines
/// are not the blame's.
fn decode_spans(bytes: &[u8], lines: u64) -> Option<Vec<Span>> {
  let mut spans: Vec<Span> = Vec::new();
  let mut rest = bytes;
  let mut total = 0_u64;
  while !rest.is_empty() {
    let (count, after) = rest.split_first_chunk::<FIELD_BYTES>()?;
    let (origin, after) = after.split_first_chunk::<FIELD_BYTES>()?;
    let (version, after) = field(after)?;
    let (path, after) = field(after)?;
    let span = Span {
      lines: u32::from_le_bytes(*count),
      version: version.to_vec(),
      path: path.to_vec(),
      origin: u32::from_le_bytes(*origin),
    };
    let empty = span.lines == 0 || span.version.is_empty() || span.path.is_empty();
    if empty || spans.last().is_some_and(|last| joins(last, &span)) {
      return None;
    }
    total = total.checked_add(u64::from(span.lines))?;
    spans.push(span);
    rest = after;
  }
  (total == lines).then_some(spans)
}

/// The blame of the file at `path` in the version whose value (its blame tree) is `value`: its spans in line order, or
/// `None` when no file stands there; and what reading it cost: the trie nodes on the path and the spans' bytes, never
/// more. Refused for a path that is none, a tree or blame that does not read as the type's, or past the store's
/// budget.
pub fn blame(
  store: &mut ContentStore,
  value: ContentId,
  path: &[u8],
) -> Result<(Option<Vec<Span>>, BlameCost), BlameRefused> {
  let (found, lookups) = blame_at(store, value, path)?;
  let mut cost = BlameCost {
    lookups,
    span_bytes: 0,
  };
  let Some(id) = found else {
    return Ok((None, cost));
  };
  let node = read_node(store, id)?;
  store.charge(node.span_bytes)?;
  let spans = spans_of(store, id, node);
  let discharged = store.discharge(node.span_bytes);
  let spans = spans?;
  discharged?;
  cost.span_bytes = node.span_bytes;
  Ok((Some(spans), cost))
}

/// The spans of the blame `id`, whose node is `node`, read from its object.
fn spans_of(store: &ContentStore, id: ContentId, node: Node) -> Result<Vec<Span>, BlameRefused> {
  let malformed = BlameRefused::Malformed { node: id };
  let Some(bytes) = store.object_bytes(&node.spans, node.span_bytes) else {
    return Err(malformed);
  };
  decode_spans(&bytes, node.lines).ok_or(malformed)
}

/// `bytes`' lines: each up to and including a newline, the last without one when the bytes end without one.
fn lines_of(bytes: &[u8]) -> Vec<&[u8]> {
  bytes.split_inclusive(|byte| *byte == NEWLINE).collect()
}

/// The (version, path) pairs a derivation's attributions name, each once, by number.
#[derive(Default)]
struct Table {
  who: Vec<(Vec<u8>, Vec<u8>)>,
  numbers: BTreeMap<(Vec<u8>, Vec<u8>), usize>,
}

impl Table {
  /// The number of (`version`, `path`).
  fn number(&mut self, version: &[u8], path: &[u8]) -> usize {
    let key = (version.to_vec(), path.to_vec());
    if let Some(number) = self.numbers.get(&key) {
      return *number;
    }
    let number = self.who.len();
    self.who.push(key.clone());
    self.numbers.insert(key, number);
    number
  }

  /// Each line's attribution, from `spans`: its pair's number and its origin.
  fn lines(&mut self, spans: &[Span]) -> Option<Vec<(usize, u32)>> {
    let mut lines = Vec::new();
    for span in spans {
      let who = self.number(&span.version, &span.path);
      for offset in 0..span.lines {
        lines.push((who, span.origin.checked_add(offset)?));
      }
    }
    Some(lines)
  }

  /// The longest runs of `lines`' attributions.
  fn spans(&self, lines: &[(usize, u32)]) -> Option<Vec<Span>> {
    let mut spans: Vec<Span> = Vec::new();
    for (who, origin) in lines {
      let (version, path) = self.who.get(*who)?;
      let next = Span {
        lines: 1,
        version: version.clone(),
        path: path.clone(),
        origin: *origin,
      };
      match spans.last_mut() {
        Some(last) if joins(last, &next) => last.lines = last.lines.checked_add(1)?,
        _ => spans.push(next),
      }
    }
    Some(spans)
  }
}

/// Each line of every one of `buffers`, by its number among the distinct lines of all of them.
fn numbered(buffers: &[Vec<u8>]) -> Option<Vec<Vec<u32>>> {
  let mut numbers: BTreeMap<&[u8], u32> = BTreeMap::new();
  let mut all = Vec::with_capacity(buffers.len());
  for buffer in buffers {
    let mut lines = Vec::new();
    for line in lines_of(buffer) {
      let next = u32::try_from(numbers.len()).ok()?;
      lines.push(*numbers.entry(line).or_insert(next));
    }
    all.push(lines);
  }
  Some(all)
}

/// The pairs (i, j) of a longest common subsequence of `old` and `new`, in order: the lines of `new` the diff keeps
/// from `old` (module docs, "The diff"). `None` only past a count's width.
fn matched(old: &[u32], new: &[u32]) -> Option<Vec<(usize, usize)>> {
  let mut pairs = Vec::new();
  let mut pending = vec![(0, old.len(), 0, new.len())];
  while let Some((old_start, old_end, new_start, new_end)) = pending.pop() {
    let (mut x, mut y) = (old_start, new_start);
    while x < old_end && y < new_end && old.get(x) == new.get(y) {
      pairs.push((x, y));
      x = x.checked_add(1)?;
      y = y.checked_add(1)?;
    }
    let (mut u, mut v) = (old_end, new_end);
    while u > x && v > y && old.get(u.checked_sub(1)?) == new.get(v.checked_sub(1)?) {
      u = u.checked_sub(1)?;
      v = v.checked_sub(1)?;
      pairs.push((u, v));
    }
    if x < u
      && y < v
      && let Some((split_x, split_y)) = middle(old.get(x..u)?, new.get(y..v)?)?
    {
      let (split_x, split_y) = (x.checked_add(split_x)?, y.checked_add(split_y)?);
      let whole = (split_x, split_y) == (x, y) || (split_x, split_y) == (u, v);
      if !whole {
        pending.push((split_x, u, split_y, v));
        pending.push((x, split_x, y, split_y));
      }
    }
  }
  pairs.sort_unstable();
  Some(pairs)
}

/// A search's furthest reaching paths: for each diagonal, the furthest `x` its path has reached, or -1 for none yet
/// (diff-match-patch's `v1` and `v2`, the reference implementation of Myers' bisection).
struct Furthest {
  reached: Vec<i64>,
  offset: i64,
  /// The diagonals at either end it no longer extends, since their paths left the grid (`k1start`, `k1end`).
  trim_start: i64,
  trim_end: i64,
}

impl Furthest {
  fn new(offset: i64) -> Option<Furthest> {
    let len = usize::try_from(offset.checked_mul(2)?.checked_add(2)?).ok()?;
    let mut reached = vec![-1; len];
    *reached.get_mut(usize::try_from(offset.checked_add(1)?).ok()?)? = 0;
    Some(Furthest {
      reached,
      offset,
      trim_start: 0,
      trim_end: 0,
    })
  }

  fn get(&self, diagonal: i64) -> Option<i64> {
    let at = usize::try_from(self.offset.checked_add(diagonal)?).ok()?;
    self.reached.get(at).copied()
  }

  fn set(&mut self, diagonal: i64, x: i64) -> Option<()> {
    let at = usize::try_from(self.offset.checked_add(diagonal)?).ok()?;
    *self.reached.get_mut(at)? = x;
    Some(())
  }

  /// Where the path on `diagonal` starts at distance `d`: from the diagonal above (a move down) or below (a move
  /// right), whichever reached further (Myers 1986, Fig. 2).
  fn start(&self, diagonal: i64, d: i64) -> Option<i64> {
    let from_down = diagonal == d.checked_neg()?
      || (diagonal != d
        && self.get(diagonal.checked_sub(1)?)? < self.get(diagonal.checked_add(1)?)?);
    if from_down {
      self.get(diagonal.checked_add(1)?)
    } else {
      self.get(diagonal.checked_sub(1)?)?.checked_add(1)
    }
  }
}

/// Lines `x` of `old` and `y` of `new`, counted from the front, or from the back when `back`.
fn same(old: &[u32], new: &[u32], (x, y): (i64, i64), back: bool) -> Option<bool> {
  let (x, y) = (usize::try_from(x).ok()?, usize::try_from(y).ok()?);
  if back {
    let at_old = old.len().checked_sub(x.checked_add(1)?)?;
    let at_new = new.len().checked_sub(y.checked_add(1)?)?;
    Some(old.get(at_old) == new.get(at_new))
  } else {
    Some(old.get(x) == new.get(y))
  }
}

/// The furthest point the path on `diagonal` reaches at distance `d`, its snake followed; from the front, or from the
/// back when `back`. A path that leaves the grid trims its end of the diagonals.
fn reach(
  (old, new): (&[u32], &[u32]),
  paths: &mut Furthest,
  (diagonal, d): (i64, i64),
  back: bool,
) -> Option<(i64, i64)> {
  let (n, m) = (
    i64::try_from(old.len()).ok()?,
    i64::try_from(new.len()).ok()?,
  );
  let mut x = paths.start(diagonal, d)?;
  let mut y = x.checked_sub(diagonal)?;
  while x < n && y < m && y >= 0 && same(old, new, (x, y), back)? {
    x = x.checked_add(1)?;
    y = y.checked_add(1)?;
  }
  paths.set(diagonal, x)?;
  if x > n {
    paths.trim_end = paths.trim_end.checked_add(2)?;
  } else if y > m {
    paths.trim_start = paths.trim_start.checked_add(2)?;
  }
  Some((x, y))
}

/// A split point on the grid of `n` by `m`, as positions; `None` off the grid.
fn on_grid((x, y): (i64, i64), (n, m): (i64, i64)) -> Option<(usize, usize)> {
  let inside = (0..=n).contains(&x) && (0..=m).contains(&y);
  if inside {
    Some((usize::try_from(x).ok()?, usize::try_from(y).ok()?))
  } else {
    None
  }
}

/// One distance of the forward search: each diagonal's path extended; with `odd`, the split where one overlaps the
/// reverse search's path on its mirrored diagonal.
fn forward_sweep(
  (old, new): (&[u32], &[u32]),
  (forward, reverse): (&mut Furthest, &Furthest),
  (d, delta, odd): (i64, i64, bool),
) -> Option<Option<(usize, usize)>> {
  let (n, m) = (
    i64::try_from(old.len()).ok()?,
    i64::try_from(new.len()).ok()?,
  );
  let mut diagonal = forward.trim_start.checked_sub(d)?;
  let last = d.checked_sub(forward.trim_end)?;
  while diagonal <= last {
    let (x, y) = reach((old, new), forward, (diagonal, d), false)?;
    let back = reverse.get(delta.checked_sub(diagonal)?).unwrap_or(-1);
    if odd && x <= n && y <= m && back >= 0 && x >= n.checked_sub(back)? {
      return Some(on_grid((x, y), (n, m)));
    }
    diagonal = diagonal.checked_add(2)?;
  }
  Some(None)
}

/// One distance of the reverse search: each diagonal's path extended from the back; without `odd`, the split where one
/// overlaps the forward search's path on its mirrored diagonal (the forward path's end).
fn reverse_sweep(
  (old, new): (&[u32], &[u32]),
  (forward, reverse): (&Furthest, &mut Furthest),
  (d, delta, odd): (i64, i64, bool),
) -> Option<Option<(usize, usize)>> {
  let (n, m) = (
    i64::try_from(old.len()).ok()?,
    i64::try_from(new.len()).ok()?,
  );
  let mut diagonal = reverse.trim_start.checked_sub(d)?;
  let last = d.checked_sub(reverse.trim_end)?;
  while diagonal <= last {
    let (back_x, back_y) = reach((old, new), reverse, (diagonal, d), true)?;
    let mirrored = delta.checked_sub(diagonal)?;
    let x = forward.get(mirrored).unwrap_or(-1);
    if !odd && back_x <= n && back_y <= m && x >= 0 && x >= n.checked_sub(back_x)? {
      return Some(on_grid((x, x.checked_sub(mirrored)?), (n, m)));
    }
    diagonal = diagonal.checked_add(2)?;
  }
  Some(None)
}

/// Where a minimal diff of `old` and `new` (both non-empty, their first lines differing and their last) splits: the end
/// of the forward path's last snake where it overlaps the reverse path, a point on an optimal path (Myers 1986, §4b,
/// the middle snake), searched from both ends at once. `Some(None)` when the searches never overlap within half the
/// lines, which by parity means no line is common (D = n + m). `None` only past a count's width.
fn middle(old: &[u32], new: &[u32]) -> Option<Option<(usize, usize)>> {
  let (n, m) = (
    i64::try_from(old.len()).ok()?,
    i64::try_from(new.len()).ok()?,
  );
  let most = n.checked_add(m)?.checked_add(1)?.checked_div(2)?;
  let delta = n.checked_sub(m)?;
  let odd = delta.checked_rem(2)? != 0;
  let mut forward = Furthest::new(most)?;
  let mut reverse = Furthest::new(most)?;
  for d in 0..most {
    if let Some(split) = forward_sweep((old, new), (&mut forward, &reverse), (d, delta, odd))? {
      return Some(Some(split));
    }
    if let Some(split) = reverse_sweep((old, new), (&forward, &mut reverse), (d, delta, odd))? {
      return Some(Some(split));
    }
  }
  Some(None)
}

/// Shape: what a derivation keeps for each line of the files it reads: its number, its attribution and the result's
/// slot. Derived from the types.
fn line_bytes() -> u64 {
  let bytes = size_of::<u32>()
    .saturating_add(size_of::<(usize, u32)>())
    .saturating_add(size_of::<Option<(usize, u32)>>());
  u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// Shape: what one diff of `old` and `new` lines keeps besides the lines: its two searches' arrays and its pairs.
/// Derived from the types and Myers' bound on the search's diagonals.
fn diff_bytes(old: usize, new: usize) -> u64 {
  let diagonals = old.saturating_add(new).saturating_add(2);
  let arrays = diagonals
    .saturating_mul(2)
    .saturating_mul(size_of::<Option<i64>>());
  let pairs = old.min(new).saturating_mul(size_of::<(usize, usize)>());
  u64::try_from(arrays.saturating_add(pairs)).unwrap_or(u64::MAX)
}

/// One derivation's work: the store, the path map that applies the blame tree's edits, the holds it took and what it
/// charged (each given back when it settles), and what it counted.
struct Work<'s, 'm, S> {
  store: &'s mut ContentStore,
  maps: &'m mut PathMap<S>,
  owned: Vec<ContentId>,
  charged: u64,
  counters: BlameCounters,
}

impl<S> Work<'_, '_, S> {
  /// Charges the store for `bytes` more kept.
  fn charge(&mut self, bytes: u64) -> Result<(), BlameRefused> {
    self.store.charge(bytes)?;
    self.charged = self.charged.saturating_add(bytes);
    Ok(())
  }

  /// The version's blame tree: its first parent's, with each file its change sets judged and each it removes removed.
  fn derive<V: Ord>(
    &mut self,
    (change, sources): (&Change, &Sources<V>),
    given: &Given<V>,
  ) -> Result<ContentId, BlameRefused> {
    let mut edits = Change::new();
    for (path, entry) in change {
      let Some(entry) = entry else {
        edits.insert(path.clone(), None);
        continue;
      };
      if let Some(blame) = self.judge(path, entry.content, given, sources)? {
        let blame = Entry {
          class: BLAME_CLASS,
          content: blame,
        };
        edits.insert(path.clone(), Some(blame));
      }
    }
    let first = given.parents.first().copied();
    // A-666 part 3's mutation (a branch not for main): every file the first parent holds that the change leaves is
    // blamed again by a diff with the first parent's, the same blame made again, as a type that re-blames the whole
    // tree each version would.
    if let Some(tree) = first {
      for (path, entry) in self.maps.files(self.store, tree)? {
        if change.contains_key(&path) {
          continue;
        }
        self.counters.judged = self.counters.judged.saturating_add(1);
        let content = read_node(self.store, entry.content)?.content;
        let made = self.make((path.as_slice(), content), &[entry.content], &given.name)?;
        let blame = Entry {
          class: BLAME_CLASS,
          content: made,
        };
        edits.insert(path, Some(blame));
      }
    }
    Ok(self.maps.apply(self.store, first, &edits)?)
  }

  /// The blame of the file `path` now holding `content`: kept from the first parent's (`None`) or taken from a later
  /// parent's when that parent's file has the same content; else made from the candidates (module docs).
  fn judge<V: Ord>(
    &mut self,
    path: &[u8],
    content: ContentId,
    given: &Given<V>,
    sources: &Sources<V>,
  ) -> Result<Option<ContentId>, BlameRefused> {
    self.counters.judged = self.counters.judged.saturating_add(1);
    let mut candidates = Vec::with_capacity(given.parents.len().saturating_add(1));
    for (index, tree) in given.parents.iter().enumerate() {
      let Some(id) = blame_at(self.store, *tree, path)?.0 else {
        continue;
      };
      if read_node(self.store, id)?.content == content {
        self.counters.kept = self.counters.kept.saturating_add(1);
        return Ok((index > 0).then_some(id));
      }
      if !candidates.contains(&id) {
        candidates.push(id);
      }
    }
    if let Some((from_path, from)) = sources.get(path) {
      let bad = || BlameRefused::BadSource {
        path: path.to_vec(),
      };
      let tree = given.inputs.get(from).ok_or_else(bad)?;
      let id = blame_at(self.store, *tree, from_path)?.0.ok_or_else(bad)?;
      if !candidates.contains(&id) {
        candidates.push(id);
      }
    }
    self
      .make((path, content), &candidates, &given.name)
      .map(Some)
  }

  /// Reads `content`'s bytes, each window charged as it is read.
  fn read(&mut self, content: ContentId) -> Result<Vec<u8>, BlameRefused> {
    if !self.store.contains(&content) {
      return Err(BlameRefused::Content(ContentRefused::NotHeld));
    }
    let mut bytes = Vec::new();
    let mut windows = Vec::new();
    for window in self.store.windows(&content) {
      windows.push(u64::try_from(window.len()).unwrap_or(u64::MAX));
      bytes.extend_from_slice(window);
    }
    self.charge(
      windows
        .iter()
        .fold(0_u64, |sum, len| sum.saturating_add(*len)),
    )?;
    Ok(bytes)
  }

  /// A blame made for `path` holding `content` from `candidates`' blames, in order (module docs): each line the first
  /// candidate's whose diff keeps it, else the version's own.
  fn make(
    &mut self,
    (path, content): (&[u8], ContentId),
    candidates: &[ContentId],
    name: &[u8],
  ) -> Result<ContentId, BlameRefused> {
    let mut buffers = vec![self.read(content)?];
    let mut table = Table::default();
    let mut through = Vec::with_capacity(candidates.len());
    for id in candidates {
      let node = read_node(self.store, *id)?;
      buffers.push(self.read(node.content)?);
      let spans = spans_of(self.store, *id, node)?;
      through.push((*id, table.lines(&spans).ok_or(BlameRefused::Exhausted)?));
    }
    let numbers = numbered(&buffers).ok_or(BlameRefused::Exhausted)?;
    let lines = self.attribute(&numbers, &through)?;
    let own = table.number(name, path);
    let lines: Vec<(usize, u32)> = lines
      .into_iter()
      .enumerate()
      .map(|(at, line)| match line {
        Some(line) => Ok(line),
        None => Ok((own, u32::try_from(at).map_err(|_| BlameRefused::Exhausted)?)),
      })
      .collect::<Result<_, BlameRefused>>()?;
    let spans = table.spans(&lines).ok_or(BlameRefused::Exhausted)?;
    self.put(content, &spans, number_of(lines.len())?)
  }

  /// Each line of the file (`numbers`' first) attributed through the first candidate whose diff keeps it; `None` for
  /// a line none keeps. Charges what it keeps; counts the lines read.
  fn attribute(
    &mut self,
    numbers: &[Vec<u32>],
    through: &[(ContentId, Vec<(usize, u32)>)],
  ) -> Result<Vec<Option<(usize, u32)>>, BlameRefused> {
    let Some((file, others)) = numbers.split_first() else {
      return Err(BlameRefused::Exhausted);
    };
    let read = numbers
      .iter()
      .fold(0_usize, |sum, lines| sum.saturating_add(lines.len()));
    self.charge(line_bytes().saturating_mul(number_of(read)?))?;
    self.counters.lines_read = self.counters.lines_read.saturating_add(number_of(read)?);
    let mut lines = vec![None; file.len()];
    for (old, (id, attribution)) in others.iter().zip(through) {
      if attribution.len() != old.len() {
        return Err(BlameRefused::Malformed { node: *id });
      }
      self.charge(diff_bytes(old.len(), file.len()))?;
      for (from, to) in matched(old, file).ok_or(BlameRefused::Exhausted)? {
        if let (Some(slot), Some(line)) = (lines.get_mut(to), attribution.get(from))
          && slot.is_none()
        {
          *slot = Some(*line);
        }
      }
    }
    self.counters.merged = self
      .counters
      .merged
      .saturating_add(u64::from(through.len() > 1));
    Ok(lines)
  }

  /// Puts a blame of `content` and `spans`, `lines` lines: the spans' object, then the node naming both.
  fn put(
    &mut self,
    content: ContentId,
    spans: &[Span],
    lines: u64,
  ) -> Result<ContentId, BlameRefused> {
    let bytes = encode_spans(spans).ok_or(BlameRefused::Exhausted)?;
    let object = self.store.put_object(&bytes)?;
    self.owned.push(object);
    let mut payload = Vec::with_capacity(BLAME_BYTES);
    payload.push(BLAME);
    payload.extend_from_slice(&lines.to_le_bytes());
    payload.extend_from_slice(&number_of(bytes.len())?.to_le_bytes());
    let blame = self
      .store
      .put_derived(&[content, object], &payload)?
      .identity;
    self.owned.push(blame);
    self.counters.made = self.counters.made.saturating_add(1);
    Ok(blame)
  }

  /// Ends the work: every hold it took given back (the tree holds each blame, a blame its spans) and every charge;
  /// the tree, held once by the caller. Past a refusal the rest is given back still, and the first refusal is the
  /// answer.
  fn settle(
    self,
    made: Result<ContentId, BlameRefused>,
  ) -> (Result<ContentId, BlameRefused>, BlameCounters) {
    let Work {
      store,
      owned,
      charged,
      counters,
      ..
    } = self;
    let mut released: Result<(), BlameRefused> = store.discharge(charged).map_err(Into::into);
    for node in owned {
      if let Err(refused) = store.release(&node) {
        released = released.and(Err(refused.into()));
      }
    }
    (made.and_then(|root| released.map(|()| root)), counters)
  }
}

/// A count as the format keeps it; refused past its width.
fn number_of(count: usize) -> Result<u64, BlameRefused> {
  u64::try_from(count).map_err(|_| BlameRefused::Exhausted)
}
