//! A version's map of paths to contents, as derived data (A-647 part 1; A-645 "Derived data, one mechanism"; §4.17.1
//! "A version": "its tree: the snapshot of the files at the branch's head").
//!
//! **The value.** Every file of a version, by path: its class (its tool's distinction among files, opaque here) and its
//! bytes' content in the store. The value is a tree of derived nodes ([`crate::content::Form::Derived`]): one trie a
//! directory, whose entries name each file's content or the root of the directory beneath, so a version that changes
//! one file shares every other directory with its parent by identity, as git's trees and Mercurial's manifests share
//! them [C: git `Documentation/gitformat-index`'s trees; `research/edenfs-scale-distribution.md` §2.1, "a tree entry
//! has the name of the child entry as well as the object ID"].
//!
//! **A directory's trie.** A directory is one leaf, its entries in name order, while that leaf fits the window, the
//! store's longest content (D-6). Past it, the directory is a branch over the digits of each name's place, a hash of the
//! name read four bits a level, with a node beneath for each digit its names have, each made by the same rule a level
//! down: a hash array mapped trie [A: Bagwell, "Ideal Hash Trees", EPFL 2001], here with sixteen children a branch so a
//! full branch stays about half a kilobyte and the window's room goes to the leaves. The shape is a function of the
//! directory's names alone, never of the order they arrived in, so the same map is the same tree and the same root
//! identity on every tier: the condition for keeping a value under its version with no coordination
//! ([`super::Derivations`]). A branch records what its entries would take in one leaf, so whether a changed branch now
//! fits one, and so must become one, is known without reading beneath it. The window is part of the type's name, since
//! the shape depends on it.
//!
//! **Incremental** ([`PathMap::apply`], the type's derivation). A version's change is each path it sets, to a file, or
//! removes, stated against its first parent's map; other parents name what it merges and change nothing here. The
//! derivation looks up, from the root down, only the directories on the change's paths, then makes them again from the
//! deepest up: in each, only the trie nodes on its changed names' places, reusing every other node by identity (path
//! copying [A: Driscoll, Sarnak, Sleator and Tarjan, JCSS 1989]). So it costs the change's directories and, in each, a
//! trie's depth of nodes, never the map ([`PathCounters::directories`] counts the directories it entered).
//!
//! **From scratch** ([`PathMap::build`]): the same map made from its files alone, every directory made. The oracle
//! compares the two: an incremental value equals the build of its version's files, identity for identity.
//!
//! **Refusals** ([`PathsRefused`]): a path with an empty name, `.` or `..`; a removal of a path holding no file; a path
//! that would be a file and a directory at once; an entry alone past the window; a node that does not read as the
//! map's; and the store's own. The change is judged whole: a refused derivation leaves the store as it was.
//!
//! No I/O, no clock, no randomness, no panic.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::mem::size_of;

use crate::content::{
  ContentId, ContentRefused, ContentStore, DerivedNode, IDENTITY_BYTES, NAMES_COUNT_BYTES,
};

use super::{DerivedType, Graph, Mode};

/// Format: the type's name and its format's version; the window it splits by follows, eight bytes, big-endian.
const TYPE_NAME: &[u8] = b"slates-vcs paths 1 ";

/// Format: the first byte of a leaf's payload.
const LEAF: u8 = b'L';

/// Format: the first byte of a branch's payload.
const BRANCH: u8 = b'B';

/// Format: the bytes of a payload's tag.
const TAG_BYTES: usize = size_of::<u8>();

/// Format: the bytes of an entry's name's length in a leaf, little-endian.
const NAME_LEN_BYTES: usize = size_of::<u32>();

/// Format: the bytes of an entry's kind, after its name.
const KIND_BYTES: usize = size_of::<u8>();

/// Format: the bytes of an entry's class, after its kind.
const CLASS_BYTES: usize = size_of::<u8>();

/// Format: an entry's kind: a file.
const FILE: u8 = 0;

/// Format: an entry's kind: a directory, whose class is zero.
const DIRECTORY: u8 = 1;

/// Format: the bytes of a branch's bitmap, one bit for each digit it has a child for, little-endian.
const BITMAP_BYTES: usize = size_of::<u16>();

/// Format: the bytes of a branch's cost, what its entries would take in one leaf, little-endian.
const COST_BYTES: usize = size_of::<u64>();

/// Format: a branch's children at most: one a digit, as many as its bitmap's bits.
const FANOUT: u8 = 16;

/// Format: a digit's bits: a level of a directory's trie reads one hexadecimal digit of a name's place.
const DIGIT_BITS: u32 = 4;

/// Format: the digits a byte of a place holds, the high one first.
const DIGITS_PER_BYTE: usize = 2;

/// Format: the mask of a byte's low digit.
const LOW_DIGIT: u8 = 0x0f;

