//! Path history as derived data: for each path, the versions that changed it, answering `log <path>` and
//! `log --follow <path>` with no walk of the history (A-666 part 2; A-645 "Queries by API"; A-647; §4.17.1 "Reads"). The
//! sources and what was rejected are in `research/path-history.md`.
//!
//! **The question, in git's words.** In its default mode `git log -- <path>` includes a commit "if [it is] not TREESAME
//! to any parent ... If the commit was a merge, and it was TREESAME to one parent, follow only that parent. (Even if
//! there are several TREESAME parents, follow only one of them.) Otherwise, follow all parents." \[C: git
//! `Documentation/rev-list-options.adoc` 507-513\]. The one followed is the first in parent order (`revision.c`
//! 1001-1066: the first parent found TREESAME becomes the commit's only one), and a root "is compared to an empty tree"
//! (`rev-list-options.adoc` 525-526). This type answers that mode. A path is TREESAME in a version and a parent when it
//! holds the same in both: nothing, the same file (its class and its content), or a directory of the same files. A
//! directory stands while a file is beneath it, the root too, as a git tree does.
//!
//! **The value** ([`PathHistory`]): Mononoke's unodes \[C: `mononoke_types/src/unode.rs` 31-57: "designed to find file
//! or directory history quickly which can be used to answer "log" or "blame" requests"\], made to answer git's rule
//! exactly. A version's value names its map of paths to contents (the path map's value, [`super::paths`]) and its
//! history tree, a path map whose files are each path's node: the node of a path `p` is kept at `p/\0`, the root's at
//! `\0`, so a file and a directory once at one path share one history, as they do in git. A path's node is a derived
//! node naming its parents (the path's nodes in the version's parents), its source's node (below) and its file's
//! content, and holding what the version left at the path (nothing, a file and its class, or a directory), the version's
//! generation and name, and the source's path.
//! - A version keeps a path's node from the first parent the path is TREESAME to, or keeps none when that parent had
//!   none; so a path no change reaches keeps its first parent's node by path copying, at no cost. Mononoke's unodes v2
//!   reuse a parent's unode the same way, the first whose content and type match \[C:
//!   `derived_data/unodes/derive.rs` 360-430\]. A directory's unode there is reused only when its children's unodes
//!   are, which compares history, not content, and lists a merge git does not; this type compares a directory by the
//!   root of its trie in the path map, a function of its files alone.
//! - Otherwise the version makes a node, its parents the parents' nodes in parent order, each once. A removed path gets
//!   a node of no file, so `log` lists the removal and reaches the history before it, as git's does. Mononoke keeps the
//!   same in a second structure, its deleted manifest \[C: `derived_data/deleted_manifest/derive.rs` 60-99\].
//! - So the nodes reachable from a path's node are exactly the versions git's default mode lists for it, by induction
//!   over the history: a version TREESAME to a parent has the first such parent's node, and lists what it lists; one
//!   TREESAME to none lists itself and what each parent lists. A path's node exists exactly when its history is not
//!   empty, so a path that stands always has one.
//!
//! **Renames and copies are recorded, never inferred** ([`PathSources`]): each path a version's change makes from
//! another, with the path and the version it came from, as the works' ops document records them (`Rename`, `Copy`;
//! §4.16 "Data model"; a copy names any earlier version, Subversion's copy-from revision). A node the version makes at
//! such a path names its source's node, so `--follow` continues there in one step. git's `--follow` finds renames by
//! content \[C: `git-log.adoc` 30-32\], and Mononoke keeps copy-from outside the unode so it can be changed later \[C:
//! `unode.rs` 41-42\]; here a source is what the work did, so it is part of the node and never changes. The source
//! version's value is an input of the derivation ([`super::DerivedType::inputs`]), so a fill derives it first.
//!
//! **The query** ([`log`]): the path's node is looked up in the version's history tree, then nodes are listed newest
//! first (the greatest generation, then the version's name, then the path), each listed node's parents put on the
//! frontier and, with `--follow`, its source at the source's path, each node once a path, until the limit. A full answer
//! reads the value, the trie nodes on the path's key, and one node for each version it lists: its cost is the path's
//! changes, never the history's length. Mononoke reads the same chain in batches of at most 110 entries \[C:
//! `mononoke_types/src/fastlog_batch.rs` 51-55, 89-104\] to spare a blobstore round trip a node; this store is in
//! memory, so each node is one read.
//!
//! **Typed refusals** ([`HistoryRefused`]): no such version; values that are not the version's inputs; a path naming
//! the reserved name; a source that does not hold; a node or value that does not read as the type's or contradicts what
//! names it; a generation past `u64::MAX`; and the path map's and the store's own. A refused derivation leaves the
//! store as it was; a question charges the store for what it keeps and is refused past its budget.
//!
//! **Evidence** (`history/tests.rs`): every answer held to git's default-mode walk over generated histories with runs,
//! merges of two and three, criss-cross merges, unrelated roots, removals, a file made a directory, renames of files and
//! directories (some edited as they move), and copies from the first parent and from older versions; every derivation
//! judges exactly the paths of its change and the directories on them; every query reads at most one node more than
//! the versions it lists; and a long history is answered by a path's changes alone.
//!
//! **Owed:** the server's verbs over the type (each repository's values on its owner shard, A-662); the other
//! simplifications git offers (`--full-history`, `--simplify-merges`, `--show-pulls`), each a type of its own; nodes
//! offered between tiers in batches, as fastlog's, once values travel between tiers; blame (A-666 part 3).
//!
//! No I/O, no clock, no randomness, no panic.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;

