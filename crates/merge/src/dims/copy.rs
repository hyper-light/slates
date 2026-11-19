//! A path's tree copied by reference in every map of a green's state (A-542's Copy, its maps; §4.17.4 item 3; A-534),
//! and taken away again (A-586): the parent module's "A tree is copied by reference" says what and why. The engine's
//! `Copy` operation (A-586) calls these, records the copy by one entry for the destination's prefix, and a green made at
//! a version before a copy takes the copy's tree away by [`Dim::cut_tree`].

use super::{Block, Dim, Files, PATH_END, Xattrs, escaped, path_key};
use crate::ptrie::{Holders, Plain, Ptrie, Shared};

/// Format: the byte between a path's names, which every key beneath a path holds right after it.
const SEPARATOR: u8 = b'/';

/// The prefix every key beneath `path` starts with: `path` and [`SEPARATOR`].
fn beneath(path: &[u8]) -> Vec<u8> {
  let mut prefix = Vec::with_capacity(path.len().saturating_add(1));
  prefix.extend_from_slice(path);
  prefix.push(SEPARATOR);
  prefix
}

/// The prefix of the keys in `path`'s tree other than its own: [`beneath`] `path`, or nothing for the root (the empty
/// path), whose tree is every key, its own among them.
fn below(path: &[u8]) -> Vec<u8> {
  if path.is_empty() {
    Vec::new()
  } else {
    beneath(path)
  }
}

/// Why a copy of a path's tree was refused; nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyRefused {
  /// The destination, or a key beneath it, is in the map: a copy never mixes with what is there.
  Occupied,
  /// The root as the destination: it is always present, and no copy is made onto it (a Subversion editor adds a path
  /// under a parent it opened, never the root, `libsvn_repos/commit.c`).
  Root,
}

/// What a copy brought to its destination.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Copied {
  /// The source's own key, copied to the destination's.
  pub(crate) own: bool,
  /// Keys beneath the source, grafted beneath the destination.
  pub(crate) beneath: bool,
}

impl Copied {
  /// Whether the copy brought any key.
  pub(crate) fn brought(&self) -> bool {
    self.own || self.beneath
  }
}

/// A path's tree held for a copy (A-626): the keys beneath it as one held subtree (every key, the root's own among them,
/// for the root) and its own key's value, its holder told. Taken before a commit changes any map, so every copy of one
/// version reads its source as the version before it, whatever an earlier copy of that version takes away; a graft
/// takes it, or gives it back when it refuses.
#[derive(Debug)]
pub(crate) struct Held<V> {
  /// The keys beneath the tree's own, as one held subtree.
  below: Option<Shared>,
  /// The tree's own key's value, one more holder of it told.
  own: Option<V>,
  /// Whether the tree is the root's, whose subtree holds its own key too.
  root: bool,
}

impl<V: Clone> Dim<V> {
  /// Copies `from`'s tree to `to` (the parent module's "A tree is copied by reference"): O(the paths' lengths), no walk.
  /// The root's tree is every key: they all come beneath `to`, and the root's own key to `to`'s. A hold and its graft,
  /// one after the other: the engine holds every source of a commit before it grafts any (A-626), so only the tests copy
  /// this way.
  #[cfg(test)]
  pub(crate) fn copy_tree(&mut self, from: &str, to: &str) -> Result<Copied, CopyRefused> {
    let held = self.hold_tree_with(from.as_bytes(), &mut Plain);
    self.graft_tree_with(held, to.as_bytes(), &mut Plain)
  }

  /// `from`'s tree held for a later graft ([`Held`]): one descent and one count, no walk.
  pub(crate) fn hold_tree(&mut self, from: &str) -> Held<V> {
    self.hold_tree_with(from.as_bytes(), &mut Plain)
  }

  /// A held tree grafted at `to` (the parent module's "A tree is copied by reference"); refused, and the hold given back,
  /// onto the root or a destination that holds a key.
  pub(crate) fn graft_tree(&mut self, held: Held<V>, to: &str) -> Result<Copied, CopyRefused> {
    self.graft_tree_with(held, to.as_bytes(), &mut Plain)
  }

  /// [`Dim::hold_tree`], the own value's holder told.
  fn hold_tree_with(&mut self, from: &[u8], holders: &mut impl Holders<V>) -> Held<V> {
    let below = self.root.subtree(&mut self.nodes, &below(from));
    let own = self.root.get(&self.nodes, from).cloned();
    if let Some(value) = &own {
      holders.copied(value);
    }
    Held {
      below,
      own,
      root: from.is_empty(),
    }
  }

