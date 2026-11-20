//! A green's current state as persistent maps (§4.16 "A green made at another green's version"; §4.17.1 shared piece
//! 1's second half; A-534): each dimension of the state (files, IPC nodes, directories, modes, symbolic links, hard
//! links, extended attributes, and the reverse map of hard links) is a persistent radix tree ([`crate::ptrie`]) keyed by
//! the path's bytes, whose nodes live in an arena of their own. A green made at another green's version forks every
//! root (one count each) and shares every node until one of the two changes a key, which copies only that key's path
//! (path copying, Driscoll, Sarnak, Sleator and Tarjan, "Making data structures persistent", JCSS 1989, tier A).
//!
//! **Why a radix tree.** A green's maps were ordered maps keyed by the path string, so making a green at another's
//! version copied them: O(the tree) for every branch, which is what §4.17.1 item 4 forbids (a branch of the kernel tree,
//! 97,229 files, would copy each one's name and bytes). A persistent balanced tree keyed by the whole path would make the
//! copy O(1) too, but Subversion's copy (§4.17.4, g292) re-roots a whole subtree under another path, and a tree keyed by
//! whole paths must re-key every entry for it, O(the subtree). A radix tree's keys are its edges: a prefix is one node,
//! so a subtree grafts under another prefix in O(its depth) ([`crate::ptrie::Ptrie::graft`]). Its byte order is the
//! string order, so every walk yields keys in the order the ordered maps did, which every encoding relies on.
//!
//! **What a key costs to read.** A walk assembles each key from its edges, so readers yield owned strings, never a
//! borrow of a stored key: a stored whole key would be wrong after a graft.
//!
//! **Files hold blocks.** A file's bytes are a block of [`Blocks`], counted by the map nodes naming it, so copying a node
//! on a path costs a count, never the file's bytes; a block is freed with its last holder.
//!
//! **Attributes keep their order.** An attribute's key is its path with every NUL escaped ([`NUL_ESCAPED`]), then
//! [`PATH_END`], then its name: escaping keeps byte order, and the path's end sorts before any continuation of it, so a
//! walk yields `(path, name)` in the order of the ordered map of pairs it replaces, and one path's attributes are one
//! prefix.
//!
//! **Arenas are lent.** A green that stands alone owns its arenas. The greens of a [`crate::engine::Lineage`] share the
//! lineage's: the lineage lends them to a green for the length of one call ([`Arenas`]), so there is one owner of every
//! node and no shared-ownership pointer (CLAUDE.md R2).
//!
//! **A tree is copied by reference (A-542, Copy's maps).** A path's tree is its own key and the keys that start with it
//! and `/`. [`Dim::copy_tree`] holds the source's keys beneath it as one subtree, grafts them beneath the destination
//! ([`crate::ptrie::Ptrie::graft`]) and sets the destination's own key to the source's value: two descents, two counts
//! and the copies of the nodes on the destination's path, O(the paths' lengths) whatever the tree holds, and no walk
//! ([`crate::ptrie::Nodes::visited`] does not move). A file's copy is one more holder of its block, never its bytes.
//! An attribute's tree is two prefixes, the path's own attributes and its descendants' ([`Xattrs::copy_tree`]). The
//! destination must be absent with nothing beneath it, as Subversion's copy needs (§4.17.4 item 3.2), and the subtree
//! is held before anything is grafted, so a copy into the source's own subtree sees the source as it was. The root's
//! tree is every key (A-586: a stock client copies `^/` into a child of itself): the whole map is held and grafted
//! beneath the destination, and the root's own key, which came with it, is moved to the destination's own key (for
//! attributes, its own prefix is cut away from beneath the destination, [`crate::ptrie::Ptrie::cut`]). The root is never
//! a destination: it is always present. A tree is taken away again by [`Dim::cut_tree`], a copy's inverse, which a green
//! made at a version before a copy runs.

use std::collections::BTreeSet;

use crate::ptrie::{Holders, Nodes, Plain, Ptrie, Releasing};
use crate::special::SpecialNode;

