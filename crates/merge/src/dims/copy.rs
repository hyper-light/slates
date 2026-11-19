//! A path's tree copied by reference in every map of a green's state (A-542's Copy, its maps; §4.17.4 item 3; A-534):
//! the parent module's "A tree is copied by reference" says what and why. The engine's `Copy` operation, which records
//! the copy in the histories, the change index and `last_changed` by one entry for the destination's prefix, is A-542's
//! next part; until it lands, only this module's tests call these maps' copies.
#![cfg_attr(
  not(test),
  expect(
    dead_code,
    reason = "the engine's Copy operation (A-542, part 2) is the caller; this expectation goes when it lands"
  )
)]

use super::{Dim, Files, Xattrs, escaped, path_key};
use crate::ptrie::{GraftRefused, Holders, Plain, Ptrie};

/// Format: the byte between a path's names, which every key beneath a path holds right after it.
const SEPARATOR: u8 = b'/';

/// The prefix every key beneath `path` starts with: `path` and [`SEPARATOR`].
fn beneath(path: &[u8]) -> Vec<u8> {
  let mut prefix = Vec::with_capacity(path.len().saturating_add(1));
  prefix.extend_from_slice(path);
  prefix.push(SEPARATOR);
  prefix
}

/// Why a copy of a path's tree was refused; nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyRefused {
  /// The destination, or a key beneath it, is in the map: a copy never mixes with what is there.
  Occupied,
  /// The root as the source or the destination.
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

impl<V: Clone> Dim<V> {
  /// Copies `from`'s tree to `to` (the parent module's "A tree is copied by reference"): O(the paths' lengths), no walk.
  pub(crate) fn copy_tree(&mut self, from: &str, to: &str) -> Result<Copied, CopyRefused> {
    self.copy_tree_held(from.as_bytes(), to.as_bytes(), &mut Plain)
  }

  /// [`Dim::copy_tree`], each value it copies told it has one more holder.
  fn copy_tree_held(
    &mut self,
    from: &[u8],
    to: &[u8],
    holders: &mut impl Holders<V>,
  ) -> Result<Copied, CopyRefused> {
    if from.is_empty() || to.is_empty() {
      return Err(CopyRefused::Root);
    }
    let into = beneath(to);
    if self.root.contains_key(&self.nodes, to) || self.occupied(&into, holders) {
      return Err(CopyRefused::Occupied);
    }
    let held = self.root.subtree(&mut self.nodes, &beneath(from));
    let own = self.root.get(&self.nodes, from).cloned();
    let copied = Copied {
      own: own.is_some(),
      beneath: held.is_some(),
    };
    if let Some(shared) = held {
      self
        .root
        .graft(&mut self.nodes, &into, shared, holders)
        .map_err(|_: GraftRefused| CopyRefused::Occupied)?;
    }
    if let Some(value) = own {
      holders.copied(&value);
      if let Some(replaced) = self.root.insert(&mut self.nodes, to, value, holders) {
        holders.freed(replaced);
      }
    }
    Ok(copied)
  }

  /// Whether a key starts with `prefix`: one descent, and a count taken and given back, never a walk.
  fn occupied(&mut self, prefix: &[u8], holders: &mut impl Holders<V>) -> bool {
    match self.root.subtree(&mut self.nodes, prefix) {
      Some(shared) => {
        Ptrie::drop_subtree(&mut self.nodes, shared, holders);
        true
      }
      None => false,
    }
  }
}

impl Files {
  /// [`Dim::copy_tree`]: each copied file one more holder of its block, never a copy of its bytes.
  pub(crate) fn copy_tree(&mut self, from: &str, to: &str) -> Result<Copied, CopyRefused> {
    self
      .map
      .copy_tree_held(from.as_bytes(), to.as_bytes(), &mut self.blocks)
  }
}

impl Xattrs {
  /// Copies the attributes of `from`'s tree to `to`'s: `from`'s own attributes, one prefix, and its descendants',
  /// another, both held before either is grafted. O(the paths' lengths), no walk.
  pub(crate) fn copy_tree(&mut self, from: &str, to: &str) -> Result<Copied, CopyRefused> {
    if from.is_empty() || to.is_empty() {
      return Err(CopyRefused::Root);
    }
    let own_to = path_key(to);
    let beneath_to = beneath(&escaped(to));
    if self.map.occupied(&own_to, &mut Plain) || self.map.occupied(&beneath_to, &mut Plain) {
      return Err(CopyRefused::Occupied);
    }
    let own = self.map.root.subtree(&mut self.map.nodes, &path_key(from));
    let below = self
      .map
      .root
      .subtree(&mut self.map.nodes, &beneath(&escaped(from)));
    let copied = Copied {
      own: own.is_some(),
      beneath: below.is_some(),
    };
    // The checks above leave the graft nothing to refuse; were it to refuse, every hold is still given back.
    let mut refused = false;
    for (held, into) in [(own, own_to), (below, beneath_to)] {
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
    if refused {
      Err(CopyRefused::Occupied)
    } else {
      Ok(copied)
    }
  }
}

#[cfg(test)]
mod tests;
