//! A version's git trees as derived data (A-664; A-647; §4.17.2 "The repository in git's terms" item 5; §4.17.1 "A push
//! is a work" item 4).
//!
//! **The value.** One node a directory ([`slates_vcs::Form::Derived`]): it names the directory's tree object, held as
//! content in the shared store exactly as git hashes it after its header (so the git store's row for the same object
//! names the same content, A-614), then the nodes of its subtrees in the tree's order; its payload is the tree's id, the
//! object's length, its count of subtrees and how many levels of lists stand between it and them. A directory with more
//! subtrees than one node names holds them through list nodes, each naming as many as one node holds, a level of lists
//! above another until the directory's node names them all: the store's own rule for an object longer than a window
//! (A-581), so no directory is too wide. Every node is a function of the tree it describes and the store's window,
//! never of how it was made, which is what lets a value be kept under (type, version) with no coordination (A-647).
//!
//! **The tree of a directory** is git's: each file's mode from its class ([`FileClass`]: `100644`, `100755`, `120000`,
//! and `160000` for a submodule's commit, the special node), a subtree's `40000`; a blob's id hashed from the file's
//! bytes as `git hash-object` hashes them, a submodule link's id the commit id its content holds; the entries in git's
//! order ([`crate::objects::tree::compare`]: a subtree's name read as if it ended with `/`); written as git writes a
//! tree, and named by git's rule in the repository's format.
//!
//! **Incremental** ([`Trees::apply`], the type's derivation). A version's change is each path it sets or removes,
//! stated against its first parent's map (A-647's path map reads the same change). The derivation reads, from the root
//! down, only the directories on the change's paths, then makes them again from the deepest up: in each, the entries
//! the change did not touch keep their modes and ids from the parent's tree, and only the files it set are hashed. So
//! a derivation costs the change's directories and the files it set, never the version's tree
//! ([`TreeCounters::directories`] and [`TreeCounters::blobs`] count both).
//!
//! **From scratch** ([`Trees::build`]): the same value made from a version's files alone, every directory made and
//! every file hashed. The oracle holds every derivation to it.
//!
//! **Refusals** ([`TreesRefused`]): a path that is no path of a tree (an empty name, `.`, `..`, a NUL), a removal of a
//! path that holds no file, a path both a file and a directory, a class git has no mode for, a file whose content is
//! no file's bytes, a submodule link that holds no commit id, a blob SHA-1's detection finds to be part of a collision
//! attack, a tree git's strict fsck refuses, a node that does not read as this type's, a store whose window is not the
//! one declared, and the store's own. The change is judged whole: refused, the store is as it was.
//!
//! No I/O, no clock, no randomness, no panic.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::mem::size_of;
use std::num::NonZeroUsize;

use slates_vcs::content::{IDENTITY_BYTES, NAMES_COUNT_BYTES};
use slates_vcs::derived::paths::{Change, Entry, PathChanges};
use slates_vcs::{ContentId, ContentRefused, ContentStore, DerivedNode, DerivedType, Form};

use super::FileClass;
use crate::object_name::{Hasher, Header, Kind, ObjectFormat, ObjectId, name_of};
use crate::objects::Refusal;
use crate::objects::fsck::{FsckId, Strictness};
use crate::objects::tree::{self, Mode, Tree, compare};
use crate::{Admission, PastAdmission};

/// Format: the type's name and its format's version; the object format's name, a space and the store's window (eight
/// bytes, big-endian) follow, since a value's nodes depend on both.
const TYPE_NAME: &[u8] = b"slates-git trees 1 ";

/// Format: the first byte of a directory's node.
const DIRECTORY: u8 = b'T';

/// Format: the first byte of a list's node: the nodes of a directory's subtrees in its tree's order, or lists of them.
const LIST: u8 = b'L';

/// Format: the bytes of a node's tag.
const TAG_BYTES: usize = size_of::<u8>();

/// Format: the bytes of a directory node's tree object length, little-endian.
const LENGTH_BYTES: usize = size_of::<u64>();

/// Format: the bytes of a directory node's count of subtrees, little-endian.
const SUBTREES_BYTES: usize = size_of::<u32>();

/// Format: the bytes of a directory node's height: the levels of lists between it and its subtrees' nodes.
const HEIGHT_BYTES: usize = size_of::<u8>();

/// Shape: the fewest nodes a list names, or a level of lists would not be shorter than the level it names.
const LEAST_FANOUT: usize = 2;

/// Format: the byte between a path's names.
const SEPARATOR: u8 = b'/';