use crate::content::{ContentId, ContentRefused, ContentStore, IDENTITY_BYTES};

use super::paths::{At, Change, Entry, PathChanges, PathCounters, PathMap, PathsRefused, at};
use super::{DerivedType, Mode};

#[cfg(test)]
mod tests;

/// Format: the type's name and its format's version; the window its maps split by follows, eight bytes, big-endian.
const TYPE_NAME: &[u8] = b"slates-vcs path history 1 ";

/// Format: the name a path's node is kept under in the history tree, beneath the path: one NUL byte, which no name
/// holds in git, Mercurial, Subversion or POSIX, and which no change or question may name
/// ([`HistoryRefused::ReservedName`]).
const OWN: &[u8] = b"\0";

/// Format: the class of every file of a history tree, whose content is a path's node.
const NODE_CLASS: u8 = 0;

/// Format: the first byte of a version's value's payload.
const VALUE: u8 = b'H';

/// Format: the first byte of a path's node's payload.
const NODE: u8 = b'N';

/// Format: a node's kind when its version left nothing at its path.
const ABSENT: u8 = 0;

/// Format: a node's kind when its version left a file at its path.
const FILE: u8 = 1;

/// Format: a node's kind when its version left a directory at its path.
const DIRECTORY: u8 = 2;

/// Format: a node's source byte when it names no source.
const NO_SOURCE: u8 = 0;

/// Format: a node's source byte when it names a source.
const HAS_SOURCE: u8 = 1;

/// Format: the bytes of a node's count of parents, little-endian.
const COUNT_BYTES: usize = size_of::<u32>();

/// Format: the bytes of a generation, little-endian.
const GENERATION_BYTES: usize = size_of::<u64>();

/// Format: the bytes of a node's version name's length, little-endian.
const NAME_LEN_BYTES: usize = size_of::<u32>();

/// Format: the bytes of a node's payload before its name: its tag, source byte, kind and class, a byte each, then its
/// count of parents, its generation and its name's length.
const NODE_HEAD_BYTES: usize = COUNT_BYTES
  .saturating_add(GENERATION_BYTES)
  .saturating_add(NAME_LEN_BYTES)
  .saturating_add(4);

/// Format: the bytes of a value's payload: its tag, then its generation.
const VALUE_BYTES: usize = GENERATION_BYTES.saturating_add(1);

/// Format: a root version's generation, as git's topological level counts it and the ancestry type does.
const ROOT_GENERATION: u64 = 1;

/// Format: the byte between a path's names.
const SEPARATOR: u8 = b'/';

/// A version's renames and copies as path history reads them: each path its change makes from another (a destination,
/// a file or a directory, and each file and directory beneath a renamed or copied directory), with the path it came
/// from and the version that held it then.
pub type Sources<V> = BTreeMap<Vec<u8>, (Vec<u8>, V)>;

/// A history whose versions' changes, renames and copies path history reads.
pub trait PathSources: PathChanges {
  /// `version`'s renames and copies; `None` when the history holds no such version.
  fn sources(&self, version: Self::Version) -> Option<&Sources<Self::Version>>;
}