mod copy;

pub(crate) use copy::{Held, HeldAttributes};

/// A path's key bytes as the string they were made from. Keys enter only as strings and a walk reassembles one whole,
/// so the lossy arm never runs; it keeps the conversion total, as the no-panic rule asks.
fn text(key: Vec<u8>) -> String {
  String::from_utf8(key)
    .unwrap_or_else(|refused| String::from_utf8_lossy(refused.as_bytes()).into_owned())
}

/// One dimension's map: its root, and the arena its nodes live in (module docs, "Arenas are lent").
#[derive(Debug)]
pub(crate) struct Dim<V> {
  root: Ptrie<V>,
  nodes: Nodes<V>,
}

// By hand: a derived `Default` would ask it of the values, which an empty map holds none of.
impl<V> Default for Dim<V> {
  fn default() -> Self {
    Dim {
      root: Ptrie::default(),
      nodes: Nodes::default(),
    }
  }
}

impl<V: Clone> Dim<V> {
  /// The value under `key`.
  pub(crate) fn get(&self, key: impl AsRef<[u8]>) -> Option<&V> {
    self.root.get(&self.nodes, key.as_ref())
  }

  /// Whether the map holds `key`.
  pub(crate) fn contains_key(&self, key: impl AsRef<[u8]>) -> bool {
    self.root.contains_key(&self.nodes, key.as_ref())
  }

  /// Sets `key` to `value`; the value it replaced.
  pub(crate) fn insert(&mut self, key: impl AsRef<[u8]>, value: V) -> Option<V> {
    self
      .root
      .insert(&mut self.nodes, key.as_ref(), value, &mut Plain)
  }

  /// Takes `key` out; its value.
  pub(crate) fn remove(&mut self, key: impl AsRef<[u8]>) -> Option<V> {
    self.root.remove(&mut self.nodes, key.as_ref(), &mut Plain)
  }

  /// Every key and value, in key order.
  pub(crate) fn iter(&self) -> impl Iterator<Item = (String, &V)> + '_ {
    self
      .root
      .iter(&self.nodes)
      .map(|(key, value)| (text(key), value))
  }

  /// Every key, in order.
  pub(crate) fn keys(&self) -> impl Iterator<Item = String> + '_ {
    self.iter().map(|(key, _)| key)
  }

  /// Every value, in key order.
  pub(crate) fn values(&self) -> impl Iterator<Item = &V> + '_ {
    self.root.iter(&self.nodes).map(|(_, value)| value)
  }

  /// The keys that start with `prefix` and their values, in key order: one descent, then the keys it answers.
  pub(crate) fn prefix(&self, prefix: &str) -> impl Iterator<Item = (String, &V)> + '_ {
    self
      .root
      .prefix(&self.nodes, prefix.as_bytes())
      .map(|(key, value)| (text(key), value))
  }

  /// The values the map holds, without a walk.
  pub(crate) fn len(&self) -> usize {
    self.root.len(&self.nodes)
  }

  /// A map holding exactly this one's values, sharing every node: its root counted once more here, its arena left
  /// here (module docs, "Arenas are lent").
  pub(crate) fn fork(&mut self) -> Dim<V> {
    Dim {
      root: self.root.fork(&mut self.nodes),
      nodes: Nodes::default(),
    }
  }

  /// Exchanges this map's arena with `nodes`.
  pub(crate) fn swap_arena(&mut self, nodes: &mut Nodes<V>) {
    std::mem::swap(&mut self.nodes, nodes);
  }

  /// Exchanges this map's arena with `other`'s.
  pub(crate) fn lend(&mut self, other: &mut Dim<V>) {
    std::mem::swap(&mut self.nodes, &mut other.nodes);
  }

  /// This map's hold on its nodes, to give back a node a step in its arena ([`Arenas::release_step`]); the map is
  /// empty after, and no node is freed until the steps run.
  pub(crate) fn releasing(&mut self) -> Releasing {
    self.root.releasing()
  }

  /// The nodes walks of this map's arena visited, while it holds its arena ([`Nodes::visited`]).
  pub(crate) fn visited(&self) -> u64 {
    self.nodes.visited()
  }
}