/// Why the trees refused (module docs). Each leaves the store as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreesRefused {
  /// The history holds no change for the version.
  NoChange,
  /// `path` is no path of a tree: empty, or with a name empty, `.`, `..`, or holding a NUL, which ends a name in a tree.
  BadPath {
    /// The path.
    path: Vec<u8>,
  },
  /// The change removes `path`, which holds no file in the parent's tree.
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
  /// The file at `path` has a class git has no mode for.
  UnknownClass {
    /// The path.
    path: Vec<u8>,
    /// The class.
    class: u8,
  },
  /// The file at `path` names a content that is no file's bytes: a derived node, or an index's table.
  NotAFile {
    /// The path.
    path: Vec<u8>,
  },
  /// The submodule link at `path` holds other than a commit id of the repository's format, its raw bytes.
  BadGitlink {
    /// The path.
    path: Vec<u8>,
  },
  /// The object made for `path` (the blob at it, or the tree of the directory at it, the root's path empty) is part of
  /// a SHA-1 collision attack, as git's detection refuses it ([`crate::store::COLLISION_WORDS`]).
  Collision {
    /// The path.
    path: Vec<u8>,
    /// The object's plain SHA-1.
    sha1: ObjectId,
  },
  /// The tree made for the directory at `path` (the root's path empty) fails git's strict fsck with `finding`: a tree
  /// no stock client that checks what it fetches would take.
  Fsck {
    /// The directory's path.
    path: Vec<u8>,
    /// The first finding strict fsck refuses.
    finding: FsckId,
  },
  /// The node `node` does not read as a node of this type's values.
  Malformed {
    /// The node.
    node: ContentId,
  },
  /// The window is smaller than a directory's node and a list of two.
  WindowTooSmall {
    /// The window asked for.
    window: usize,
  },
  /// The store's window is not the one the type was declared with, which its nodes depend on.
  WindowMismatch {
    /// The window declared.
    declared: usize,
    /// The store's.
    store: usize,
  },
  /// The store refused, or what a read or a check would keep is past the store's room.
  Content(ContentRefused),
}

impl From<ContentRefused> for TreesRefused {
  fn from(refused: ContentRefused) -> TreesRefused {
    TreesRefused::Content(refused)
  }
}

impl From<PastAdmission> for TreesRefused {
  fn from(past: PastAdmission) -> TreesRefused {
    TreesRefused::Content(ContentRefused::OverBudget {
      need: past.asked,
      room: past.left,
    })
  }
}

/// What the trees have done, for the tests' non-vacuity checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeCounters {
  /// Directories made again: in a derivation, the root and each directory on the change's paths, and no other; in a
  /// build, every directory.
  pub directories: u64,
  /// Blobs named by hashing their bytes: in a derivation, each file the change sets, once; in a build, every file.
  pub blobs: u64,
  /// Directory nodes read.
  pub read: u64,
  /// Nodes put, directories' and lists': stored new, or found held.
  pub nodes: u64,
  /// List nodes put: a directory's subtrees past what its node names.
  pub lists: u64,
}

impl TreeCounters {
  fn add(self, more: TreeCounters) -> TreeCounters {
    TreeCounters {
      directories: self.directories.saturating_add(more.directories),
      blobs: self.blobs.saturating_add(more.blobs),
      read: self.read.saturating_add(more.read),
      nodes: self.nodes.saturating_add(more.nodes),
      lists: self.lists.saturating_add(more.lists),
    }
  }
}

/// What a name in a directory holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
  /// A file: its mode and its id, a blob's (a submodule link's: the commit's).
  File {
    /// The mode.
    mode: Mode,
    /// The id.
    id: ObjectId,
  },
  /// A subtree: its tree's id and its directory's node in the value.
  Tree {
    /// The tree's id.
    id: ObjectId,
    /// The directory's node.
    node: ContentId,
  },
}

impl Item {
  /// The mode git writes for it.
  pub fn mode(&self) -> Mode {
    match self {
      Item::File { mode, .. } => *mode,
      Item::Tree { .. } => Mode::Directory,
    }
  }

  /// The object it names.
  pub fn id(&self) -> ObjectId {
    match self {
      Item::File { id, .. } | Item::Tree { id, .. } => *id,
    }
  }

  /// The directory's node, for a subtree.
  fn node(&self) -> Option<ContentId> {
    match self {
      Item::File { .. } => None,
      Item::Tree { node, .. } => Some(*node),
    }
  }
}

/// A directory's names, read or to be written, by name.
type Listing = BTreeMap<Vec<u8>, Item>;

/// A directory's node, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directory {
  /// The tree's id.
  pub id: ObjectId,
  /// The tree object's content: its bytes as git hashes them after the header.
  pub object: ContentId,
  /// The tree object's length in bytes.
  pub len: u64,
  /// Its subtrees' directory nodes, in the tree's order.
  pub subtrees: Vec<ContentId>,
}