/// Why the type refused to derive a version or to answer a question. Each leaves the store as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryRefused {
  /// The history holds no version the derivation was asked for, or no change or sources for it.
  NoVersion,
  /// The derivation was given `given` values for `expected` inputs.
  Inputs {
    /// The inputs ([`super::DerivedType::inputs`]).
    expected: usize,
    /// The values given.
    given: usize,
  },
  /// `path`, a change's or a question's, names the reserved name a path's node is kept under.
  ReservedName {
    /// The path.
    path: Vec<u8>,
  },
  /// The source recorded for `path` does not hold: the version holds nothing there, the source version held nothing at
  /// the source path or something of another kind, or the source version is not older than the version.
  BadSource {
    /// The path.
    path: Vec<u8>,
  },
  /// The node or value `node` does not read as the type's, or contradicts what names it (a parent or a source no older
  /// than the node naming it).
  Malformed {
    /// The node.
    node: ContentId,
  },
  /// A generation would pass `u64::MAX`, or a node's parents or name would pass the width of their count.
  Exhausted,
  /// The path map refused: a path that is none, a change it refuses, a node of its that does not read.
  Paths(PathsRefused),
  /// The store refused: its budget, a count, a content not held, a node past the window.
  Content(ContentRefused),
}

impl From<ContentRefused> for HistoryRefused {
  fn from(refused: ContentRefused) -> HistoryRefused {
    HistoryRefused::Content(refused)
  }
}

impl From<PathsRefused> for HistoryRefused {
  fn from(refused: PathsRefused) -> HistoryRefused {
    match refused {
      PathsRefused::Content(refused) => HistoryRefused::Content(refused),
      refused => HistoryRefused::Paths(refused),
    }
  }
}

/// What the type's derivations have done, for the tests' non-vacuity checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistoryCounters {
  /// Values derived.
  pub derived: u64,
  /// Paths judged against the parents: each path of a change and each directory on one, the root's included, and no
  /// other.
  pub judged: u64,
  /// Nodes made: paths TREESAME to no parent.
  pub made: u64,
  /// Of them, nodes of no file: removals.
  pub removals: u64,
  /// Of them, nodes naming a source: a rename's or a copy's.
  pub sourced: u64,
  /// Paths that took a later parent's node, TREESAME to it and not to the first.
  pub diverted: u64,
}

impl HistoryCounters {
  fn add(self, more: HistoryCounters) -> HistoryCounters {
    HistoryCounters {
      derived: self.derived.saturating_add(more.derived),
      judged: self.judged.saturating_add(more.judged),
      made: self.made.saturating_add(more.made),
      removals: self.removals.saturating_add(more.removals),
      sourced: self.sourced.saturating_add(more.sourced),
      diverted: self.diverted.saturating_add(more.diverted),
    }
  }
}

/// What a version left at a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Made {
  /// Nothing: it removed what stood there.
  Absent,
  /// A file.
  File(Entry),
  /// A directory.
  Directory,
}

/// A version a path's history lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Logged {
  /// The bytes that name the version in its history ([`super::Graph::name`]).
  pub version: Vec<u8>,
  /// Its generation.
  pub generation: u64,
  /// The path it changed: the path asked, or with `--follow` a path a rename or a copy came from.
  pub path: Vec<u8>,
  /// What it left there.
  pub made: Made,
}

/// What a question cost: the census's evidence and the owner's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogCost {
  /// Trie nodes read to find the path's node in the version's history tree.
  pub lookups: u64,
  /// Nodes read from the store: the version's value and each path's node, once each.
  pub reads: u64,
  /// Versions listed.
  pub listed: u64,
  /// Sources followed.
  pub followed: u64,
}

/// The type (module docs), declared over a history `S`.
#[derive(Debug)]
pub struct PathHistory<S> {
  name: Vec<u8>,
  mode: Mode,
  maps: PathMap<S>,
  counters: HistoryCounters,
}

impl<S> PathHistory<S> {
  /// The type whose maps split by `window`, the store's, computed by `mode`. Refused for a window smaller than a branch
  /// of every child.
  pub fn declare(window: usize, mode: Mode) -> Result<PathHistory<S>, PathsRefused> {
    let maps = PathMap::declare(window, mode)?;
    let wide = u64::try_from(window).map_err(|_| PathsRefused::WindowTooSmall { window })?;
    let mut name = TYPE_NAME.to_vec();
    name.extend_from_slice(&wide.to_be_bytes());
    Ok(PathHistory {
      name,
      mode,
      maps,
      counters: HistoryCounters::default(),
    })
  }

  /// What its derivations have done.
  pub fn counters(&self) -> HistoryCounters {
    self.counters
  }

  /// What its maps (the versions' and the history trees') have done.
  pub fn map_counters(&self) -> PathCounters {
    self.maps.counters()
  }
}