  /// [`Dim::graft_tree`], each value told as its nodes are copied and freed.
  fn graft_tree_with(
    &mut self,
    held: Held<V>,
    to: &[u8],
    holders: &mut impl Holders<V>,
  ) -> Result<Copied, CopyRefused> {
    if to.is_empty() || self.holds_tree_of(to) {
      self.drop_held_with(held, holders);
      return Err(if to.is_empty() {
        CopyRefused::Root
      } else {
        CopyRefused::Occupied
      });
    }
    let Held { below, own, root } = held;
    let brought = below.as_ref().map_or(0, |shared| shared.len(&self.nodes));
    // The root's subtree holds its own key as well as every key beneath it.
    let own_in_below = root && own.is_some();
    let copied = Copied {
      own: own.is_some(),
      beneath: brought > usize::from(own_in_below),
    };
    let into = beneath(to);
    if let Some(shared) = below {
      if self
        .root
        .graft(&mut self.nodes, &into, shared, holders)
        .is_err()
      {
        // The check above leaves the graft nothing to refuse; were it to refuse, the own value is given back too.
        if let Some(value) = own {
          holders.freed(value);
        }
        return Err(CopyRefused::Occupied);
      }
      if own_in_below && let Some(misplaced) = self.root.remove(&mut self.nodes, &into, holders) {
        // The root's own key came with every key and stands at the destination's separator: it goes from there, and
        // is set at the destination's own key below.
        holders.freed(misplaced);
      }
    }
    if let Some(value) = own
      && let Some(replaced) = self.root.insert(&mut self.nodes, to, value, holders)
    {
      holders.freed(replaced);
    }
    Ok(copied)
  }

  /// Gives a hold back without grafting it, each value whose last holder goes told.
  fn drop_held_with(&mut self, held: Held<V>, holders: &mut impl Holders<V>) {
    if let Some(shared) = held.below {
      Ptrie::drop_subtree(&mut self.nodes, shared, holders);
    }
    if let Some(value) = held.own {
      holders.freed(value);
    }
  }

  /// Whether `path`'s tree holds a key: its own, or one beneath it (any key, for the root). Descents, no walk.
  pub(crate) fn holds_tree(&self, path: &str) -> bool {
    self.holds_tree_of(path.as_bytes())
  }

  /// How many keys `path`'s tree holds: its own and those beneath it (every key, for the root). Descents and the counts
  /// their nodes keep, no walk (A-626: whether a copy's source held anything at the version it is copied at).
  pub(crate) fn count_tree(&self, path: &str) -> usize {
    if path.is_empty() {
      return self.root.len(&self.nodes);
    }
    let own = usize::from(self.root.contains_key(&self.nodes, path.as_bytes()));
    own.saturating_add(self.root.prefix_len(&self.nodes, &below(path.as_bytes())))
  }

  /// [`Dim::holds_tree`] over the path's bytes.
  fn holds_tree_of(&self, path: &[u8]) -> bool {
    self.root.contains_key(&self.nodes, path) || self.root.has_prefix(&self.nodes, &below(path))
  }

  /// Takes `path`'s tree out of the map (A-586): its own key and every key beneath it, the inverse of a copy onto `path`.
  /// Two descents, O(the path's length) whatever the tree holds, no walk. What the cut held is given back at once: the
  /// one caller, a green made at a version before a copy, cuts a tree it shares with the green it was made from, so the
  /// give-back frees none of the tree's nodes, only the count its link held.
  pub(crate) fn cut_tree(&mut self, path: &str) {
    self.cut_tree_held(path.as_bytes(), &mut Plain);
  }

  /// [`Dim::cut_tree`], each value whose last node goes told so.
  fn cut_tree_held(&mut self, path: &[u8], holders: &mut impl Holders<V>) {
    if let Some(cut) = self.root.cut(&mut self.nodes, &below(path), holders) {
      Ptrie::drop_subtree(&mut self.nodes, cut, holders);
    }
    if let Some(removed) = self.root.remove(&mut self.nodes, path, holders) {
      holders.freed(removed);
    }
  }
}

impl Files {
  /// [`Dim::copy_tree`]: each copied file one more holder of its block, never a copy of its bytes.
  #[cfg(test)]
  pub(crate) fn copy_tree(&mut self, from: &str, to: &str) -> Result<Copied, CopyRefused> {
    let held = self.hold_tree(from);
    self.graft_tree(held, to)
  }

  /// [`Dim::hold_tree`]: the own file one more holder of its block, so a removal in the same commit frees no block a
  /// held tree still names.
  pub(crate) fn hold_tree(&mut self, from: &str) -> Held<Block> {
    self.map.hold_tree_with(from.as_bytes(), &mut self.blocks)
  }

  /// [`Dim::graft_tree`].
  pub(crate) fn graft_tree(&mut self, held: Held<Block>, to: &str) -> Result<Copied, CopyRefused> {
    self
      .map
      .graft_tree_with(held, to.as_bytes(), &mut self.blocks)
  }

  /// [`Dim::holds_tree`].
  pub(crate) fn holds_tree(&self, path: &str) -> bool {
    self.map.holds_tree(path)
  }

  /// [`Dim::count_tree`]: the files of `path`'s tree.
  pub(crate) fn count_tree(&self, path: &str) -> usize {
    self.map.count_tree(path)
  }

  /// [`Dim::cut_tree`]: a block is let go with its last holder.
  pub(crate) fn cut_tree(&mut self, path: &str) {
    self.map.cut_tree_held(path.as_bytes(), &mut self.blocks);
  }
}