/// A file's bytes: its place in [`Blocks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Block(usize);

/// The bytes of current files, each block with the count of the map nodes naming it (module docs, "Files hold
/// blocks").
#[derive(Debug, Default)]
pub(crate) struct Blocks {
  /// Each block's bytes and holders; `None` once freed.
  slots: Vec<Option<(Vec<u8>, u64)>>,
  /// Freed slots, reused before the arena grows.
  free: Vec<usize>,
}

impl Blocks {
  /// A new block holding `bytes`, with one holder.
  fn add(&mut self, bytes: Vec<u8>) -> Block {
    if let Some(slot) = self.free.pop()
      && let Some(place) = self.slots.get_mut(slot)
    {
      *place = Some((bytes, 1));
      return Block(slot);
    }
    self.slots.push(Some((bytes, 1)));
    Block(self.slots.len().saturating_sub(1))
  }

  /// A block's bytes; none for a block that is gone (never, while a node names it).
  fn bytes(&self, block: Block) -> &[u8] {
    self
      .slots
      .get(block.0)
      .and_then(Option::as_ref)
      .map(|(bytes, _)| bytes.as_slice())
      .unwrap_or_default()
  }

  /// One more holder of `block`.
  fn hold(&mut self, block: Block) {
    if let Some(Some((_, holders))) = self.slots.get_mut(block.0) {
      *holders = holders.saturating_add(1);
    }
  }

  /// One holder of `block` fewer; the block is freed with its last.
  fn release(&mut self, block: Block) {
    let Some(place) = self.slots.get_mut(block.0) else {
      return;
    };
    if let Some((_, holders)) = place.as_mut()
      && *holders > 1
    {
      *holders = holders.saturating_sub(1);
      return;
    }
    if place.take().is_some() {
      self.free.push(block.0);
    }
  }

  /// One holder of `block` fewer, and its bytes: moved out with the last holder, copied while another remains.
  fn take(&mut self, block: Block) -> Vec<u8> {
    let Some(place) = self.slots.get_mut(block.0) else {
      return Vec::new();
    };
    if let Some((bytes, holders)) = place.as_mut()
      && *holders > 1
    {
      *holders = holders.saturating_sub(1);
      return bytes.clone();
    }
    match place.take() {
      Some((bytes, _)) => {
        self.free.push(block.0);
        bytes
      }
      None => Vec::new(),
    }
  }

  /// The blocks alive.
  #[cfg(test)]
  pub(crate) fn live(&self) -> usize {
    self.slots.len().saturating_sub(self.free.len())
  }
}

impl Holders<Block> for Blocks {
  fn copied(&mut self, value: &Block) {
    self.hold(*value);
  }

  fn freed(&mut self, value: Block) {
    self.release(value);
  }
}

/// The current files: a map from path to block, and the blocks (module docs).
#[derive(Debug, Default)]
pub(crate) struct Files {
  map: Dim<Block>,
  blocks: Blocks,
}

impl Files {
  /// A file's bytes.
  pub(crate) fn get(&self, path: impl AsRef<[u8]>) -> Option<&[u8]> {
    self.map.get(path).map(|block| self.blocks.bytes(*block))
  }

  /// Whether `path` is a file.
  pub(crate) fn contains_key(&self, path: impl AsRef<[u8]>) -> bool {
    self.map.contains_key(path)
  }

  /// Sets `path`'s bytes; the bytes it replaced are let go.
  pub(crate) fn insert(&mut self, path: impl AsRef<[u8]>, bytes: Vec<u8>) {
    let block = self.blocks.add(bytes);
    let replaced =
      self
        .map
        .root
        .insert(&mut self.map.nodes, path.as_ref(), block, &mut self.blocks);
    if let Some(replaced) = replaced {
      self.blocks.release(replaced);
    }
  }