/// Format: the levels a directory's trie has at most: one a digit of a place.
const LEVELS: usize = IDENTITY_BYTES.saturating_mul(DIGITS_PER_BYTE);

/// Format: the context a name's place is derived under, in the form the `blake3` crate's `derive_key` documents.
const PLACE_CONTEXT: &str = "slates-vcs 2026-11-19 derived paths: a name's place in its directory";

/// Format: the byte between a path's names.
const SEPARATOR: u8 = b'/';

/// A file in a version's map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entry {
  /// Its class: its tool's distinction among files (git's mode, Mercurial's flags), opaque to the map.
  pub class: u8,
  /// Its bytes' content in the store: a window's identity, or the root of an object's windows.
  pub content: ContentId,
}

/// A version's change as the path map reads it: each path the version sets, to its file, or removes (`None`), stated
/// against its first parent's map, or an empty map for a root.
pub type Change = BTreeMap<Vec<u8>, Option<Entry>>;

/// A history whose versions' changes the path map reads.
pub trait PathChanges: Graph {
  /// `version`'s change; `None` when the history holds no such version.
  fn change(&self, version: Self::Version) -> Option<&Change>;
}

/// Why the path map refused (module docs). Each leaves the store as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathsRefused {
  /// The history holds no change for the version.
  NoChange,
  /// `path` is no path of a map: empty, or with a name empty, `.` or `..`.
  BadPath {
    /// The path.
    path: Vec<u8>,
  },
  /// The change removes `path`, which holds no file in the parent's map.
  RemovedAbsent {
    /// The path.
    path: Vec<u8>,
  },
  /// `path` would be a file and a directory at once: a file set where files stay beneath it, or files set beneath a
  /// file that stays.
  Shadowed {
    /// The path.
    path: Vec<u8>,
  },
  /// The entry `name` alone is past the window: no leaf could hold it.
  EntryPastWindow {
    /// The name.
    name: Vec<u8>,
  },
  /// A directory's names agree in every digit of their places and are past the window together: no split holds them.
  Unsplittable,
  /// The node `node` does not read as a node of the map.
  Malformed {
    /// The node.
    node: ContentId,
  },
  /// The window is smaller than a branch of every child.
  WindowTooSmall {
    /// The window asked for.
    window: usize,
  },
  /// The store refused.
  Content(ContentRefused),
}

impl From<ContentRefused> for PathsRefused {
  fn from(refused: ContentRefused) -> PathsRefused {
    PathsRefused::Content(refused)
  }
}

/// What the path map has done, for the tests' non-vacuity checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathCounters {
  /// Directories made: in a derivation, the root and each directory on the change's paths, and no other; in a build,
  /// every directory.
  pub directories: u64,
  /// Nodes put: stored new, or found held.
  pub nodes_put: u64,
  /// Nodes read by derivations and builds.
  pub nodes_read: u64,
}

impl PathCounters {
  fn add(self, more: PathCounters) -> PathCounters {
    PathCounters {
      directories: self.directories.saturating_add(more.directories),
      nodes_put: self.nodes_put.saturating_add(more.nodes_put),
      nodes_read: self.nodes_read.saturating_add(more.nodes_read),
    }
  }
}

/// The path map, declared over a history `S` (module docs).
#[derive(Debug)]
pub struct PathMap<S> {
  name: Vec<u8>,
  mode: Mode,
  window: u64,
  counters: PathCounters,
  history: PhantomData<fn(&S)>,
}

impl<S> PathMap<S> {
  /// The path map split by `window`, the store's, computed by `mode`. Refused for a window smaller than a branch of
  /// every child.
  pub fn declare(window: usize, mode: Mode) -> Result<PathMap<S>, PathsRefused> {
    let wide = u64::try_from(window).map_err(|_| PathsRefused::WindowTooSmall { window })?;
    if window < full_branch_bytes() {
      return Err(PathsRefused::WindowTooSmall { window });
    }
    let mut name = TYPE_NAME.to_vec();
    name.extend_from_slice(&wide.to_be_bytes());
    Ok(PathMap {
      name,
      mode,
      window: wide,
      counters: PathCounters::default(),
      history: PhantomData,
    })
  }

  /// What the map has done.
  pub fn counters(&self) -> PathCounters {
    self.counters
  }

  /// The map of `files` made from scratch, every directory made: its root, held once by the caller.
  pub fn build(
    &mut self,
    store: &mut ContentStore,
    files: &BTreeMap<Vec<u8>, Entry>,
  ) -> Result<ContentId, PathsRefused> {
    let mut work = Work::new(store, self.window);
    let made = work.build(files);
    self.settle(work, made)
  }