/// A path's attributes held for a copy (A-626, [`Held`]): its own attributes and its descendants', each one held subtree
/// (every attribute, the root's own among them, for the root's descendants).
#[derive(Debug)]
pub(crate) struct HeldAttributes {
  /// The path's own attributes.
  own: Option<Shared>,
  /// Its descendants' attributes.
  below: Option<Shared>,
  /// Whether the path is the root.
  root: bool,
}

impl Xattrs {
  /// Copies the attributes of `from`'s tree to `to`'s: `from`'s own attributes, one prefix, and its descendants',
  /// another, both held before either is grafted. O(the paths' lengths), no walk. The root's descendants are every key,
  /// its own attributes among them: they come beneath `to` with the rest and are cut away from there, the root's own
  /// having come to `to`'s own prefix. As [`Dim::copy_tree`], the tests' form.
  #[cfg(test)]
  pub(crate) fn copy_tree(&mut self, from: &str, to: &str) -> Result<Copied, CopyRefused> {
    let held = self.hold_tree(from);
    self.graft_tree(held, to)
  }

  /// `from`'s attributes held for a later graft ([`HeldAttributes`]): two descents and two counts, no walk.
  pub(crate) fn hold_tree(&mut self, from: &str) -> HeldAttributes {
    HeldAttributes {
      own: self.map.root.subtree(&mut self.map.nodes, &path_key(from)),
      below: self
        .map
        .root
        .subtree(&mut self.map.nodes, &below(&escaped(from))),
      root: from.is_empty(),
    }
  }

  /// Held attributes grafted at `to`'s tree; refused, and the holds given back, onto the root or a tree that carries an
  /// attribute.
  pub(crate) fn graft_tree(
    &mut self,
    held: HeldAttributes,
    to: &str,
  ) -> Result<Copied, CopyRefused> {
    if to.is_empty() || self.holds_tree(to) {
      self.drop_held(held);
      return Err(if to.is_empty() {
        CopyRefused::Root
      } else {
        CopyRefused::Occupied
      });
    }
    let HeldAttributes { own, below, root } = held;
    let own_len = own.as_ref().map_or(0, |shared| shared.len(&self.map.nodes));
    let below_len = below
      .as_ref()
      .map_or(0, |shared| shared.len(&self.map.nodes));
    let root_own = if root { own_len } else { 0 };
    let copied = Copied {
      own: own_len > 0,
      beneath: below_len > root_own,
    };
    let beneath_to = beneath(&escaped(to));
    if self.graft_each([(own, path_key(to)), (below, beneath_to.clone())]) {
      return Err(CopyRefused::Occupied);
    }
    if root_own > 0 {
      let mut misplaced = beneath_to;
      misplaced.extend_from_slice(&PATH_END);
      if let Some(cut) = self
        .map
        .root
        .cut(&mut self.map.nodes, &misplaced, &mut Plain)
      {
        Ptrie::drop_subtree(&mut self.map.nodes, cut, &mut Plain);
      }
    }
    Ok(copied)
  }

  /// Gives held attributes back without grafting them.
  fn drop_held(&mut self, held: HeldAttributes) {
    for shared in [held.own, held.below].into_iter().flatten() {
      Ptrie::drop_subtree(&mut self.map.nodes, shared, &mut Plain);
    }
  }

  /// Grafts each held subtree at its prefix, in order; true when one was refused. The copy's checks leave the grafts
  /// nothing to refuse; were one to refuse, every hold is still given back.
  fn graft_each(&mut self, grafts: [(Option<Shared>, Vec<u8>); 2]) -> bool {
    let mut refused = false;
    for (held, into) in grafts {
      let Some(shared) = held else { continue };
      if refused {
        Ptrie::drop_subtree(&mut self.map.nodes, shared, &mut Plain);
      } else {
        refused = self
          .map
          .root
          .graft(&mut self.map.nodes, &into, shared, &mut Plain)
          .is_err();
      }
    }
    refused
  }

  /// How many attributes `path` and the paths beneath it carry (every one, for the root): descents and counts, no walk.
  pub(crate) fn count_tree(&self, path: &str) -> usize {
    if path.is_empty() {
      return self.map.root.len(&self.map.nodes);
    }
    [path_key(path), below(&escaped(path))]
      .iter()
      .map(|prefix| self.map.root.prefix_len(&self.map.nodes, prefix))
      .fold(0, usize::saturating_add)
  }

  /// Whether `path` or a path beneath it carries an attribute (any path, for the root). Descents, no walk.
  pub(crate) fn holds_tree(&self, path: &str) -> bool {
    self.map.root.has_prefix(&self.map.nodes, &path_key(path))
      || self
        .map
        .root
        .has_prefix(&self.map.nodes, &below(&escaped(path)))
  }

  /// [`Dim::cut_tree`] for attributes: `path`'s own and its descendants', two prefixes.
  pub(crate) fn cut_tree(&mut self, path: &str) {
    for prefix in [path_key(path), below(&escaped(path))] {
      if let Some(cut) = self.map.root.cut(&mut self.map.nodes, &prefix, &mut Plain) {
        Ptrie::drop_subtree(&mut self.map.nodes, cut, &mut Plain);
      }
    }
  }
}

#[cfg(test)]
mod tests;