impl<S: PathSources> DerivedType for PathHistory<S> {
  type Source = S;
  type Refused = HistoryRefused;

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
  ) -> Result<ContentId, HistoryRefused> {
    let given = given(source, store, version, inputs)?;
    let change = source.change(version).ok_or(HistoryRefused::NoVersion)?;
    let sources = source.sources(version).ok_or(HistoryRefused::NoVersion)?;
    let mut work = Work {
      store,
      maps: &mut self.maps,
      owned: Vec::new(),
      counters: HistoryCounters::default(),
    };
    let made = work.derive((change, sources), &given);
    let (root, counters) = work.settle(made);
    let root = root?;
    self.counters = self.counters.add(counters);
    self.counters.derived = self.counters.derived.saturating_add(1);
    Ok(root)
  }
}

/// [`PathHistory`]'s inputs: `version`'s parents, then each version its sources name that is not among them, in order.
fn inputs_of<S: PathSources>(source: &S, version: S::Version) -> Option<Vec<S::Version>> {
  let mut inputs = source.parents(version)?;
  let named: BTreeSet<S::Version> = source
    .sources(version)?
    .values()
    .map(|(_, from)| *from)
    .collect();
  for from in named {
    if !inputs.contains(&from) {
      inputs.push(from);
    }
  }
  Some(inputs)
}

/// What stands at a path, as TREESAME compares it: nothing, a file, or a directory by its trie's root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
  Absent,
  File(Entry),
  Directory(ContentId),
}

impl State {
  /// Whether both are files, or both directories.
  fn same_kind(self, other: State) -> bool {
    matches!(
      (self, other),
      (State::File(_), State::File(_)) | (State::Directory(_), State::Directory(_))
    )
  }

  /// What a node made for it holds.
  fn made(self) -> Made {
    match self {
      State::Absent => Made::Absent,
      State::File(entry) => Made::File(entry),
      State::Directory(_) => Made::Directory,
    }
  }
}

/// What stands at `path` in the map `map`.
fn state_at(store: &ContentStore, map: ContentId, path: &[u8]) -> Result<State, HistoryRefused> {
  Ok(match at(store, map, path)?.0 {
    None => State::Absent,
    Some(At::File(entry)) => State::File(entry),
    Some(At::Directory(root)) => State::Directory(root),
  })
}

/// The path a path's node is kept at in the history tree: `path/\0`, the root's `\0`.
fn key(path: &[u8]) -> Vec<u8> {
  let mut key = Vec::with_capacity(path.len().saturating_add(OWN.len()).saturating_add(1));
  if !path.is_empty() {
    key.extend_from_slice(path);
    key.push(SEPARATOR);
  }
  key.extend_from_slice(OWN);
  key
}

/// Whether `path` names the reserved name.
fn reserved(path: &[u8]) -> bool {
  path
    .split(|byte| *byte == SEPARATOR)
    .any(|name| name == OWN)
}

/// The node a history tree's lookup found: `None` for none; refused for what is no node's entry.
fn node_of(found: Option<At>, tree: ContentId) -> Result<Option<ContentId>, HistoryRefused> {
  match found {
    None => Ok(None),
    Some(At::File(Entry {
      class: NODE_CLASS,
      content,
    })) => Ok(Some(content)),
    Some(_) => Err(HistoryRefused::Malformed { node: tree }),
  }
}

/// The node of `path` in the history tree `tree`; `None` when the tree holds none.
fn node_at(
  store: &ContentStore,
  tree: ContentId,
  path: &[u8],
) -> Result<Option<ContentId>, HistoryRefused> {
  node_of(at(store, tree, &key(path))?.0, tree)
}

/// The paths a change makes its version judge: each path it names, each directory on one, and the root.
fn judged(change: &Change) -> BTreeSet<Vec<u8>> {
  let mut paths = BTreeSet::new();
  paths.insert(Vec::new());
  for path in change.keys() {
    for (end, byte) in path.iter().enumerate() {
      if *byte == SEPARATOR
        && let Some(directory) = path.get(..end)
      {
        paths.insert(directory.to_vec());
      }
    }
    paths.insert(path.clone());
  }
  paths
}

/// A version's value, read: its map, its history tree and its generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Value {
  map: ContentId,
  tree: ContentId,
  generation: u64,
}

/// The value `root` names: a node naming the map and the history tree, its payload its tag and generation.
fn read_value(store: &ContentStore, root: ContentId) -> Result<Value, HistoryRefused> {
  let malformed = HistoryRefused::Malformed { node: root };
  let Some(node) = store.derived(&root) else {
    return Err(malformed);
  };
  let mut names = node.names();
  let (Some(map), Some(tree), None) = (names.next(), names.next(), names.next()) else {
    return Err(malformed);
  };
  let Some((&VALUE, generation)) = node.payload().split_first() else {
    return Err(malformed);
  };
  let Ok(generation) = <[u8; GENERATION_BYTES]>::try_from(generation) else {
    return Err(malformed);
  };
  let generation = u64::from_le_bytes(generation);
  if generation < ROOT_GENERATION {
    return Err(malformed);
  }
  Ok(Value {
    map,
    tree,
    generation,
  })
}