  /// Takes `path` out; its bytes.
  pub(crate) fn remove(&mut self, path: impl AsRef<[u8]>) -> Option<Vec<u8>> {
    let removed = self
      .map
      .root
      .remove(&mut self.map.nodes, path.as_ref(), &mut self.blocks)?;
    Some(self.blocks.take(removed))
  }

  /// Every file and its bytes, in path order.
  pub(crate) fn iter(&self) -> impl Iterator<Item = (String, &[u8])> + '_ {
    self
      .map
      .iter()
      .map(|(path, block)| (path, self.blocks.bytes(*block)))
  }

  /// Every file's path, in order.
  pub(crate) fn keys(&self) -> impl Iterator<Item = String> + '_ {
    self.map.keys()
  }

  /// Every file's bytes, in path order.
  pub(crate) fn values(&self) -> impl Iterator<Item = &[u8]> + '_ {
    self.map.values().map(|block| self.blocks.bytes(*block))
  }

  /// The files under `prefix` and their bytes, in path order.
  pub(crate) fn prefix(&self, prefix: &str) -> impl Iterator<Item = (String, &[u8])> + '_ {
    self
      .map
      .prefix(prefix)
      .map(|(path, block)| (path, self.blocks.bytes(*block)))
  }

  /// The block `path`'s file holds, given one more holder: what a held text shares with the file (A-716, the green's
  /// identity table), so holding a file's text copies no byte. None for no file at `path`.
  pub(crate) fn hold_block_of(&mut self, path: impl AsRef<[u8]>) -> Option<Block> {
    let block = *self.map.get(path)?;
    self.blocks.hold(block);
    Some(block)
  }

  /// A new block holding `bytes`, with one holder and named by no path (A-716: a text held before any file names it).
  pub(crate) fn add_unnamed_block(&mut self, bytes: Vec<u8>) -> Block {
    self.blocks.add(bytes)
  }

  /// A held block's bytes; none for a block that is gone (never, while the table holds it).
  pub(crate) fn block_bytes(&self, block: Block) -> &[u8] {
    self.blocks.bytes(block)
  }

  /// One holder of a held block fewer; the block is freed with its last.
  pub(crate) fn release_block(&mut self, block: Block) {
    self.blocks.release(block);
  }

  /// [`Dim::fork`]: the blocks stay with the arena.
  pub(crate) fn fork(&mut self) -> Files {
    Files {
      map: self.map.fork(),
      blocks: Blocks::default(),
    }
  }

  /// Exchanges this map's arena and blocks with `nodes` and `blocks`.
  pub(crate) fn swap_arena(&mut self, nodes: &mut Nodes<Block>, blocks: &mut Blocks) {
    self.map.swap_arena(nodes);
    std::mem::swap(&mut self.blocks, blocks);
  }

  /// Exchanges this map's arena and blocks with `other`'s.
  pub(crate) fn lend(&mut self, other: &mut Files) {
    self.map.lend(&mut other.map);
    std::mem::swap(&mut self.blocks, &mut other.blocks);
  }

  /// [`Dim::releasing`]: the blocks are let go as their nodes are freed.
  pub(crate) fn releasing(&mut self) -> Releasing {
    self.map.releasing()
  }

  /// [`Dim::visited`].
  pub(crate) fn visited(&self) -> u64 {
    self.map.visited()
  }
}

/// Format: the bytes that end a path in an attribute's key (module docs, "Attributes keep their order"): two NULs,
/// which sort before every escaped byte of a longer path.
const PATH_END: [u8; 2] = [0, 0];

/// Format: a NUL inside a path, escaped so it never reads as [`PATH_END`] and still sorts first.
const NUL_ESCAPED: [u8; 2] = [0, 1];

/// `path`'s bytes with every NUL escaped: the start of every attribute key of `path` and of the paths beneath it.
fn escaped(path: &str) -> Vec<u8> {
  let mut key = Vec::with_capacity(path.len().saturating_add(PATH_END.len()));
  for byte in path.bytes() {
    match byte {
      0 => key.extend_from_slice(&NUL_ESCAPED),
      other => key.push(other),
    }
  }
  key
}