  /// The map `change` makes of `parent`'s (`None`: an empty map), made incrementally (module docs): its root, held once
  /// by the caller.
  pub fn apply(
    &mut self,
    store: &mut ContentStore,
    parent: Option<ContentId>,
    change: &Change,
  ) -> Result<ContentId, PathsRefused> {
    let mut work = Work::new(store, self.window);
    let made = work.apply(parent, change);
    self.settle(work, made)
  }

  /// Ends a work: one hold kept on its root for the caller, every other hold its puts took given back; refused, all of
  /// them. Past a refusal the rest is given back still, and the first refusal is the answer.
  fn settle(
    &mut self,
    work: Work<'_>,
    made: Result<ContentId, PathsRefused>,
  ) -> Result<ContentId, PathsRefused> {
    let Work {
      store,
      owned,
      counters,
      ..
    } = work;
    self.counters = self.counters.add(counters);
    let mut kept = false;
    let mut released: Result<(), PathsRefused> = Ok(());
    for node in owned {
      if !kept && made.as_ref().is_ok_and(|root| *root == node) {
        kept = true;
        continue;
      }
      if let Err(refused) = store.release(&node) {
        released = released.and(Err(PathsRefused::Content(refused)));
      }
    }
    let root = made?;
    released?;
    Ok(root)
  }

  /// The file at `path` in the map `root`.
  pub fn get(
    &self,
    store: &ContentStore,
    root: ContentId,
    path: &[u8],
  ) -> Result<Option<Entry>, PathsRefused> {
    let names = split_path(path)?;
    let last = names.len().saturating_sub(1);
    let mut directory = root;
    for (at, name) in names.into_iter().enumerate() {
      match lookup(&mut |node| read_node(store, node), directory, name)? {
        Some(Slot::Directory(below)) if at < last => directory = below,
        Some(Slot::File(entry)) if at == last => return Ok(Some(entry)),
        _ => return Ok(None),
      }
    }
    Ok(None)
  }

  /// Every file of the map `root`, by path: a walk of the whole map.
  pub fn files(
    &self,
    store: &ContentStore,
    root: ContentId,
  ) -> Result<BTreeMap<Vec<u8>, Entry>, PathsRefused> {
    let mut files = BTreeMap::new();
    let mut directories = vec![(Vec::new(), root)];
    while let Some((prefix, directory)) = directories.pop() {
      for (name, slot) in gather(&mut |node| read_node(store, node), [directory])? {
        let path = join(&prefix, &name);
        match slot {
          Slot::File(entry) => {
            files.insert(path, entry);
          }
          Slot::Directory(below) => directories.push((path, below)),
        }
      }
    }
    Ok(files)
  }
}

impl<S: PathChanges> DerivedType for PathMap<S> {
  type Source = S;
  type Refused = PathsRefused;

  fn name(&self) -> &[u8] {
    &self.name
  }

  fn mode(&self) -> Mode {
    self.mode
  }

  fn derive(
    &mut self,
    source: &S,
    store: &mut ContentStore,
    version: S::Version,
    parents: &[ContentId],
  ) -> Result<ContentId, PathsRefused> {
    let change = source.change(version).ok_or(PathsRefused::NoChange)?;
    self.apply(store, parents.first().copied(), change)
  }
}

/// What a name in a directory holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
  File(Entry),
  Directory(ContentId),
}

impl Slot {
  /// The content it names: the file's, or the directory's root.
  fn content(self) -> ContentId {
    match self {
      Slot::File(entry) => entry.content,
      Slot::Directory(root) => root,
    }
  }

  /// Its kind and class as a leaf writes them.
  fn kind_and_class(self) -> (u8, u8) {
    match self {
      Slot::File(entry) => (FILE, entry.class),
      Slot::Directory(_) => (DIRECTORY, 0),
    }
  }
}

/// A directory's names, read or to be written, in name order.
type Entries = BTreeMap<Vec<u8>, Slot>;

/// A build's directories by path, each name in one with its slot: a file, or a directory's trie once it is made
/// (`None` until then).
type Directories = BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Option<Slot>>>;

/// A name's place in its directory's trie: its hash, read a digit a level.
type Place = [u8; IDENTITY_BYTES];

/// The place of `name`.
fn place(name: &[u8]) -> Place {
  let mut hasher = blake3::Hasher::new_derive_key(PLACE_CONTEXT);
  hasher.update(name);
  *hasher.finalize().as_bytes()
}

/// The digit of `place` that level `level` reads; `None` past the last.
fn digit(place: &Place, level: usize) -> Option<u8> {
  let byte = *place.get(level.checked_div(DIGITS_PER_BYTE)?)?;
  if level.is_multiple_of(DIGITS_PER_BYTE) {
    Some(byte.wrapping_shr(DIGIT_BITS))
  } else {
    Some(byte & LOW_DIGIT)
  }
}