/// A path's node, read or to be written.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
  parents: Vec<ContentId>,
  /// Its source's node and path.
  source: Option<(ContentId, Vec<u8>)>,
  made: Made,
  generation: u64,
  /// The bytes that name its version.
  name: Vec<u8>,
}

impl Node {
  /// The node's names and payload: the parents, the source's node, the file's content, as each is present; then the
  /// tag, the count of parents, the source byte, the kind, the class, the generation, the name's length, the name and
  /// the source's path. `None` past a count's width.
  fn encode(&self) -> Option<(Vec<ContentId>, Vec<u8>)> {
    let count = u32::try_from(self.parents.len()).ok()?;
    let name_len = u32::try_from(self.name.len()).ok()?;
    let mut names = self.parents.clone();
    names.extend(self.source.as_ref().map(|(node, _)| *node));
    let (kind, class) = match self.made {
      Made::Absent => (ABSENT, 0),
      Made::File(entry) => {
        names.push(entry.content);
        (FILE, entry.class)
      }
      Made::Directory => (DIRECTORY, 0),
    };
    let from: &[u8] = self
      .source
      .as_ref()
      .map_or(&[], |(_, path)| path.as_slice());
    let mut payload = Vec::with_capacity(
      NODE_HEAD_BYTES
        .saturating_add(self.name.len())
        .saturating_add(from.len()),
    );
    payload.push(NODE);
    payload.extend_from_slice(&count.to_le_bytes());
    payload.push(if self.source.is_some() {
      HAS_SOURCE
    } else {
      NO_SOURCE
    });
    payload.push(kind);
    payload.push(class);
    payload.extend_from_slice(&self.generation.to_le_bytes());
    payload.extend_from_slice(&name_len.to_le_bytes());
    payload.extend_from_slice(&self.name);
    payload.extend_from_slice(from);
    Some((names, payload))
  }
}

/// The node `id` names, read and checked on its own.
fn read_node(store: &ContentStore, id: ContentId) -> Result<Node, HistoryRefused> {
  let malformed = HistoryRefused::Malformed { node: id };
  let Some(node) = store.derived(&id) else {
    return Err(malformed);
  };
  let names: Vec<ContentId> = node.names().collect();
  decode(&names, node.payload()).ok_or(malformed)
}

/// A node's fields from its names and payload; `None` for bytes in any other form: a removal names no source and has a
/// parent (it removed something), a source names a path, and nothing is left over.
fn decode(names: &[ContentId], payload: &[u8]) -> Option<Node> {
  let Some((&NODE, rest)) = payload.split_first() else {
    return None;
  };
  let (count, rest) = rest.split_first_chunk::<COUNT_BYTES>()?;
  let (&flag, rest) = rest.split_first()?;
  let (&kind, rest) = rest.split_first()?;
  let (&class, rest) = rest.split_first()?;
  let (generation, rest) = rest.split_first_chunk::<GENERATION_BYTES>()?;
  let (name_len, rest) = rest.split_first_chunk::<NAME_LEN_BYTES>()?;
  let name_len = usize::try_from(u32::from_le_bytes(*name_len)).ok()?;
  let (name, from) = rest.split_at_checked(name_len)?;
  let count = usize::try_from(u32::from_le_bytes(*count)).ok()?;
  let mut names = names.iter().copied();
  let parents: Vec<ContentId> = names.by_ref().take(count).collect();
  let source = match flag {
    NO_SOURCE if from.is_empty() => None,
    HAS_SOURCE if !from.is_empty() => Some((names.next()?, from.to_vec())),
    _ => return None,
  };
  let made = match (kind, class) {
    (ABSENT, 0) if source.is_none() && count > 0 => Made::Absent,
    (FILE, class) => Made::File(Entry {
      class,
      content: names.next()?,
    }),
    (DIRECTORY, 0) => Made::Directory,
    _ => return None,
  };
  let generation = u64::from_le_bytes(*generation);
  let whole = parents.len() == count && names.next().is_none() && generation >= ROOT_GENERATION;
  whole.then(|| Node {
    parents,
    source,
    made,
    generation,
    name: name.to_vec(),
  })
}

/// What a derivation reads besides its change: its parents' values in order, every input's value by version, and the
/// version's generation and name.
struct Given<V> {
  parents: Vec<Value>,
  inputs: BTreeMap<V, Value>,
  generation: u64,
  name: Vec<u8>,
}