/// The key prefix every attribute of `path` shares: its escaped bytes and [`PATH_END`].
fn path_key(path: &str) -> Vec<u8> {
  let mut key = escaped(path);
  key.extend_from_slice(&PATH_END);
  key
}

/// The key of `(path, name)`.
fn attribute_key(path: &str, name: &str) -> Vec<u8> {
  let mut key = path_key(path);
  key.extend_from_slice(name.as_bytes());
  key
}

/// `(path, name)` from an attribute's key; the inverse of [`attribute_key`].
fn split_key(key: &[u8]) -> (String, String) {
  let mut path = Vec::with_capacity(key.len());
  let mut rest = key;
  while let Some((first, after)) = rest.split_first() {
    if *first == 0 {
      match after.split_first() {
        // An escaped NUL of the path.
        Some((1, tail)) => {
          path.push(0);
          rest = tail;
          continue;
        }
        // The path's end: the name follows.
        Some((0, tail)) => return (text(path), text(tail.to_vec())),
        _ => break,
      }
    }
    path.push(*first);
    rest = after;
  }
  (text(path), String::new())
}

/// The extended attributes: a map from `(path, name)` to value, keyed as the module docs say.
#[derive(Debug, Default)]
pub(crate) struct Xattrs {
  map: Dim<Vec<u8>>,
}

impl Xattrs {
  /// `name`'s value on `path`.
  pub(crate) fn get(&self, path: &str, name: &str) -> Option<&Vec<u8>> {
    self.map.get(attribute_key(path, name))
  }

  /// Whether `path` carries `name`.
  pub(crate) fn contains_key(&self, path: &str, name: &str) -> bool {
    self.map.contains_key(attribute_key(path, name))
  }

  /// Sets `name` on `path`; the value it replaced.
  pub(crate) fn insert(&mut self, path: &str, name: &str, value: Vec<u8>) -> Option<Vec<u8>> {
    self.map.insert(attribute_key(path, name), value)
  }

  /// Takes `name` off `path`; its value.
  pub(crate) fn remove(&mut self, path: &str, name: &str) -> Option<Vec<u8>> {
    self.map.remove(attribute_key(path, name))
  }

  /// Every attribute, in `(path, name)` order.
  pub(crate) fn iter(&self) -> impl Iterator<Item = ((String, String), &Vec<u8>)> + '_ {
    self
      .map
      .root
      .iter(&self.map.nodes)
      .map(|(key, value)| (split_key(&key), value))
  }

  /// Every attribute of `path`'s tree, `(path, name)`: its own and its descendants', two descents and the keys they
  /// answer (A-626: what a replace takes away).
  pub(crate) fn iter_tree(&self, path: &str) -> impl Iterator<Item = (String, String)> + '_ {
    let own = self.map.root.prefix(&self.map.nodes, &path_key(path));
    let mut beneath = escaped(path);
    beneath.push(b'/');
    let below = self.map.root.prefix(&self.map.nodes, &beneath);
    own.chain(below).map(|(key, _)| split_key(&key))
  }

  /// `path`'s attributes, names in order, with their values: one descent, then its own keys.
  pub(crate) fn of(&self, path: &str) -> impl Iterator<Item = (String, &Vec<u8>)> + '_ {
    self
      .map
      .root
      .prefix(&self.map.nodes, &path_key(path))
      .map(|(key, value)| (split_key(&key).1, value))
  }

  /// [`Dim::fork`].
  pub(crate) fn fork(&mut self) -> Xattrs {
    Xattrs {
      map: self.map.fork(),
    }
  }

  /// [`Dim::swap_arena`].
  pub(crate) fn swap_arena(&mut self, nodes: &mut Nodes<Vec<u8>>) {
    self.map.swap_arena(nodes);
  }

  /// [`Dim::lend`].
  pub(crate) fn lend(&mut self, other: &mut Xattrs) {
    self.map.lend(&mut other.map);
  }

  /// [`Dim::releasing`].
  pub(crate) fn releasing(&mut self) -> Releasing {
    self.map.releasing()
  }

  /// [`Dim::visited`].
  pub(crate) fn visited(&self) -> u64 {
    self.map.visited()
  }
}