/// The trees, declared over a history `S` (module docs).
#[derive(Debug)]
pub struct Trees<S> {
  format: ObjectFormat,
  name: Vec<u8>,
  mode: slates_vcs::Mode,
  window: usize,
  /// The nodes a directory's node names beside its object: its subtrees', or the lists above them.
  room: NonZeroUsize,
  /// The nodes a list's node names.
  fanout: NonZeroUsize,
  counters: TreeCounters,
  history: PhantomData<fn(&S)>,
}

impl<S> Trees<S> {
  /// The trees of a repository of `format`, over a store of `window`, computed by `mode`. Refused for a window that holds
  /// no directory's node naming one subtree, or no list of two.
  pub fn declare(
    format: ObjectFormat,
    window: usize,
    mode: slates_vcs::Mode,
  ) -> Result<Trees<S>, TreesRefused> {
    let too_small = TreesRefused::WindowTooSmall { window };
    let wide = u64::try_from(window).map_err(|_| too_small.clone())?;
    let room = names_within(window, directory_payload(format))
      .checked_sub(1)
      .and_then(NonZeroUsize::new)
      .ok_or_else(|| too_small.clone())?;
    let fanout = NonZeroUsize::new(names_within(window, TAG_BYTES))
      .filter(|fanout| fanout.get() >= LEAST_FANOUT)
      .ok_or(too_small)?;
    let mut name = TYPE_NAME.to_vec();
    name.extend_from_slice(format.name().as_bytes());
    name.push(b' ');
    name.extend_from_slice(&wide.to_be_bytes());
    Ok(Trees {
      format,
      name,
      mode,
      window,
      room,
      fanout,
      counters: TreeCounters::default(),
      history: PhantomData,
    })
  }

  /// The repository's format.
  pub fn format(&self) -> ObjectFormat {
    self.format
  }

  /// What the trees have done.
  pub fn counters(&self) -> TreeCounters {
    self.counters
  }

  /// The value of `files` made from scratch, every directory made and every file hashed: its root, held once by the
  /// caller.
  pub fn build(
    &mut self,
    store: &mut ContentStore,
    files: &BTreeMap<Vec<u8>, Entry>,
  ) -> Result<ContentId, TreesRefused> {
    self.same_window(store)?;
    let mut work = self.work(store);
    let made = work.build(files);
    self.settle(work, made)
  }

  /// The value `change` makes of `parent`'s (`None`: no files), made incrementally (module docs): its root, held once by
  /// the caller.
  pub fn apply(
    &mut self,
    store: &mut ContentStore,
    parent: Option<ContentId>,
    change: &Change,
  ) -> Result<ContentId, TreesRefused> {
    self.same_window(store)?;
    let mut work = self.work(store);
    let made = work.apply(parent, change);
    self.settle(work, made)
  }

  /// The id of the tree at the root of the value `root`: the version's tree.
  pub fn id(&self, store: &ContentStore, root: ContentId) -> Result<ObjectId, TreesRefused> {
    read_directory(store, self.format, root).map(|directory| directory.id)
  }

  /// The directory `node`: its tree's id, its object and length, and its subtrees' nodes.
  pub fn directory(
    &self,
    store: &ContentStore,
    node: ContentId,
  ) -> Result<Directory, TreesRefused> {
    read_directory(store, self.format, node)
  }

  /// The names of the directory `node`, each with what it holds.
  pub fn names(
    &self,
    store: &ContentStore,
    node: ContentId,
  ) -> Result<BTreeMap<Vec<u8>, Item>, TreesRefused> {
    let directory = read_directory(store, self.format, node)?;
    listing(store, self.format, node, &directory)
  }

  /// What `path` names in the value `root`: one directory read a name, no walk of the tree.
  pub fn get(
    &self,
    store: &ContentStore,
    root: ContentId,
    path: &[u8],
  ) -> Result<Option<Item>, TreesRefused> {
    let names = split_path(path)?;
    let last = names.len().saturating_sub(1);
    let mut node = root;
    for (at, name) in names.into_iter().enumerate() {
      let Some(item) = self.names(store, node)?.get(name).copied() else {
        return Ok(None);
      };
      match item {
        _ if at == last => return Ok(Some(item)),
        Item::Tree { node: below, .. } => node = below,
        Item::File { .. } => return Ok(None),
      }
    }
    Ok(None)
  }

  /// Refuses a store whose window is not the one declared.
  fn same_window(&self, store: &ContentStore) -> Result<(), TreesRefused> {
    if store.window() == self.window {
      Ok(())
    } else {
      Err(TreesRefused::WindowMismatch {
        declared: self.window,
        store: store.window(),
      })
    }
  }