/// The inputs' values read, against `version`'s inputs.
fn given<S: PathSources>(
  source: &S,
  store: &ContentStore,
  version: S::Version,
  values: &[ContentId],
) -> Result<Given<S::Version>, HistoryRefused> {
  let parents = source.parents(version).ok_or(HistoryRefused::NoVersion)?;
  let expected = inputs_of(source, version).ok_or(HistoryRefused::NoVersion)?;
  if expected.len() != values.len() {
    return Err(HistoryRefused::Inputs {
      expected: expected.len(),
      given: values.len(),
    });
  }
  let mut inputs = BTreeMap::new();
  for (input, root) in expected.into_iter().zip(values) {
    inputs.insert(input, read_value(store, *root)?);
  }
  let parents: Vec<Value> = parents
    .iter()
    .filter_map(|parent| inputs.get(parent).copied())
    .collect();
  let generation = match parents.iter().map(|value| value.generation).max() {
    None => ROOT_GENERATION,
    Some(greatest) => greatest.checked_add(1).ok_or(HistoryRefused::Exhausted)?,
  };
  let mut name = Vec::new();
  source.name(version, &mut name);
  Ok(Given {
    parents,
    inputs,
    generation,
    name,
  })
}

/// Each destination's source resolved: its path and its node.
type Resolved = BTreeMap<Vec<u8>, (Vec<u8>, ContentId)>;

/// What a judged path's entry in the history tree becomes: kept as the first parent's (`None`), or set to a node or to
/// none.
type Judgement = Option<Option<ContentId>>;

/// One derivation's work: the store, the path map that applies the version's change and its history tree's, the holds
/// it took (each given back when it settles), and what it counted.
struct Work<'s, 'm, S> {
  store: &'s mut ContentStore,
  maps: &'m mut PathMap<S>,
  owned: Vec<ContentId>,
  counters: HistoryCounters,
}

impl<S> Work<'_, '_, S> {
  /// The version's value (module docs): its map, each judged path's node, its history tree, and the value naming both.
  fn derive<V: Ord>(
    &mut self,
    (change, sources): (&Change, &Sources<V>),
    given: &Given<V>,
  ) -> Result<ContentId, HistoryRefused> {
    if let Some(path) = change.keys().find(|path| reserved(path)) {
      return Err(HistoryRefused::ReservedName { path: path.clone() });
    }
    let first = given.parents.first().copied();
    let map = self
      .maps
      .apply(self.store, first.map(|value| value.map), change)?;
    self.owned.push(map);
    let resolved = self.resolve(sources, given, map)?;
    let mut edits = Change::new();
    for path in judged(change) {
      if let Some(entry) = self.judge(&path, given, map, &resolved)? {
        let entry = entry.map(|node| Entry {
          class: NODE_CLASS,
          content: node,
        });
        edits.insert(key(&path), entry);
      }
    }
    let tree = self
      .maps
      .apply(self.store, first.map(|value| value.tree), &edits)?;
    self.owned.push(tree);
    let mut payload = Vec::with_capacity(VALUE_BYTES);
    payload.push(VALUE);
    payload.extend_from_slice(&given.generation.to_le_bytes());
    Ok(self.store.put_derived(&[map, tree], &payload)?.identity)
  }

  /// Each source resolved to its path and node, every one checked (the variant's docs).
  fn resolve<V: Ord>(
    &mut self,
    sources: &Sources<V>,
    given: &Given<V>,
    map: ContentId,
  ) -> Result<Resolved, HistoryRefused> {
    let mut resolved = Resolved::new();
    for (path, (from_path, from)) in sources {
      let bad = || HistoryRefused::BadSource { path: path.clone() };
      let value = given.inputs.get(from).ok_or_else(bad)?;
      let now = state_at(self.store, map, path)?;
      let then = state_at(self.store, value.map, from_path)?;
      if !now.same_kind(then) || value.generation >= given.generation {
        return Err(bad());
      }
      let node = node_at(self.store, value.tree, from_path)?
        .ok_or(HistoryRefused::Malformed { node: value.tree })?;
      resolved.insert(path.clone(), (from_path.clone(), node));
    }
    Ok(resolved)
  }

  /// What `path`'s entry becomes (module docs): the first parent's it is TREESAME to, else a node made.
  fn judge<V>(
    &mut self,
    path: &[u8],
    given: &Given<V>,
    map: ContentId,
    resolved: &Resolved,
  ) -> Result<Judgement, HistoryRefused> {
    self.counters.judged = self.counters.judged.saturating_add(1);
    let now = state_at(self.store, map, path)?;
    if let Some(judgement) = self.treesame(path, given, now)? {
      return Ok(judgement);
    }
    if now == State::Absent && given.parents.is_empty() {
      return Ok(None);
    }
    self
      .make(path, given, now, resolved)
      .map(|node| Some(Some(node)))
  }