/// A green's eight maps' holds being given back a node a step (A-575: a green retired from its lineage), each map's in
/// its own arena ([`Arenas::release_step`]).
#[derive(Debug, Default)]
pub(crate) struct GreenRelease {
  pub(crate) files: Releasing,
  pub(crate) specials: Releasing,
  pub(crate) dirs: Releasing,
  pub(crate) modes: Releasing,
  pub(crate) symlinks: Releasing,
  pub(crate) hardlinks: Releasing,
  pub(crate) xattrs: Releasing,
  pub(crate) links_to: Releasing,
}

impl GreenRelease {
  /// Whether every map's hold has been given back.
  pub(crate) fn is_done(&self) -> bool {
    [
      &self.files,
      &self.specials,
      &self.dirs,
      &self.modes,
      &self.symlinks,
      &self.hardlinks,
      &self.xattrs,
      &self.links_to,
    ]
    .iter()
    .all(|releasing| releasing.is_done())
  }
}

/// The arenas a lineage's greens share (module docs, "Arenas are lent"): one per dimension, and the files' blocks.
#[derive(Debug, Default)]
pub(crate) struct Arenas {
  pub(crate) files: Nodes<Block>,
  pub(crate) blocks: Blocks,
  pub(crate) specials: Nodes<SpecialNode>,
  pub(crate) dirs: Nodes<()>,
  pub(crate) modes: Nodes<u32>,
  pub(crate) symlinks: Nodes<String>,
  pub(crate) hardlinks: Nodes<String>,
  pub(crate) xattrs: Nodes<Vec<u8>>,
  pub(crate) links_to: Nodes<BTreeSet<String>>,
}

impl Arenas {
  /// One node of `release` given back, in the first map whose release is not done (the files' with their blocks): a
  /// node is freed, and its value let go, when no other green's map names it. False once every map's is done.
  pub(crate) fn release_step(&mut self, release: &mut GreenRelease) -> bool {
    self
      .files
      .release_step(&mut release.files, &mut self.blocks)
      || self
        .specials
        .release_step(&mut release.specials, &mut Plain)
      || self.dirs.release_step(&mut release.dirs, &mut Plain)
      || self.modes.release_step(&mut release.modes, &mut Plain)
      || self
        .symlinks
        .release_step(&mut release.symlinks, &mut Plain)
      || self
        .hardlinks
        .release_step(&mut release.hardlinks, &mut Plain)
      || self.xattrs.release_step(&mut release.xattrs, &mut Plain)
      || self
        .links_to
        .release_step(&mut release.links_to, &mut Plain)
  }

  /// The nodes walks of every dimension visited: what a branch must leave unmoved (A-534's non-vacuity count).
  pub(crate) fn visited(&self) -> u64 {
    [
      self.files.visited(),
      self.specials.visited(),
      self.dirs.visited(),
      self.modes.visited(),
      self.symlinks.visited(),
      self.hardlinks.visited(),
      self.xattrs.visited(),
      self.links_to.visited(),
    ]
    .into_iter()
    .fold(0u64, u64::saturating_add)
  }

  /// The nodes every dimension copied because another holder named them.
  pub(crate) fn copied(&self) -> u64 {
    [
      self.files.copied(),
      self.specials.copied(),
      self.dirs.copied(),
      self.modes.copied(),
      self.symlinks.copied(),
      self.hardlinks.copied(),
      self.xattrs.copied(),
      self.links_to.copied(),
    ]
    .into_iter()
    .fold(0u64, u64::saturating_add)
  }

  /// The nodes alive in every dimension's arena.
  pub(crate) fn live(&self) -> usize {
    [
      self.files.live(),
      self.specials.live(),
      self.dirs.live(),
      self.modes.live(),
      self.symlinks.live(),
      self.hardlinks.live(),
      self.xattrs.live(),
      self.links_to.live(),
    ]
    .into_iter()
    .fold(0usize, usize::saturating_add)
  }