  /// A work over `store`.
  fn work<'s>(&self, store: &'s mut ContentStore) -> Work<'s> {
    Work {
      store,
      format: self.format,
      room: self.room,
      fanout: self.fanout,
      owned: Vec::new(),
      counters: TreeCounters::default(),
    }
  }

  /// Ends a work: one hold kept on its root for the caller, every other hold its puts took given back; refused, all of
  /// them. Past a refusal the rest is given back still, and the first refusal is the answer.
  fn settle(
    &mut self,
    work: Work<'_>,
    made: Result<ContentId, TreesRefused>,
  ) -> Result<ContentId, TreesRefused> {
    let Work {
      store,
      owned,
      counters,
      ..
    } = work;
    self.counters = self.counters.add(counters);
    let mut kept = false;
    let mut released: Result<(), TreesRefused> = Ok(());
    for node in owned {
      if !kept && made.as_ref().is_ok_and(|root| *root == node) {
        kept = true;
        continue;
      }
      if let Err(refused) = store.release(&node) {
        released = released.and(Err(TreesRefused::Content(refused)));
      }
    }
    let root = made?;
    released?;
    Ok(root)
  }
}

impl<S: PathChanges> DerivedType for Trees<S> {
  type Source = S;
  type Refused = TreesRefused;

  fn name(&self) -> &[u8] {
    &self.name
  }

  fn mode(&self) -> slates_vcs::Mode {
    self.mode
  }

  fn derive(
    &mut self,
    source: &S,
    store: &mut ContentStore,
    version: S::Version,
    parents: &[ContentId],
  ) -> Result<ContentId, TreesRefused> {
    let change = source.change(version).ok_or(TreesRefused::NoChange)?;
    self.apply(store, parents.first().copied(), change)
  }
}

/// What a directory's node takes beside its names in `format`: its tag, its tree's id, the object's length, its count
/// of subtrees and its height.
fn directory_payload(format: ObjectFormat) -> usize {
  TAG_BYTES
    .saturating_add(format.raw_len())
    .saturating_add(LENGTH_BYTES)
    .saturating_add(SUBTREES_BYTES)
    .saturating_add(HEIGHT_BYTES)
}

/// The names a node whose payload is `payload` bytes holds at most in `window`.
fn names_within(window: usize, payload: usize) -> usize {
  window
    .saturating_sub(NAMES_COUNT_BYTES)
    .saturating_sub(payload)
    .checked_div(IDENTITY_BYTES)
    .unwrap_or(0)
}

/// The head of a directory's node: its tree's id, the object's length, its count of subtrees and its height.
struct Head {
  id: ObjectId,
  len: u64,
  subtrees: usize,
  height: u8,
}

/// The head `node`'s payload holds, if it is a directory's node of `format`.
fn head(format: ObjectFormat, node: &DerivedNode<'_>) -> Option<Head> {
  let (tag, rest) = node.payload().split_first()?;
  if *tag != DIRECTORY {
    return None;
  }
  let (id, rest) = rest.split_at_checked(format.raw_len())?;
  let (len, rest) = rest.split_first_chunk::<LENGTH_BYTES>()?;
  let (subtrees, rest) = rest.split_first_chunk::<SUBTREES_BYTES>()?;
  let (height, rest) = rest.split_first()?;
  if !rest.is_empty() {
    return None;
  }
  Some(Head {
    id: ObjectId::from_bytes(format, id)?,
    len: u64::from_le_bytes(*len),
    subtrees: usize::try_from(u32::from_le_bytes(*subtrees)).ok()?,
    height: *height,
  })
}

/// The tree's id the directory node `node` holds, if it is one of `format`: what a commit made of the value names
/// ([`super::commit`]).
pub(super) fn tree_id(
  store: &ContentStore,
  format: ObjectFormat,
  node: ContentId,
) -> Option<ObjectId> {
  head(format, &store.derived(&node)?).map(|head| head.id)
}

/// The directory node `node`, read: its head, and its subtrees' nodes through its lists.
fn read_directory(
  store: &ContentStore,
  format: ObjectFormat,
  node: ContentId,
) -> Result<Directory, TreesRefused> {
  let malformed = TreesRefused::Malformed { node };
  let derived = store.derived(&node).ok_or_else(|| malformed.clone())?;
  let head = head(format, &derived).ok_or_else(|| malformed.clone())?;
  let object = derived.name(0).ok_or_else(|| malformed.clone())?;
  let items = derived.names().skip(1).collect();
  let subtrees = expand(store, items, head.height, head.subtrees).ok_or(malformed)?;
  Ok(Directory {
    id: head.id,
    object,
    len: head.len,
    subtrees,
  })
}