  /// The first parent `path` is TREESAME to, as a judgement: kept for the first parent, else that parent's entry.
  fn treesame<V>(
    &mut self,
    path: &[u8],
    given: &Given<V>,
    now: State,
  ) -> Result<Option<Judgement>, HistoryRefused> {
    for (index, parent) in given.parents.iter().enumerate() {
      if state_at(self.store, parent.map, path)? != now {
        continue;
      }
      if index == 0 {
        return Ok(Some(None));
      }
      self.counters.diverted = self.counters.diverted.saturating_add(1);
      return Ok(Some(Some(node_at(self.store, parent.tree, path)?)));
    }
    Ok(None)
  }

  /// A node made for `path`: its parents the parents' nodes in order, each once; its source, if recorded.
  fn make<V>(
    &mut self,
    path: &[u8],
    given: &Given<V>,
    now: State,
    resolved: &Resolved,
  ) -> Result<ContentId, HistoryRefused> {
    let mut parents = Vec::with_capacity(given.parents.len());
    for parent in &given.parents {
      if let Some(node) = node_at(self.store, parent.tree, path)?
        && !parents.contains(&node)
      {
        parents.push(node);
      }
    }
    let source = resolved.get(path).map(|(from, node)| (*node, from.clone()));
    let node = Node {
      parents,
      source,
      made: now.made(),
      generation: given.generation,
      name: given.name.clone(),
    };
    let (names, payload) = node.encode().ok_or(HistoryRefused::Exhausted)?;
    let identity = self.store.put_derived(&names, &payload)?.identity;
    self.owned.push(identity);
    self.counters.made = self.counters.made.saturating_add(1);
    self.counters.removals = self
      .counters
      .removals
      .saturating_add(u64::from(now == State::Absent));
    self.counters.sourced = self
      .counters
      .sourced
      .saturating_add(u64::from(node.source.is_some()));
    Ok(identity)
  }

  /// Ends the work: every hold it took given back (the value holds the map and the tree, and the tree each node), so
  /// the value is held once, by the caller; refused, the store as it was. Past a refusal the rest is given back still,
  /// and the first refusal is the answer.
  fn settle(
    self,
    made: Result<ContentId, HistoryRefused>,
  ) -> (Result<ContentId, HistoryRefused>, HistoryCounters) {
    let Work {
      store,
      owned,
      counters,
      ..
    } = self;
    let mut released: Result<(), HistoryRefused> = Ok(());
    for node in owned {
      if let Err(refused) = store.release(&node) {
        released = released.and(Err(refused.into()));
      }
    }
    (made.and_then(|root| released.map(|()| root)), counters)
  }
}

/// The history of `path` in the version whose value is `value` (module docs, "The query"): the versions that changed
/// it, newest first, at most `limit` of them; with `follow`, continued past each rename and copy into the source's
/// history. A full answer reads the value, the trie nodes on the path's key ([`LogCost::lookups`]) and at most one node
/// for each version listed; one cut at the limit reads at most the parents and sources of the versions it lists besides.
/// Refused for a path naming the reserved name or no path, a value or node that does not read as the type's or
/// contradicts what names it, or past the store's budget.
pub fn log(
  store: &mut ContentStore,
  value: ContentId,
  path: &[u8],
  follow: bool,
  limit: u64,
) -> Result<(Vec<Logged>, LogCost), HistoryRefused> {
  if reserved(path) {
    return Err(HistoryRefused::ReservedName {
      path: path.to_vec(),
    });
  }
  let mut walk = Walk::new(store);
  let answer = walk.log(value, path, (follow, limit));
  walk.finish(answer)
}

/// A node on the frontier, the newest first: its generation (the greatest first), its version's name, the path it is
/// read at, and the node.
type Pending = (Reverse<u64>, Vec<u8>, Vec<u8>, ContentId);

/// Shape: what one node a question keeps charges the store: its fields and its lists' bytes. Derived from the type.
fn node_bytes(node: &Node) -> u64 {
  let lists = node
    .parents
    .len()
    .saturating_mul(IDENTITY_BYTES)
    .saturating_add(node.name.len())
    .saturating_add(node.source.as_ref().map_or(0, |(_, path)| path.len()));
  u64::try_from(size_of::<Node>().saturating_add(lists)).unwrap_or(u64::MAX)
}