  /// Links that named no node in any arena: zero in every correct use.
  pub(crate) fn missing(&self) -> u64 {
    [
      self.files.missing(),
      self.specials.missing(),
      self.dirs.missing(),
      self.modes.missing(),
      self.symlinks.missing(),
      self.hardlinks.missing(),
      self.xattrs.missing(),
      self.links_to.missing(),
    ]
    .into_iter()
    .fold(0u64, u64::saturating_add)
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use super::*;

  /// A-534. Do: set attributes on paths that are prefixes of one another, hold a NUL or end in high bytes, then walk
  /// them and read each path's own. Expect: the walk yields `(path, name)` in the order of an ordered map of pairs,
  /// every key splits back to its pair, and a path's attributes never include a longer path's.
  #[test]
  fn attribute_keys_keep_the_order_of_pairs_and_split_back() {
    let pairs = [
      ("a", "x"),
      ("a", ""),
      ("a/b", "y"),
      ("a\0b", "z"),
      ("a\0", "w"),
      ("ab", "v"),
      ("", "u"),
      ("a\u{ff}", "t"),
    ];
    let mut xattrs = Xattrs::default();
    let mut model: BTreeMap<(String, String), Vec<u8>> = BTreeMap::new();
    for (index, (path, name)) in pairs.iter().enumerate() {
      let value = vec![u8::try_from(index).unwrap()];
      xattrs.insert(path, name, value.clone());
      model.insert(((*path).to_owned(), (*name).to_owned()), value);
    }
    let walked: Vec<((String, String), Vec<u8>)> = xattrs
      .iter()
      .map(|(key, value)| (key, value.clone()))
      .collect();
    let expected: Vec<((String, String), Vec<u8>)> = model.clone().into_iter().collect();
    assert_eq!(walked, expected);
    for path in ["a", "a\0", "", "ab"] {
      let own: Vec<String> = xattrs.of(path).map(|(name, _)| name).collect();
      let expected: Vec<String> = model
        .keys()
        .filter(|(at, _)| at == path)
        .map(|(_, name)| name.clone())
        .collect();
      assert_eq!(own, expected, "the attributes of {path:?}");
    }
  }

  /// Give `files`' hold back a node a step until nothing is left of it.
  fn release_a_node_a_step(files: &mut Files) {
    let mut releasing = files.releasing();
    while files
      .map
      .nodes
      .release_step(&mut releasing, &mut files.blocks)
    {}
  }

  /// A-534, A-575. Do: put a file, fork the map, change the file in the fork and remove another, then release both
  /// maps a node a step. Expect: each map reads its own bytes, the original's blocks are untouched by the fork's changes
  /// and by the fork's release, and once both are released every node and every block is gone.
  #[test]
  fn a_forked_files_map_shares_blocks_until_changed_and_frees_them_with_the_last_holder() {
    let mut files = Files::default();
    files.insert("a", b"one".to_vec());
    files.insert("b", b"two".to_vec());
    let mut fork = files.fork();
    files.lend(&mut fork);
    fork.insert("a", b"changed".to_vec());
    assert_eq!(fork.remove("b"), Some(b"two".to_vec()));
    fork.lend(&mut files);
    assert_eq!(files.get("a"), Some(b"one".as_slice()));
    assert_eq!(files.get("b"), Some(b"two".as_slice()));
    files.lend(&mut fork);
    assert_eq!(fork.get("a"), Some(b"changed".as_slice()));
    assert_eq!(fork.get("b"), None);
    release_a_node_a_step(&mut fork);
    fork.lend(&mut files);
    assert_eq!(files.get("a"), Some(b"one".as_slice()));
    release_a_node_a_step(&mut files);
    assert_eq!(files.map.nodes.live(), 0, "every node freed");
    assert_eq!(files.blocks.live(), 0, "every block freed");
  }
}