/// The bitmap's bit for `digit`.
fn bit(digit: u8) -> u16 {
  1_u16.wrapping_shl(u32::from(digit))
}

/// The child a branch keeps for `digit`: its position among the children, or `None` when the bitmap has none.
fn child_at(bitmap: u16, digit: u8) -> Option<usize> {
  let bit = bit(digit);
  if bitmap & bit == 0 {
    return None;
  }
  usize::try_from((bitmap & bit.wrapping_sub(1)).count_ones()).ok()
}

/// A byte count as costs are kept.
fn wide(bytes: usize) -> u64 {
  u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// What `name`'s entry takes in a leaf: its identity among the node's names, its name's length and bytes, its kind and
/// its class.
fn entry_cost(name: &[u8]) -> u64 {
  wide(
    IDENTITY_BYTES
      .saturating_add(NAME_LEN_BYTES)
      .saturating_add(name.len())
      .saturating_add(KIND_BYTES)
      .saturating_add(CLASS_BYTES),
  )
}

/// What entries costing `cost` take as one leaf: the node's count of names and the leaf's tag besides.
fn leaf_bytes(cost: u64) -> u64 {
  cost.saturating_add(wide(NAMES_COUNT_BYTES.saturating_add(TAG_BYTES)))
}

/// What a branch of every child takes: its count of names, the children's identities, its tag, bitmap and cost.
fn full_branch_bytes() -> usize {
  NAMES_COUNT_BYTES
    .saturating_add(usize::from(FANOUT).saturating_mul(IDENTITY_BYTES))
    .saturating_add(TAG_BYTES)
    .saturating_add(BITMAP_BYTES)
    .saturating_add(COST_BYTES)
}

/// What `entries` take in a leaf, their frame aside.
fn cost_of(entries: &Entries) -> u64 {
  entries
    .keys()
    .fold(0_u64, |cost, name| cost.saturating_add(entry_cost(name)))
}

/// Whether `name` can be a name in a directory: not empty, not `.` or `..`, and holding no separator.
fn valid_name(name: &[u8]) -> bool {
  !name.is_empty() && name != b"." && name != b".." && !name.contains(&SEPARATOR)
}

/// `path`'s names, or the refusal of a path that is no path of a map.
fn split_path(path: &[u8]) -> Result<Vec<&[u8]>, PathsRefused> {
  let names: Vec<&[u8]> = path.split(|byte| *byte == SEPARATOR).collect();
  if names.iter().all(|name| valid_name(name)) {
    Ok(names)
  } else {
    Err(PathsRefused::BadPath {
      path: path.to_vec(),
    })
  }
}

/// `name` beneath the directory `prefix` (the root's names have none).
fn join(prefix: &[u8], name: &[u8]) -> Vec<u8> {
  let mut path = Vec::with_capacity(prefix.len().saturating_add(name.len()).saturating_add(1));
  if !prefix.is_empty() {
    path.extend_from_slice(prefix);
    path.push(SEPARATOR);
  }
  path.extend_from_slice(name);
  path
}

/// `directory`'s parent and its own name; `None` for the root.
fn parent_of(directory: &[u8]) -> Option<(&[u8], &[u8])> {
  if directory.is_empty() {
    return None;
  }
  match directory.iter().rposition(|byte| *byte == SEPARATOR) {
    Some(at) => Some((directory.get(..at)?, directory.get(at.checked_add(1)?..)?)),
    None => Some((&[], directory)),
  }
}

/// A node of a directory's trie, read.
enum Node {
  Leaf(Entries),
  Branch {
    bitmap: u16,
    cost: u64,
    children: Vec<ContentId>,
  },
}

/// The node `id`, read from the store.
fn read_node(store: &ContentStore, id: ContentId) -> Result<Node, PathsRefused> {
  let malformed = PathsRefused::Malformed { node: id };
  let Some(node) = store.derived(&id) else {
    return Err(malformed);
  };
  let read = match node.payload().split_first() {
    Some((&LEAF, rest)) => read_leaf(&node, rest).map(Node::Leaf),
    Some((&BRANCH, rest)) => read_branch(&node, rest),
    _ => None,
  };
  read.ok_or(malformed)
}

/// A leaf's entries: one for each name the node names, in name order, nothing left over.
fn read_leaf(node: &DerivedNode<'_>, mut rest: &[u8]) -> Option<Entries> {
  let mut entries = Entries::new();
  for content in node.names() {
    let (len, after) = rest.split_first_chunk::<NAME_LEN_BYTES>()?;
    let (name, after) = after.split_at_checked(usize::try_from(u32::from_le_bytes(*len)).ok()?)?;
    let (kind, after) = after.split_first()?;
    let (class, after) = after.split_first()?;
    let slot = match (*kind, *class) {
      (FILE, class) => Slot::File(Entry { class, content }),
      (DIRECTORY, 0) => Slot::Directory(content),
      _ => return None,
    };
    let ordered = entries
      .last_key_value()
      .is_none_or(|(last, _)| last.as_slice() < name);
    if !ordered || !valid_name(name) {
      return None;
    }
    entries.insert(name.to_vec(), slot);
    rest = after;
  }
  rest.is_empty().then_some(entries)
}

/// A branch: its bitmap, its cost, and one child for each bit, nothing left over.
fn read_branch(node: &DerivedNode<'_>, rest: &[u8]) -> Option<Node> {
  let (bitmap, rest) = rest.split_first_chunk::<BITMAP_BYTES>()?;
  let (cost, rest) = rest.split_first_chunk::<COST_BYTES>()?;
  let bitmap = u16::from_le_bytes(*bitmap);
  let counted = usize::try_from(bitmap.count_ones()).ok()?;
  if !rest.is_empty() || bitmap == 0 || counted != node.len() {
    return None;
  }
  Some(Node::Branch {
    bitmap,
    cost: u64::from_le_bytes(*cost),
    children: node.names().collect(),
  })
}

/// What `name` holds in the directory whose trie is `root`, read through `read`.
fn lookup(
  read: &mut impl FnMut(ContentId) -> Result<Node, PathsRefused>,
  root: ContentId,
  name: &[u8],
) -> Result<Option<Slot>, PathsRefused> {
  let place = place(name);
  let mut node = root;
  for level in 0..=LEVELS {
    match read(node)? {
      Node::Leaf(entries) => return Ok(entries.get(name).copied()),
      Node::Branch {
        bitmap, children, ..
      } => {
        let Some(digit) = digit(&place, level) else {
          break;
        };
        let Some(at) = child_at(bitmap, digit) else {
          return Ok(None);
        };
        node = *children.get(at).ok_or(PathsRefused::Malformed { node })?;
      }
    }
  }
  Err(PathsRefused::Malformed { node })
}

/// Every entry of the tries `roots`, read whole through `read`.
fn gather(
  read: &mut impl FnMut(ContentId) -> Result<Node, PathsRefused>,
  roots: impl IntoIterator<Item = ContentId>,
) -> Result<Entries, PathsRefused> {
  let mut entries = Entries::new();
  let mut pending: Vec<ContentId> = roots.into_iter().collect();
  while let Some(node) = pending.pop() {
    match read(node)? {
      Node::Leaf(leaf) => entries.extend(leaf),
      Node::Branch { children, .. } => pending.extend(children),
    }
  }
  Ok(entries)
}

/// A leaf's entries under `edits`.
fn edited(mut entries: Entries, edits: Vec<Edit>) -> Entries {
  for edit in edits {
    match edit.slot {
      Some(slot) => entries.insert(edit.name, slot),
      None => entries.remove(&edit.name),
    };
  }
  entries
}

/// The children of a branch by digit.
fn children_by_digit(bitmap: u16, children: &[ContentId]) -> Option<BTreeMap<u8, ContentId>> {
  let digits: Vec<u8> = (0..FANOUT)
    .filter(|digit| bitmap & bit(*digit) != 0)
    .collect();
  (digits.len() == children.len())
    .then(|| digits.into_iter().zip(children.iter().copied()).collect())
}

/// A trie node made: its root, and what its entries would take in one leaf.
#[derive(Clone, Copy, Debug)]
struct Made {
  root: ContentId,
  cost: u64,
}

/// A name's change in a directory: its place, its name, and what it holds after (`None`: nothing).
struct Edit {
  place: Place,
  name: Vec<u8>,
  slot: Option<Slot>,
}

/// A directory the change reaches.
#[derive(Debug, Default)]
struct Reached {
  /// Its trie's root in the parent's map: `None` when no directory stood at its path.
  old: Option<ContentId>,
  /// The names in it the change touches.
  names: BTreeMap<Vec<u8>, Touched>,
}

/// A name the change touches.
#[derive(Debug, Default)]
struct Touched {
  /// What it held in the parent's map.
  old: Option<Slot>,
  /// The change of the file at it: set (`Some(Some(..))`) or removed (`Some(None)`).
  file: Option<Option<Entry>>,
  /// Whether the change reaches beneath it.
  beneath: bool,
  /// The trie of the directory beneath it after the change, once made: `None` when nothing stands beneath.
  below: Option<ContentId>,
}

/// What is wrong with a touched name, before its path is known.
enum Wrong {
  RemovedAbsent,
  Shadowed,
}

/// What a touched name holds after the change (module docs' refusals).
fn decide(touched: &Touched) -> Result<Option<Slot>, Wrong> {
  let below = touched.beneath.then_some(touched.below);
  let old_file = matches!(touched.old, Some(Slot::File(_)));
  let old_directory = matches!(touched.old, Some(Slot::Directory(_)));
  match (touched.file, below) {
    (Some(None), _) if !old_file => Err(Wrong::RemovedAbsent),
    (Some(Some(_)), Some(Some(_))) => Err(Wrong::Shadowed),
    (None, Some(Some(_))) if old_file => Err(Wrong::Shadowed),
    (Some(None) | None, Some(Some(root))) => Ok(Some(Slot::Directory(root))),
    (Some(Some(_)), None) if old_directory => Err(Wrong::Shadowed),
    (Some(Some(entry)), _) => Ok(Some(Slot::File(entry))),
    (Some(None), _) => Ok(None),
    (None, Some(None)) => Ok(touched.old.filter(|slot| matches!(slot, Slot::File(_)))),
    (None, None) => Ok(touched.old),
  }
}

/// The edits of `directory`'s touched names, each decided (its refusal named by its path).
fn edits_of(
  directory: &[u8],
  names: BTreeMap<Vec<u8>, Touched>,
) -> Result<Vec<Edit>, PathsRefused> {
  let mut edits = Vec::with_capacity(names.len());
  for (name, touched) in names {
    let slot = decide(&touched).map_err(|wrong| {
      let path = join(directory, &name);
      match wrong {
        Wrong::RemovedAbsent => PathsRefused::RemovedAbsent { path },
        Wrong::Shadowed => PathsRefused::Shadowed { path },
      }
    })?;
    edits.push(Edit {
      place: place(&name),
      name,
      slot,
    });
  }
  Ok(edits)
}

/// One build's or derivation's work: the store, the window, the nodes it put (each with the hold its put gave, given
/// back when it settles), and what it counted.
struct Work<'s> {
  store: &'s mut ContentStore,
  window: u64,
  owned: Vec<ContentId>,
  counters: PathCounters,
}