/// Shape: what one node waiting at a path charges the store, its frontier entry and its entry among those seen, for a
/// path of `path` bytes and a version name of `name`. Derived from the types.
fn pending_bytes(path: usize, name: usize) -> u64 {
  let entries = size_of::<Pending>().saturating_add(size_of::<(ContentId, Vec<u8>)>());
  let lists = path.saturating_mul(2).saturating_add(name);
  u64::try_from(entries.saturating_add(lists)).unwrap_or(u64::MAX)
}

/// One question's reading of the nodes: each read once, its frontier, what it has seen, what it keeps charged to the
/// store, and what it cost.
struct Walk<'s> {
  store: &'s mut ContentStore,
  nodes: BTreeMap<ContentId, Node>,
  frontier: BTreeSet<Pending>,
  seen: BTreeSet<(ContentId, Vec<u8>)>,
  charged: u64,
  cost: LogCost,
}

impl<'s> Walk<'s> {
  fn new(store: &'s mut ContentStore) -> Walk<'s> {
    Walk {
      store,
      nodes: BTreeMap::new(),
      frontier: BTreeSet::new(),
      seen: BTreeSet::new(),
      charged: 0,
      cost: LogCost::default(),
    }
  }

  /// Charges the store for `bytes` more kept.
  fn charge(&mut self, bytes: u64) -> Result<(), HistoryRefused> {
    self.store.charge(bytes)?;
    self.charged = self.charged.saturating_add(bytes);
    Ok(())
  }

  /// [`log`]'s walk: the path's node found, then the frontier listed newest first.
  fn log(
    &mut self,
    value: ContentId,
    path: &[u8],
    (follow, limit): (bool, u64),
  ) -> Result<Vec<Logged>, HistoryRefused> {
    let value = read_value(self.store, value)?;
    self.cost.reads = self.cost.reads.saturating_add(1);
    let (found, lookups) = at(self.store, value.tree, &key(path))?;
    self.cost.lookups = lookups;
    if let Some(start) = node_of(found, value.tree)? {
      self.push(start, path.to_vec(), value.generation.saturating_add(1))?;
    }
    let mut answer = Vec::new();
    while self.cost.listed < limit {
      let Some((_, _, at_path, id)) = self.frontier.pop_first() else {
        break;
      };
      let node = self
        .nodes
        .get(&id)
        .cloned()
        .ok_or(HistoryRefused::Malformed { node: id })?;
      self.next(&node, &at_path, follow)?;
      answer.push(Logged {
        version: node.name,
        generation: node.generation,
        path: at_path,
        made: node.made,
      });
      self.cost.listed = self.cost.listed.saturating_add(1);
    }
    Ok(answer)
  }

  /// Puts on the frontier what a listed node leads to: its parents at its path, and with `follow` its source at the
  /// source's path.
  fn next(&mut self, node: &Node, path: &[u8], follow: bool) -> Result<(), HistoryRefused> {
    for parent in &node.parents {
      self.push(*parent, path.to_vec(), node.generation)?;
    }
    if follow && let Some((source, from)) = &node.source {
      self.cost.followed = self.cost.followed.saturating_add(1);
      self.push(*source, from.clone(), node.generation)?;
    }
    Ok(())
  }

  /// Puts `id` on the frontier at `path`, once; refused when it is no older than `below`, the generation of what names
  /// it.
  fn push(&mut self, id: ContentId, path: Vec<u8>, below: u64) -> Result<(), HistoryRefused> {
    let seen = (id, path);
    if self.seen.contains(&seen) {
      return Ok(());
    }
    let (generation, name) = self.read(id)?;
    if generation >= below {
      return Err(HistoryRefused::Malformed { node: id });
    }
    self.charge(pending_bytes(seen.1.len(), name.len()))?;
    self
      .frontier
      .insert((Reverse(generation), name, seen.1.clone(), id));
    self.seen.insert(seen);
    Ok(())
  }

  /// The generation and version name of the node `id`, read from the store the first time.
  fn read(&mut self, id: ContentId) -> Result<(u64, Vec<u8>), HistoryRefused> {
    if let Some(node) = self.nodes.get(&id) {
      return Ok((node.generation, node.name.clone()));
    }
    let node = read_node(self.store, id)?;
    self.charge(node_bytes(&node))?;
    self.cost.reads = self.cost.reads.saturating_add(1);
    let found = (node.generation, node.name.clone());
    self.nodes.insert(id, node);
    Ok(found)
  }

  /// Gives back what the walk charged, and its answer with its cost; past a refusal of either, the first.
  fn finish<T>(self, answer: Result<T, HistoryRefused>) -> Result<(T, LogCost), HistoryRefused> {
    let discharged = self.store.discharge(self.charged);
    let answer = answer?;
    discharged?;
    Ok((answer, self.cost))
  }
}