/// The subtrees' nodes `items` name through `height` levels of lists: exactly `count` of them, or `None`.
fn expand(
  store: &ContentStore,
  mut items: Vec<ContentId>,
  height: u8,
  count: usize,
) -> Option<Vec<ContentId>> {
  for _ in 0..height {
    let mut below = Vec::new();
    for list in &items {
      let node = store.derived(list)?;
      if node.payload() != [LIST].as_slice() || below.len().saturating_add(node.len()) > count {
        return None;
      }
      below.extend(node.names());
    }
    items = below;
  }
  (items.len() == count).then_some(items)
}

/// The names of `directory`, the node `node`, read from its tree object: each file's mode and id, each subtree's id and
/// node.
fn listing(
  store: &ContentStore,
  format: ObjectFormat,
  node: ContentId,
  directory: &Directory,
) -> Result<Listing, TreesRefused> {
  let malformed = || TreesRefused::Malformed { node };
  let bytes = store
    .object_bytes(&directory.object, directory.len)
    .ok_or_else(malformed)?;
  // The entries borrow the object's bytes; what they keep beside them is within the store's room.
  let tree =
    Tree::parse(&bytes, format, &mut Admission::new(store.room())).map_err(
      |refused| match refused.refusal {
        Refusal::PastAdmission(past) => TreesRefused::from(past),
        _ => malformed(),
      },
    )?;
  let mut subtrees = directory.subtrees.iter();
  let mut names = Listing::new();
  for entry in tree.entries() {
    let id = entry.object_id(format).ok_or_else(malformed)?;
    let item = if entry.is_dir() {
      let node = *subtrees.next().ok_or_else(malformed)?;
      Item::Tree { id, node }
    } else {
      let mode = entry.standard_mode().ok_or_else(malformed)?;
      Item::File { mode, id }
    };
    if names.insert(entry.name.to_vec(), item).is_some() {
      return Err(malformed());
    }
  }
  match subtrees.next() {
    Some(_) => Err(malformed()),
    None => Ok(names),
  }
}

/// Whether `name` can be a name in a tree: not empty, not `.` or `..`, holding no separator and no NUL.
fn valid_name(name: &[u8]) -> bool {
  !name.is_empty()
    && name != b"."
    && name != b".."
    && !name.contains(&SEPARATOR)
    && !name.contains(&0)
}