impl<'s> Work<'s> {
  fn new(store: &'s mut ContentStore, window: u64) -> Work<'s> {
    Work {
      store,
      window,
      owned: Vec::new(),
      counters: PathCounters::default(),
    }
  }

  /// Reads `node`, counted.
  fn read(&mut self, node: ContentId) -> Result<Node, PathsRefused> {
    self.counters.nodes_read = self.counters.nodes_read.saturating_add(1);
    read_node(self.store, node)
  }

  /// Puts a node, its hold owned by this work.
  fn put(&mut self, names: &[ContentId], payload: &[u8]) -> Result<ContentId, PathsRefused> {
    let stored = self.store.put_derived(names, payload)?;
    self.owned.push(stored.identity);
    self.counters.nodes_put = self.counters.nodes_put.saturating_add(1);
    Ok(stored.identity)
  }

  /// Whether entries costing `cost` fit one leaf.
  fn fits(&self, cost: u64) -> bool {
    leaf_bytes(cost) <= self.window
  }

  /// A leaf of `entries`, which fit one.
  fn put_leaf(&mut self, entries: &Entries) -> Result<ContentId, PathsRefused> {
    let mut names = Vec::with_capacity(entries.len());
    let mut payload = vec![LEAF];
    for (name, slot) in entries {
      let len = u32::try_from(name.len())
        .map_err(|_| PathsRefused::EntryPastWindow { name: name.clone() })?;
      payload.extend_from_slice(&len.to_le_bytes());
      payload.extend_from_slice(name);
      let (kind, class) = slot.kind_and_class();
      payload.push(kind);
      payload.push(class);
      names.push(slot.content());
    }
    self.put(&names, &payload)
  }

  /// A branch of `children`, in digit order, under `bitmap`, its entries costing `cost`.
  fn put_branch(
    &mut self,
    bitmap: u16,
    cost: u64,
    children: &[ContentId],
  ) -> Result<Made, PathsRefused> {
    let mut payload = Vec::with_capacity(
      TAG_BYTES
        .saturating_add(BITMAP_BYTES)
        .saturating_add(COST_BYTES),
    );
    payload.push(BRANCH);
    payload.extend_from_slice(&bitmap.to_le_bytes());
    payload.extend_from_slice(&cost.to_le_bytes());
    let root = self.put(children, &payload)?;
    Ok(Made { root, cost })
  }

  /// The node of `entries` at `level` of a directory's trie, by the rule (module docs): one leaf while they fit, else a
  /// branch over their digits at `level`. `None` for no entries.
  fn put_set(&mut self, entries: Entries, level: usize) -> Result<Option<Made>, PathsRefused> {
    if entries.is_empty() {
      return Ok(None);
    }
    let cost = cost_of(&entries);
    if self.fits(cost) {
      let root = self.put_leaf(&entries)?;
      return Ok(Some(Made { root, cost }));
    }
    self.split(entries, level, cost).map(Some)
  }

  /// A branch over `entries`' digits at `level`, a node a digit beneath it.
  fn split(&mut self, entries: Entries, level: usize, cost: u64) -> Result<Made, PathsRefused> {
    if entries.len() == 1
      && let Some(name) = entries.keys().next()
    {
      return Err(PathsRefused::EntryPastWindow { name: name.clone() });
    }
    let mut groups: BTreeMap<u8, Entries> = BTreeMap::new();
    for (name, slot) in entries {
      let digit = digit(&place(&name), level).ok_or(PathsRefused::Unsplittable)?;
      groups.entry(digit).or_default().insert(name, slot);
    }
    let mut bitmap = 0_u16;
    let mut children = Vec::with_capacity(groups.len());
    for (digit, group) in groups {
      if let Some(made) = self.put_set(group, level.saturating_add(1))? {
        bitmap |= bit(digit);
        children.push(made.root);
      }
    }
    self.put_branch(bitmap, cost, &children)
  }

  /// The node at `level` that `node` (`None`: no entries) becomes under `edits`, whose places agree with each other in
  /// every digit above `level`; and the old node's cost. Only the nodes on the edits' places are read and made; a node
  /// whose entries come to fit one leaf becomes one, and one left without entries, none.
  fn update(
    &mut self,
    node: Option<ContentId>,
    level: usize,
    edits: Vec<Edit>,
  ) -> Result<(Option<Made>, u64), PathsRefused> {
    let Some(id) = node else {
      let entries = edits
        .into_iter()
        .filter_map(|edit| edit.slot.map(|slot| (edit.name, slot)))
        .collect();
      return Ok((self.put_set(entries, level)?, 0));
    };
    match self.read(id)? {
      Node::Leaf(entries) => {
        let old = cost_of(&entries);
        Ok((self.put_set(edited(entries, edits), level)?, old))
      }
      Node::Branch {
        bitmap,
        cost,
        children,
      } => {
        let children =
          children_by_digit(bitmap, &children).ok_or(PathsRefused::Malformed { node: id })?;
        let made = self.update_branch((id, level), (children, cost), edits)?;
        Ok((made, cost))
      }
    }
  }

  /// A branch `id` at `level`, its `children` by digit and its `cost`, under `edits`: each child an edit reaches made
  /// again; the whole one leaf if it now fits one, else a branch.
  fn update_branch(
    &mut self,
    (id, level): (ContentId, usize),
    (mut children, cost): (BTreeMap<u8, ContentId>, u64),
    edits: Vec<Edit>,
  ) -> Result<Option<Made>, PathsRefused> {
    let malformed = PathsRefused::Malformed { node: id };
    let mut by_digit: BTreeMap<u8, Vec<Edit>> = BTreeMap::new();
    for edit in edits {
      let digit = digit(&edit.place, level).ok_or(PathsRefused::Unsplittable)?;
      by_digit.entry(digit).or_default().push(edit);
    }
    let mut now = cost;
    for (digit, edits) in by_digit {
      let child = children.get(&digit).copied();
      let (made, old) = self.update(child, level.saturating_add(1), edits)?;
      now = now
        .checked_sub(old)
        .and_then(|left| left.checked_add(made.map_or(0, |made| made.cost)))
        .ok_or(malformed.clone())?;
      match made {
        Some(made) => children.insert(digit, made.root),
        None => children.remove(&digit),
      };
    }
    if children.is_empty() {
      return if now == 0 { Ok(None) } else { Err(malformed) };
    }
    if self.fits(now) {
      let entries = gather(&mut |node| self.read(node), children.into_values())?;
      return self.put_set(entries, level);
    }
    let bitmap = children
      .keys()
      .fold(0_u16, |bitmap, digit| bitmap | bit(*digit));
    let roots: Vec<ContentId> = children.into_values().collect();
    self.put_branch(bitmap, now, &roots).map(Some)
  }

  /// The empty map's root: a leaf of no entries.
  fn empty(&mut self) -> Result<ContentId, PathsRefused> {
    self.put_leaf(&Entries::new())
  }

  /// The map `change` makes of `parent`'s (module docs).
  fn apply(
    &mut self,
    parent: Option<ContentId>,
    change: &Change,
  ) -> Result<ContentId, PathsRefused> {
    if change.is_empty() {
      let Some(parent) = parent else {
        return self.empty();
      };
      self.read(parent)?;
      self.store.hold(&parent)?;
      self.owned.push(parent);
      return Ok(parent);
    }
    let mut reached = reach(change)?;
    self.look_up(&mut reached, parent)?;
    let root = self.make_again(reached)?;
    match root {
      Some(root) => Ok(root),
      None => self.empty(),
    }
  }

  /// Reads, from the root down, each reached directory's old trie and what each touched name held in it.
  fn look_up(
    &mut self,
    reached: &mut BTreeMap<Vec<u8>, Reached>,
    parent: Option<ContentId>,
  ) -> Result<(), PathsRefused> {
    let directories: Vec<Vec<u8>> = reached.keys().cloned().collect();
    for directory in directories {
      let old = old_trie(reached, &directory, parent);
      let Some(entry) = reached.get_mut(&directory) else {
        continue;
      };
      entry.old = old;
      if let Some(root) = old {
        for (name, touched) in &mut entry.names {
          touched.old = lookup(&mut |node| self.read(node), root, name)?;
        }
      }
    }
    Ok(())
  }

  /// Makes each reached directory again, the deepest first: the root's trie, `None` when the map is left empty.
  fn make_again(
    &mut self,
    mut reached: BTreeMap<Vec<u8>, Reached>,
  ) -> Result<Option<ContentId>, PathsRefused> {
    let mut root = None;
    while let Some((directory, entry)) = reached.pop_last() {
      let edits = edits_of(&directory, entry.names)?;
      let made = self.update(entry.old, 0, edits)?.0.map(|made| made.root);
      self.counters.directories = self.counters.directories.saturating_add(1);
      match parent_of(&directory) {
        None => root = made,
        Some((above, name)) => {
          if let Some(touched) = reached
            .get_mut(above)
            .and_then(|above| above.names.get_mut(name))
          {
            touched.below = made;
          }
        }
      }
    }
    Ok(root)
  }

  /// The map of `files` made from scratch: every directory made, the deepest first.
  fn build(&mut self, files: &BTreeMap<Vec<u8>, Entry>) -> Result<ContentId, PathsRefused> {
    let mut directories = Directories::new();
    directories.entry(Vec::new()).or_default();
    for (path, entry) in files {
      place_file(&mut directories, path, *entry)?;
    }
    let mut root = None;
    while let Some((directory, names)) = directories.pop_last() {
      let entries: Entries = names
        .into_iter()
        .filter_map(|(name, slot)| slot.map(|slot| (name, slot)))
        .collect();
      let made = self.put_set(entries, 0)?.map(|made| made.root);
      self.counters.directories = self.counters.directories.saturating_add(1);
      match parent_of(&directory) {
        None => root = made,
        Some((above, name)) => {
          if let Some(slot) = directories
            .get_mut(above)
            .and_then(|above| above.get_mut(name))
          {
            *slot = made.map(Slot::Directory);
          }
        }
      }
    }
    match root {
      Some(root) => Ok(root),
      None => self.empty(),
    }
  }
}

/// The trie that stood at `directory` in the parent's map (`parent`, the root's), read from what its own parent's
/// touched name held.
fn old_trie(
  reached: &BTreeMap<Vec<u8>, Reached>,
  directory: &[u8],
  parent: Option<ContentId>,
) -> Option<ContentId> {
  let Some((above, name)) = parent_of(directory) else {
    return parent;
  };
  match reached.get(above)?.names.get(name)?.old {
    Some(Slot::Directory(root)) => Some(root),
    _ => None,
  }
}

/// The directories the change reaches, each with the names in it the change touches: every directory on a changed
/// path, the root always.
fn reach(change: &Change) -> Result<BTreeMap<Vec<u8>, Reached>, PathsRefused> {
  let mut reached: BTreeMap<Vec<u8>, Reached> = BTreeMap::new();
  reached.entry(Vec::new()).or_default();
  for (path, file) in change {
    let names = split_path(path)?;
    let last = names.len().saturating_sub(1);
    let mut directory: Vec<u8> = Vec::new();
    for (at, name) in names.into_iter().enumerate() {
      let touched = reached
        .entry(directory.clone())
        .or_default()
        .names
        .entry(name.to_vec())
        .or_default();
      if at == last {
        touched.file = Some(*file);
      } else {
        touched.beneath = true;
      }
      directory = join(&directory, name);
    }
  }
  Ok(reached)
}

/// Places the file `entry` at `path` among `directories`, each directory on its path named in its parent (its slot
/// made once the directory is); a name that is a file and a directory at once is refused.
fn place_file(
  directories: &mut Directories,
  path: &[u8],
  entry: Entry,
) -> Result<(), PathsRefused> {
  let names = split_path(path)?;
  let last = names.len().saturating_sub(1);
  let mut directory: Vec<u8> = Vec::new();
  for (at, name) in names.into_iter().enumerate() {
    let slots = directories.entry(directory.clone()).or_default();
    let here = join(&directory, name);
    let wanted = (at == last).then_some(Slot::File(entry));
    match (slots.get(name), wanted) {
      (None, _) => {
        slots.insert(name.to_vec(), wanted);
      }
      (Some(None), None) => {}
      _ => return Err(PathsRefused::Shadowed { path: here }),
    }
    directory = here;
  }
  Ok(())
}