/// `path`'s names, or the refusal of a path that is no path of a tree.
fn split_path(path: &[u8]) -> Result<Vec<&[u8]>, TreesRefused> {
  let names: Vec<&[u8]> = path.split(|byte| *byte == SEPARATOR).collect();
  if names.iter().all(|name| valid_name(name)) {
    Ok(names)
  } else {
    Err(TreesRefused::BadPath {
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

/// A directory made: its node, and its tree's id.
#[derive(Clone, Copy, Debug)]
struct Made {
  node: ContentId,
  id: ObjectId,
}

impl Made {
  /// What its name holds in the directory above.
  fn item(self) -> Item {
    Item::Tree {
      id: self.id,
      node: self.node,
    }
  }
}

/// A directory the change reaches.
#[derive(Debug, Default)]
struct Reached {
  /// Its names in the parent's tree: `None` when no tree stood at its path.
  old: Option<Listing>,
  /// The names in it the change touches.
  names: BTreeMap<Vec<u8>, Touched>,
}

/// A name the change touches.
#[derive(Debug, Default)]
struct Touched {
  /// What it held in the parent's tree.
  old: Option<Item>,
  /// The change of the file at it: set (`Some(Some(..))`) or removed (`Some(None)`).
  file: Option<Option<Entry>>,
  /// Whether the change reaches beneath it.
  beneath: bool,
  /// The directory beneath it after the change, once made: `None` when nothing stands beneath.
  below: Option<Made>,
}

/// What a touched name holds after the change, before a file it sets is named.
enum Decision {
  /// What it holds, decided.
  Keep(Option<Item>),
  /// The file the change sets at it.
  Set(Entry),
}

/// What is wrong with a touched name, before its path is known.
enum Wrong {
  RemovedAbsent,
  Shadowed,
}

/// What a touched name holds after the change (the path map's rule, A-647, so the two types refuse the same changes).
fn decide(touched: &Touched) -> Result<Decision, Wrong> {
  let below = touched.beneath.then_some(touched.below);
  let old_file = matches!(touched.old, Some(Item::File { .. }));
  let old_tree = matches!(touched.old, Some(Item::Tree { .. }));
  match (touched.file, below) {
    (Some(None), _) if !old_file => Err(Wrong::RemovedAbsent),
    (Some(Some(_)), Some(Some(_))) => Err(Wrong::Shadowed),
    (None, Some(Some(_))) if old_file => Err(Wrong::Shadowed),
    (Some(None) | None, Some(Some(made))) => Ok(Decision::Keep(Some(made.item()))),
    (Some(Some(_)), None) if old_tree => Err(Wrong::Shadowed),
    (Some(Some(entry)), _) => Ok(Decision::Set(entry)),
    (Some(None), _) => Ok(Decision::Keep(None)),
    (None, Some(None)) => Ok(Decision::Keep(
      touched.old.filter(|item| matches!(item, Item::File { .. })),
    )),
    (None, None) => Ok(Decision::Keep(touched.old)),
  }
}

/// One build's or derivation's work: the store, the format and the shape, the holds its puts took (given back when it
/// settles), and what it counted.
struct Work<'s> {
  store: &'s mut ContentStore,
  format: ObjectFormat,
  room: NonZeroUsize,
  fanout: NonZeroUsize,
  owned: Vec<ContentId>,
  counters: TreeCounters,
}

impl Work<'_> {
  /// Reads the directory `node` and its names, counted.
  fn read(&mut self, node: ContentId) -> Result<(Directory, Listing), TreesRefused> {
    self.counters.read = self.counters.read.saturating_add(1);
    let directory = read_directory(self.store, self.format, node)?;
    let names = listing(self.store, self.format, node, &directory)?;
    Ok((directory, names))
  }

  /// Puts a node, its hold owned by this work.
  fn put(&mut self, names: &[ContentId], payload: &[u8]) -> Result<ContentId, TreesRefused> {
    let stored = self.store.put_derived(names, payload)?;
    self.owned.push(stored.identity);
    self.counters.nodes = self.counters.nodes.saturating_add(1);
    Ok(stored.identity)
  }

  /// The id of the file `entry` at `path`, and its mode, from its class.
  fn file(&mut self, path: &[u8], entry: Entry) -> Result<Item, TreesRefused> {
    let class = FileClass::from_class(entry.class).ok_or_else(|| TreesRefused::UnknownClass {
      path: path.to_vec(),
      class: entry.class,
    })?;
    let id = match class {
      FileClass::Gitlink => self.gitlink(path, entry.content)?,
      FileClass::File | FileClass::Executable | FileClass::Symlink => {
        self.blob(path, entry.content)?
      }
    };
    Ok(Item::File {
      mode: class.mode(),
      id,
    })
  }

  /// The blob id of `content`'s bytes, read a window at a time, as `git hash-object` names them.
  fn blob(&mut self, path: &[u8], content: ContentId) -> Result<ObjectId, TreesRefused> {
    match self.store.form(&content) {
      Some(Form::Window | Form::List) => {}
      Some(Form::Table | Form::Derived) => {
        return Err(TreesRefused::NotAFile {
          path: path.to_vec(),
        });
      }
      None => return Err(TreesRefused::Content(ContentRefused::NotHeld)),
    }
    let size = self.store.windows(&content).fold(0_u64, |size, window| {
      size.saturating_add(crate::to_u64(window.len()))
    });
    let mut hasher = Hasher::object(
      self.format,
      Header {
        kind: Kind::Blob,
        size,
      },
    );
    for window in self.store.windows(&content) {
      hasher.update(&window);
    }
    self.counters.blobs = self.counters.blobs.saturating_add(1);
    hasher.finish().map_err(|attack| TreesRefused::Collision {
      path: path.to_vec(),
      sha1: attack.sha1,
    })
  }

  /// The commit id a submodule link's content holds: the format's length of raw bytes, in one window.
  fn gitlink(&self, path: &[u8], content: ContentId) -> Result<ObjectId, TreesRefused> {
    let bad = || TreesRefused::BadGitlink {
      path: path.to_vec(),
    };
    match self.store.form(&content) {
      Some(Form::Window) => {}
      Some(_) => return Err(bad()),
      None => return Err(TreesRefused::Content(ContentRefused::NotHeld)),
    }
    let bytes = self.store.get(&content).ok_or_else(bad)?;
    ObjectId::from_bytes(self.format, bytes).ok_or_else(bad)
  }

  /// The directory at `path` of `names` made: its tree written in git's order, checked by strict fsck and named, the
  /// object kept, its subtrees' nodes named through lists where one node cannot name them all.
  fn make(&mut self, path: &[u8], names: &Listing) -> Result<Made, TreesRefused> {
    let (bytes, subtrees) = write_tree(names);
    self.check(path, &bytes)?;
    let id =
      name_of(self.format, Kind::Tree, &bytes).map_err(|attack| TreesRefused::Collision {
        path: path.to_vec(),
        sha1: attack.sha1,
      })?;
    let count = u32::try_from(subtrees.len()).map_err(|_| ContentRefused::CountExhausted)?;
    let len = u64::try_from(bytes.len()).map_err(|_| ContentRefused::CountExhausted)?;
    let object = self.store.put_object(&bytes)?;
    self.owned.push(object);
    let (height, items) = self.lists(subtrees)?;
    let mut names = Vec::with_capacity(items.len().saturating_add(1));
    names.push(object);
    names.extend(items);
    let mut payload = Vec::with_capacity(directory_payload(self.format));
    payload.push(DIRECTORY);
    payload.extend_from_slice(id.as_bytes());
    payload.extend_from_slice(&len.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    payload.push(height);
    let node = self.put(&names, &payload)?;
    Ok(Made { node, id })
  }

  /// Refuses the tree `bytes` of the directory at `path` when git's strict fsck refuses it.
  fn check(&self, path: &[u8], bytes: &[u8]) -> Result<(), TreesRefused> {
    let checked = tree::fsck(
      bytes,
      self.format,
      Strictness::Strict,
      &mut Admission::new(self.store.room()),
    )?;
    match checked.report.refusal() {
      Some(finding) => Err(TreesRefused::Fsck {
        path: path.to_vec(),
        finding,
      }),
      None => Ok(()),
    }
  }

  /// The names a directory's node gives its subtrees: their nodes while it can name them all, else lists of them, a
  /// level above another until it can; and the levels of lists.
  fn lists(&mut self, subtrees: Vec<ContentId>) -> Result<(u8, Vec<ContentId>), TreesRefused> {
    let mut items = subtrees;
    let mut height = 0_u8;
    while items.len() > self.room.get() {
      let mut lists = Vec::with_capacity(items.len().div_ceil(self.fanout.get()));
      for chunk in items.chunks(self.fanout.get()) {
        lists.push(self.put(chunk, &[LIST])?);
        self.counters.lists = self.counters.lists.saturating_add(1);
      }
      items = lists;
      height = height
        .checked_add(1)
        .ok_or(ContentRefused::CountExhausted)?;
    }
    Ok((height, items))
  }

  /// The tree of no files: the root of an empty version.
  fn empty(&mut self) -> Result<ContentId, TreesRefused> {
    self.make(&[], &Listing::new()).map(|made| made.node)
  }

  /// The value `change` makes of `parent`'s (module docs).
  fn apply(
    &mut self,
    parent: Option<ContentId>,
    change: &Change,
  ) -> Result<ContentId, TreesRefused> {
    if change.is_empty() {
      let Some(parent) = parent else {
        return self.empty();
      };
      read_directory(self.store, self.format, parent)?;
      self.store.hold(&parent)?;
      self.owned.push(parent);
      return Ok(parent);
    }
    let mut reached = reach(change)?;
    self.look_up(&mut reached, parent)?;
    match self.make_again(reached)? {
      Some(root) => Ok(root.node),
      None => self.empty(),
    }
  }

  /// Reads, from the root down, each reached directory's names in the parent's tree and what each touched name held.
  fn look_up(
    &mut self,
    reached: &mut BTreeMap<Vec<u8>, Reached>,
    parent: Option<ContentId>,
  ) -> Result<(), TreesRefused> {
    let directories: Vec<Vec<u8>> = reached.keys().cloned().collect();
    for directory in directories {
      let Some((node, claimed)) = old_tree(reached, &directory, parent) else {
        continue;
      };
      let (read, names) = self.read(node)?;
      if claimed.is_some_and(|id| id != read.id) {
        return Err(TreesRefused::Malformed { node });
      }
      if let Some(entry) = reached.get_mut(&directory) {
        for (name, touched) in &mut entry.names {
          touched.old = names.get(name).copied();
        }
        entry.old = Some(names);
      }
    }
    Ok(())
  }

  /// Makes each reached directory again, the deepest first: the root's, which always stands.
  fn make_again(
    &mut self,
    mut reached: BTreeMap<Vec<u8>, Reached>,
  ) -> Result<Option<Made>, TreesRefused> {
    let mut root = None;
    while let Some((directory, entry)) = reached.pop_last() {
      let made = self.made_again(&directory, entry)?;
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

  /// The directory at `directory` made again under the change: `None` when no name is left in it, unless it is the
  /// root, which is then the empty tree.
  fn made_again(&mut self, directory: &[u8], entry: Reached) -> Result<Option<Made>, TreesRefused> {
    let mut names = entry.old.unwrap_or_default();
    for (name, touched) in entry.names {
      let path = join(directory, &name);
      let item = match decide(&touched) {
        Ok(Decision::Keep(item)) => item,
        Ok(Decision::Set(file)) => Some(self.file(&path, file)?),
        Err(Wrong::RemovedAbsent) => return Err(TreesRefused::RemovedAbsent { path }),
        Err(Wrong::Shadowed) => return Err(TreesRefused::Shadowed { path }),
      };
      match item {
        Some(item) => names.insert(name, item),
        None => names.remove(&name),
      };
    }
    if names.is_empty() && !directory.is_empty() {
      return Ok(None);
    }
    self.make(directory, &names).map(Some)
  }

  /// The value of `files` made from scratch: every file hashed, every directory made, the deepest first.
  fn build(&mut self, files: &BTreeMap<Vec<u8>, Entry>) -> Result<ContentId, TreesRefused> {
    let mut directories = Directories::new();
    directories.entry(Vec::new()).or_default();
    for (path, entry) in files {
      split_path(path)?;
      let item = self.file(path, *entry)?;
      place(&mut directories, path, item)?;
    }
    let mut root = None;
    while let Some((directory, names)) = directories.pop_last() {
      let names: Listing = names
        .into_iter()
        .filter_map(|(name, item)| item.map(|item| (name, item)))
        .collect();
      let made = self.make(&directory, &names)?;
      self.counters.directories = self.counters.directories.saturating_add(1);
      match parent_of(&directory) {
        None => root = Some(made),
        Some((above, name)) => {
          if let Some(slot) = directories
            .get_mut(above)
            .and_then(|above| above.get_mut(name))
          {
            *slot = Some(made.item());
          }
        }
      }
    }
    match root {
      Some(root) => Ok(root.node),
      None => self.empty(),
    }
  }
}

/// A tree's bytes as git writes them, its entries in git's order, and its subtrees' nodes in that order.
fn write_tree(names: &Listing) -> (Vec<u8>, Vec<ContentId>) {
  let mut ordered: Vec<(tree::Entry<'_>, Option<ContentId>)> = names
    .iter()
    .map(|(name, item)| {
      let entry = tree::Entry::new(item.mode(), name, item_id_bytes(item));
      (entry, item.node())
    })
    .collect();
  ordered.sort_by(|a, b| compare(&a.0, &b.0));
  let mut bytes = Vec::new();
  for (entry, _) in &ordered {
    entry.write(&mut bytes);
  }
  let subtrees = ordered.iter().filter_map(|(_, node)| *node).collect();
  (bytes, subtrees)
}

/// The raw bytes of the id an item names, borrowed from it.
fn item_id_bytes(item: &Item) -> &[u8] {
  match item {
    Item::File { id, .. } | Item::Tree { id, .. } => id.as_bytes(),
  }
}

/// The tree that stood at `directory` in the parent's value (`parent`, the root's), with the id its own parent's tree
/// named it by: read from what its own parent's touched name held.
fn old_tree(
  reached: &BTreeMap<Vec<u8>, Reached>,
  directory: &[u8],
  parent: Option<ContentId>,
) -> Option<(ContentId, Option<ObjectId>)> {
  let Some((above, name)) = parent_of(directory) else {
    return parent.map(|node| (node, None));
  };
  match reached.get(above)?.names.get(name)?.old {
    Some(Item::Tree { id, node }) => Some((node, Some(id))),
    _ => None,
  }
}

/// The directories the change reaches, each with the names in it the change touches: every directory on a changed
/// path, the root always.
fn reach(change: &Change) -> Result<BTreeMap<Vec<u8>, Reached>, TreesRefused> {
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

/// A build's directories by path, each name in one with what it holds: a file, or a subtree once it is made (`None`
/// until then).
type Directories = BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Option<Item>>>;

/// Places the file `item` at `path` among `directories`, each directory on its path named in its parent (its item made
/// once the directory is); a name that is a file and a directory at once is refused.
fn place(directories: &mut Directories, path: &[u8], item: Item) -> Result<(), TreesRefused> {
  let names = split_path(path)?;
  let last = names.len().saturating_sub(1);
  let mut directory: Vec<u8> = Vec::new();
  for (at, name) in names.into_iter().enumerate() {
    let slots = directories.entry(directory.clone()).or_default();
    let here = join(&directory, name);
    let wanted = (at == last).then_some(item);
    match (slots.get(name), wanted) {
      (None, _) => {
        slots.insert(name.to_vec(), wanted);
      }
      (Some(None), None) => {}
      _ => return Err(TreesRefused::Shadowed { path: here }),
    }
    directory = here;
  }
  Ok(())
}
