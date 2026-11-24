//! A persistent piece tree: each version of a file's content is a balanced tree of runs, and a version made from another
//! shares every subtree it did not change (A-818, A-803 (c); main's go of 2026-11-21 09:40 PST). The pure module first:
//! its caller to come is the content history ([`crate::history`]), whose re-anchor walk (A-793) the engine switch
//! replaces with it (A-818 piece 2).
//!
//! **Why.** A change that moves bytes (an insert, a delete, a truncate, an extend) re-anchors every earlier value of its
//! path's history on the new block today, so its cost grows with the history a reader pins: 101 to 2,551 values walked a
//! change, 0.61 to 20.1 ms, at 100 to 5,000 versions (BENCHMARKS, "Offset-shift layers"). Layers that record each
//! shift instead made a change flat but an old version's read carried its ranges through every later shift: 58 ms for
//! version 1 after 10,000 changes (A-803 (b), measured and rejected). A tree a version is neither: a change makes its
//! version from the one before by O(parts × log n) work, and any version reads in O(log n + the runs read), however
//! many versions came after it.
//!
//! **What it is.** A [`Run`] is `len` bytes at `at` in some source, which the caller names (a chunk, or a range of byte
//! identities). A version ([`Version`]) is a tree of runs in order: each node holds one run, its subtrees, its height and
//! the bytes beneath it, so a byte offset is found by descending on the left subtrees' lengths (the order-statistic
//! tree, as VS Code's piece tree keeps a left subtree's length in each node). The tree is an AVL tree kept balanced by
//! *join* (Blelloch, Ferizovic and Sun, "Just Join for Parallel Ordered Sets", SPAA 2016, tier A):
//! [`join`](PieceTrees::join) of two trees and a run between them rebalances along one spine in O(the heights'
//! difference), and split and concatenation are made of joins, O(log n) each. A change is [`PieceTrees::rebuild`]: the
//! next version as parts in order, each a range kept from the version before ([`Part::Kept`], a slice: two splits) or a
//! run the change added ([`Part::Added`]), concatenated: exactly the shape the engine's `segments` gives an op list.
//!
//! **Persistence by path copying** (Driscoll, Sarnak, Sleator and Tarjan, "Making Data Structures Persistent", JCSS 38,
//! 1989, tier A). Nodes live in one arena ([`PieceTrees`]) and count their holders: the versions whose root they are and
//! the nodes whose child they are (no shared-ownership pointer, CLAUDE.md R2). An operation opens a node
//! ([`PieceTrees::expose`]): a node only it holds is taken apart in place, and a node another holder names is left as it
//! is and its children gain a holder, so the operation's result is new nodes on the paths it changed over subtrees it
//! shares. Every version stays readable, and a change costs O(log n) new nodes a part, counted ([`PieceTrees::copied`]).
//! *What each decided.* DSST: path copying makes any linked structure fully persistent at the cost of copying the
//! access path, O(log n) space a change for a balanced tree; node copying cuts that to O(1) amortized for bounded
//! in-degree but needs back pointers and fat nodes, which joins rebuild wholesale here, so path copying is the fit.
//! VS Code's piece tree (the text buffer since VS Code 1.21, `pieceTreeBase.ts`, Peng Lyu's "Text Buffer
//! Reimplementation", 2018, tier C): a red-black tree of pieces over an original buffer and append-only added
//! buffers, each node caching its left subtree's length, so an offset is O(log n) and an edit splits at most one
//! piece; it is not persistent: a snapshot copies the piece list, O(n). This module keeps its node and its pieces and
//! adds DSST's path copying, so a snapshot (a version) is one held root.
//!
//! **Bounds.** A version of `p` runs is a tree of height at most 1.44 log2(p + 2) (AVL). A read of `len` bytes visits
//! the runs it yields and at most two root-to-leaf paths ([`PieceTrees::visited`]). Releasing a version frees only the
//! nodes no other version holds, iteratively ([`PieceTrees::release`]).
//!
//! **No panics.** Every slot is read with `get`; a link that names no node (a corruption no operation here makes) is a
//! typed refusal, [`Refusal::Missing`], and counted ([`PieceTrees::missing`]); an offset past a version's end is
//! [`Refusal::OutOfRange`], and lengths add checked ([`Refusal::Overflow`]).

// Its caller outside tests is the content history's engine switch (A-818 piece 2).
#![cfg_attr(not(test), allow(dead_code))]

use std::cell::Cell;

/// A node's place in the arena.
type Slot = usize;

/// A tree, by its root's slot: `None` is the empty tree. Inside the arena's operations a tree passed by value carries
/// one hold on its root, which the callee spends.
type Tree = Option<Slot>;

/// `len` bytes at `at` of the source `source`, a name the caller gives (a chunk, or a range of byte identities).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
  /// The source the bytes are in.
  pub(crate) source: usize,
  /// Their offset in it.
  pub(crate) at: u64,
  /// Their length: never zero in a tree.
  pub(crate) len: u64,
}

impl Run {
  /// The run cut at `offset` into the bytes before it and the bytes from it, both non-empty when `0 < offset < len`.
  fn cut(self, offset: u64) -> (Run, Run) {
    let head = Run {
      len: offset,
      ..self
    };
    let tail = Run {
      at: self.at.saturating_add(offset),
      len: self.len.saturating_sub(offset),
      ..self
    };
    (head, tail)
  }
}

/// A part of a version a change makes ([`PieceTrees::rebuild`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
  /// `len` bytes of the version before, from `from`.
  Kept {
    /// Where they start in the version before.
    from: u64,
    /// Their length.
    len: u64,
  },
  /// A run the change added.
  Added(Run),
}

/// Why an operation on a version was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
  /// A link named no node: a corruption no operation here makes.
  Missing,
  /// An offset or a range past the version's end.
  OutOfRange,
  /// A length past `u64`.
  Overflow,
  /// A change's kept ranges out of order or overlapping (`crate::byte_places`): its caller answers it as a whole
  /// replace.
  Unordered,
}

/// One version of a file's content: a held root in the arena. Not `Clone`: [`PieceTrees::fork`] makes another hold,
/// and [`PieceTrees::release`] gives one back.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Version {
  root: Tree,
}

/// One node: a run, its subtrees, and what the tree keeps of them.
#[derive(Clone, Copy, Debug)]
struct Node {
  left: Tree,
  run: Run,
  right: Tree,
  /// The longest path down from here, in nodes.
  height: u32,
  /// The bytes in this subtree, its run's included.
  total: u64,
  /// The versions and nodes that name this node.
  holders: u32,
}

/// The arena the versions made from one another share.
#[derive(Debug, Default)]
pub(crate) struct PieceTrees {
  slots: Vec<Option<Node>>,
  /// Freed slots, reused before the arena grows.
  free: Vec<Slot>,
  /// Nodes copied because another holder named them (module docs, "Persistence by path copying").
  copied: u64,
  /// Nodes a read visited (module docs, "Bounds"); a cell, as reads borrow the arena.
  visited: Cell<u64>,
  /// Links that named no node: zero in every correct use.
  missing: Cell<u64>,
}

impl PieceTrees {
  /// Nodes copied because another holder named them: the non-vacuity counter of sharing.
  pub(crate) fn copied(&self) -> u64 {
    self.copied
  }

  /// Nodes reads visited.
  pub(crate) fn visited(&self) -> u64 {
    self.visited.get()
  }

  /// Links that named no node: zero in every correct use.
  pub(crate) fn missing(&self) -> u64 {
    self.missing.get()
  }

  /// The nodes alive in the arena.
  pub(crate) fn live(&self) -> usize {
    self.slots.len().saturating_sub(self.free.len())
  }

  /// The empty version.
  pub(crate) fn empty() -> Version {
    Version { root: None }
  }

  /// A version of the runs in order (empty runs skipped), balanced: O(runs).
  pub(crate) fn of_runs(&mut self, runs: &[Run]) -> Result<Version, Refusal> {
    let runs: Vec<Run> = runs.iter().copied().filter(|run| run.len > 0).collect();
    Ok(Version {
      root: self.balanced(&runs)?,
    })
  }

  /// Another hold of `version`: O(1).
  pub(crate) fn fork(&mut self, version: &Version) -> Result<Version, Refusal> {
    self.retain(version.root)?;
    Ok(Version { root: version.root })
  }

  /// Gives `version`'s hold back, freeing the nodes no other holder names.
  pub(crate) fn release(&mut self, version: Version) {
    self.release_tree(version.root);
  }

  /// The bytes in `version`.
  pub(crate) fn len(&self, version: &Version) -> Result<u64, Refusal> {
    self.total(version.root)
  }

  /// The height of `version`'s tree, in nodes.
  pub(crate) fn height(&self, version: &Version) -> Result<u32, Refusal> {
    self.height_of(version.root)
  }

  /// The version holding `len` bytes of `version` from `from`, sharing its subtrees: O(log n).
  pub(crate) fn slice(
    &mut self,
    version: &Version,
    from: u64,
    len: u64,
  ) -> Result<Version, Refusal> {
    let end = from.checked_add(len).ok_or(Refusal::Overflow)?;
    if end > self.total(version.root)? {
      return Err(Refusal::OutOfRange);
    }
    self.retain(version.root)?;
    let (before, rest) = self.split(version.root, from)?;
    self.release_tree(before);
    let (middle, after) = self.split(rest, len)?;
    self.release_tree(after);
    Ok(Version { root: middle })
  }

  /// `first` then `second`, both spent: O(log n).
  pub(crate) fn concat(&mut self, first: Version, second: Version) -> Result<Version, Refusal> {
    Ok(Version {
      root: self.concat_trees(first.root, second.root)?,
    })
  }

  /// The version a change makes of `before`: its `parts` in order, each kept from `before` or added, sharing every
  /// subtree of `before` it keeps whole: O(parts × log n). `before` stays as it was.
  pub(crate) fn rebuild(&mut self, before: &Version, parts: &[Part]) -> Result<Version, Refusal> {
    let mut made = Self::empty();
    for part in parts {
      let next = match *part {
        Part::Kept { len: 0, .. } => continue,
        Part::Kept { from, len } => self.slice(before, from, len),
        Part::Added(run) => self.of_runs(&[run]),
      };
      let next = match next {
        Ok(next) => next,
        Err(refusal) => {
          self.release(made);
          return Err(refusal);
        }
      };
      made = self.concat(made, next)?;
    }
    Ok(made)
  }

  /// Yields the runs of `version` covering `len` bytes from `from`, cut to the range, in order: O(log n + the runs).
  pub(crate) fn read(
    &self,
    version: &Version,
    (from, len): (u64, u64),
    yield_run: &mut impl FnMut(Run),
  ) -> Result<(), Refusal> {
    let end = from.checked_add(len).ok_or(Refusal::Overflow)?;
    if end > self.total(version.root)? {
      return Err(Refusal::OutOfRange);
    }
    if len == 0 {
      return Ok(());
    }
    self.read_tree(version.root, (from, end), yield_run)
  }

  /// The runs of the subtree `tree` within `[from, end)` of it, cut to the range.
  fn read_tree(
    &self,
    tree: Tree,
    (from, end): (u64, u64),
    yield_run: &mut impl FnMut(Run),
  ) -> Result<(), Refusal> {
    let Some(slot) = tree else {
      return Ok(());
    };
    let node = *self.node(slot)?;
    self.visited.set(self.visited.get().saturating_add(1));
    let left = self.total(node.left)?;
    let run_end = left.saturating_add(node.run.len);
    if from < left {
      self.read_tree(node.left, (from, end.min(left)), yield_run)?;
    }
    if from < run_end && end > left {
      let start = from.max(left).saturating_sub(left);
      let stop = end.min(run_end).saturating_sub(left);
      yield_run(Run {
        source: node.run.source,
        at: node.run.at.saturating_add(start),
        len: stop.saturating_sub(start),
      });
    }
    if end > run_end {
      let shift = |offset: u64| offset.saturating_sub(run_end);
      self.read_tree(
        node.right,
        (shift(from.max(run_end)), shift(end)),
        yield_run,
      )?;
    }
    Ok(())
  }

  fn node(&self, slot: Slot) -> Result<&Node, Refusal> {
    let node = self.slots.get(slot).and_then(Option::as_ref);
    if node.is_none() {
      self.missing.set(self.missing.get().saturating_add(1));
    }
    node.ok_or(Refusal::Missing)
  }

  fn node_mut(&mut self, slot: Slot) -> Result<&mut Node, Refusal> {
    let node = self.slots.get_mut(slot).and_then(Option::as_mut);
    if node.is_none() {
      self.missing.set(self.missing.get().saturating_add(1));
    }
    node.ok_or(Refusal::Missing)
  }

  fn height_of(&self, tree: Tree) -> Result<u32, Refusal> {
    tree.map_or(Ok(0), |slot| self.node(slot).map(|node| node.height))
  }

  fn total(&self, tree: Tree) -> Result<u64, Refusal> {
    tree.map_or(Ok(0), |slot| self.node(slot).map(|node| node.total))
  }

  /// One more holder of `tree`'s root.
  fn retain(&mut self, tree: Tree) -> Result<(), Refusal> {
    if let Some(slot) = tree {
      let node = self.node_mut(slot)?;
      node.holders = node.holders.saturating_add(1);
    }
    Ok(())
  }

  /// One holder fewer of `tree`'s root, freeing each node whose last holder goes, iteratively.
  fn release_tree(&mut self, tree: Tree) {
    let mut pending: Vec<Slot> = tree.into_iter().collect();
    while let Some(slot) = pending.pop() {
      let Ok(node) = self.node_mut(slot) else {
        continue;
      };
      node.holders = node.holders.saturating_sub(1);
      if node.holders > 0 {
        continue;
      }
      let (left, right) = (node.left, node.right);
      self.free_slot(slot);
      pending.extend(left.into_iter().chain(right));
    }
  }

  fn free_slot(&mut self, slot: Slot) {
    if let Some(place) = self.slots.get_mut(slot) {
      *place = None;
      self.free.push(slot);
    }
  }

  /// A new node over `left`, `run` and `right` (their holds moved into it), held once.
  fn make(&mut self, left: Tree, run: Run, right: Tree) -> Result<Slot, Refusal> {
    let height = self
      .height_of(left)?
      .max(self.height_of(right)?)
      .saturating_add(1);
    let total = self
      .total(left)?
      .checked_add(run.len)
      .and_then(|sum| sum.checked_add(self.total(right).ok()?))
      .ok_or(Refusal::Overflow)?;
    let node = Node {
      left,
      run,
      right,
      height,
      total,
      holders: 1,
    };
    if let Some(slot) = self.free.pop()
      && let Some(place) = self.slots.get_mut(slot)
    {
      *place = Some(node);
      return Ok(slot);
    }
    self.slots.push(Some(node));
    Ok(self.slots.len().saturating_sub(1))
  }

  /// Opens the held node `slot` into its subtrees (each with a hold) and its run, spending the hold: a node only this
  /// hold names is taken apart in place; a shared one stays, its children gaining a holder (a copy, counted).
  fn expose(&mut self, slot: Slot) -> Result<(Tree, Run, Tree), Refusal> {
    let node = *self.node(slot)?;
    if node.holders <= 1 {
      self.free_slot(slot);
    } else {
      self.retain(node.left)?;
      self.retain(node.right)?;
      self.node_mut(slot)?.holders = node.holders.saturating_sub(1);
      self.copied = self.copied.saturating_add(1);
    }
    Ok((node.left, node.run, node.right))
  }

  /// A balanced tree of `runs` in order.
  fn balanced(&mut self, runs: &[Run]) -> Result<Tree, Refusal> {
    let middle = runs.len() / 2;
    let Some(run) = runs.get(middle) else {
      return Ok(None);
    };
    let left = self.balanced(runs.get(..middle).unwrap_or_default())?;
    let right = self.balanced(runs.get(middle.saturating_add(1)..).unwrap_or_default())?;
    self.make(left, *run, right).map(Some)
  }

  /// `left`, `run`, `right` in order as one balanced tree (all spent): O(the heights' difference).
  fn join(&mut self, left: Tree, run: Run, right: Tree) -> Result<Slot, Refusal> {
    let (left_height, right_height) = (self.height_of(left)?, self.height_of(right)?);
    match (left, right) {
      (Some(tall), _) if left_height > right_height.saturating_add(1) => {
        self.join_right(tall, run, right)
      }
      (_, Some(tall)) if right_height > left_height.saturating_add(1) => {
        self.join_left(left, run, tall)
      }
      _ => self.make(left, run, right),
    }
  }

  /// [`Self::join`] when `tall` is taller by two or more: down its right spine.
  fn join_right(&mut self, tall: Slot, run: Run, right: Tree) -> Result<Slot, Refusal> {
    let (left, top, middle) = self.expose(tall)?;
    let right_height = self.height_of(right)?;
    if self.height_of(middle)? <= right_height.saturating_add(1) {
      let joined = self.make(middle, run, right)?;
      if self.height_of(Some(joined))? <= self.height_of(left)?.saturating_add(1) {
        return self.make(left, top, Some(joined));
      }
      let turned = self.rotate_right(joined)?;
      let made = self.make(left, top, Some(turned))?;
      return self.rotate_left(made);
    }
    let joined = match middle {
      Some(middle) => self.join_right(middle, run, right)?,
      None => self.make(None, run, right)?,
    };
    let leans = self.height_of(Some(joined))? > self.height_of(left)?.saturating_add(1);
    let made = self.make(left, top, Some(joined))?;
    if leans {
      self.rotate_left(made)
    } else {
      Ok(made)
    }
  }

  /// [`Self::join`] when `tall` is taller by two or more: down its left spine.
  fn join_left(&mut self, left: Tree, run: Run, tall: Slot) -> Result<Slot, Refusal> {
    let (middle, top, right) = self.expose(tall)?;
    let left_height = self.height_of(left)?;
    if self.height_of(middle)? <= left_height.saturating_add(1) {
      let joined = self.make(left, run, middle)?;
      if self.height_of(Some(joined))? <= self.height_of(right)?.saturating_add(1) {
        return self.make(Some(joined), top, right);
      }
      let turned = self.rotate_left(joined)?;
      let made = self.make(Some(turned), top, right)?;
      return self.rotate_right(made);
    }
    let joined = match middle {
      Some(middle) => self.join_left(left, run, middle)?,
      None => self.make(left, run, None)?,
    };
    let leans = self.height_of(Some(joined))? > self.height_of(right)?.saturating_add(1);
    let made = self.make(Some(joined), top, right)?;
    if leans {
      self.rotate_right(made)
    } else {
      Ok(made)
    }
  }

  /// `(a x (b y c))` as `((a x b) y c)`.
  fn rotate_left(&mut self, slot: Slot) -> Result<Slot, Refusal> {
    let (a, x, right) = self.expose(slot)?;
    let Some(right) = right else {
      return self.make(a, x, None);
    };
    let (b, y, c) = self.expose(right)?;
    let lower = self.make(a, x, b)?;
    self.make(Some(lower), y, c)
  }

  /// `((a y b) x c)` as `(a y (b x c))`.
  fn rotate_right(&mut self, slot: Slot) -> Result<Slot, Refusal> {
    let (left, x, c) = self.expose(slot)?;
    let Some(left) = left else {
      return self.make(None, x, c);
    };
    let (a, y, b) = self.expose(left)?;
    let lower = self.make(b, x, c)?;
    self.make(a, y, Some(lower))
  }

  /// `tree` (spent) as its first `at` bytes and the rest, a run across `at` cut in two: O(log n).
  fn split(&mut self, tree: Tree, at: u64) -> Result<(Tree, Tree), Refusal> {
    let Some(slot) = tree else {
      return if at == 0 {
        Ok((None, None))
      } else {
        Err(Refusal::OutOfRange)
      };
    };
    let (left, run, right) = self.expose(slot)?;
    let left_len = self.total(left)?;
    let run_end = left_len.saturating_add(run.len);
    if at <= left_len {
      let (before, after) = self.split(left, at)?;
      let joined = self.join(after, run, right)?;
      return Ok((before, Some(joined)));
    }
    if at >= run_end {
      let (before, after) = self.split(right, at.saturating_sub(run_end))?;
      let joined = self.join(left, run, before)?;
      return Ok((Some(joined), after));
    }
    let (head, tail) = run.cut(at.saturating_sub(left_len));
    let before = self.join(left, head, None)?;
    let after = self.join(None, tail, right)?;
    Ok((Some(before), Some(after)))
  }

  /// `tree` (spent) as all but its last run, and that run.
  fn split_last(&mut self, slot: Slot) -> Result<(Tree, Run), Refusal> {
    let (left, run, right) = self.expose(slot)?;
    let Some(right) = right else {
      return Ok((left, run));
    };
    let (rest, last) = self.split_last(right)?;
    let joined = self.join(left, run, rest)?;
    Ok((Some(joined), last))
  }

  /// `first` then `second` (both spent) as one tree.
  fn concat_trees(&mut self, first: Tree, second: Tree) -> Result<Tree, Refusal> {
    let (Some(first), Some(_)) = (first, second) else {
      return Ok(first.or(second));
    };
    let (rest, last) = self.split_last(first)?;
    self.join(rest, last, second).map(Some)
  }
}

#[cfg(test)]
mod tests {
  //! The tree against a serial model: each version as the plain list of its bytes' places, made by the same parts.
  #![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
  )]

  use super::*;
  use proptest::prelude::*;

  /// The model: a version's bytes as (source, offset) places, in order.
  type Places = Vec<(usize, u64)>;

  fn places_of(trees: &PieceTrees, version: &Version) -> Places {
    let len = trees.len(version).unwrap();
    let mut out = Places::new();
    trees
      .read(version, (0, len), &mut |run| {
        out.extend((run.at..run.at + run.len).map(|at| (run.source, at)))
      })
      .unwrap();
    out
  }

  /// The model of [`PieceTrees::rebuild`].
  fn rebuilt(before: &Places, parts: &[Part]) -> Places {
    let mut out = Places::new();
    for part in parts {
      match *part {
        Part::Kept { from, len } => {
          out.extend_from_slice(&before[from as usize..(from + len) as usize])
        }
        Part::Added(run) => out.extend((run.at..run.at + run.len).map(|at| (run.source, at))),
      }
    }
    out
  }

  /// Generated parts over a version of `len` bytes: kept ranges and added runs of source `source`.
  fn parts_over(len: u64, raw: &[(bool, u16, u16)], source: usize) -> Vec<Part> {
    raw
      .iter()
      .map(|&(kept, a, b)| {
        if kept && len > 0 {
          let from = u64::from(a) % len;
          Part::Kept {
            from,
            len: u64::from(b) % (len - from + 1),
          }
        } else {
          Part::Added(Run {
            source,
            at: u64::from(a),
            len: u64::from(b) % 32 + 1,
          })
        }
      })
      .collect()
  }

  /// The AVL bound: a tree of `nodes` nodes is at most 1.4405 log2(nodes + 2) high.
  fn avl_bound(nodes: usize) -> u32 {
    (1.4405 * ((nodes + 2) as f64).log2()).floor() as u32
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// A-818 piece 1. Do: from a generated first version, make a chain of versions each rebuilt from a generated earlier
    /// one by generated parts (kept ranges and added runs), keeping every version; read every version whole and at a
    /// generated range after every change; then release the versions in a generated order. Expect: every version reads
    /// as the model's places, whole and cut to the range, however many versions came after it; every tree is within
    /// the AVL bound; a range read visits at most its runs and two paths; after the release nothing is alive.
    #[test]
    fn every_version_reads_as_the_model_however_many_came_after(
      first in prop::collection::vec(1u64..64, 0..12),
      chain in prop::collection::vec((any::<u16>(), prop::collection::vec((any::<bool>(), any::<u16>(), any::<u16>()), 0..8)), 1..24),
      range in (any::<u16>(), any::<u16>()),
      order in any::<u64>(),
    ) {
      let mut trees = PieceTrees::default();
      let runs: Vec<Run> = first.iter().enumerate().map(|(n, &len)| Run { source: 0, at: n as u64 * 100, len }).collect();
      let mut versions = vec![trees.of_runs(&runs).unwrap()];
      let mut models = vec![places_of(&trees, &versions[0])];
      for (step, (pick, raw)) in chain.iter().enumerate() {
        let base = usize::from(*pick) % versions.len();
        let parts = parts_over(models[base].len() as u64, raw, step + 1);
        let made = trees.rebuild(&versions[base], &parts).unwrap();
        models.push(rebuilt(&models[base], &parts));
        versions.push(made);
        for (version, model) in versions.iter().zip(&models) {
          prop_assert_eq!(&places_of(&trees, version), model);
          let count = trees.len(version).unwrap();
          prop_assert_eq!(count, model.len() as u64);
          let nodes = {
            let mut runs = 0usize;
            trees.read(version, (0, count), &mut |_| runs += 1).unwrap();
            runs
          };
          prop_assert!(trees.height(version).unwrap() <= avl_bound(nodes));
          let from = if count == 0 { 0 } else { u64::from(range.0) % count };
          let len = u64::from(range.1) % (count - from + 1);
          let (mut cut, mut yielded) = (Places::new(), 0u64);
          let visited = trees.visited();
          trees.read(version, (from, len), &mut |run| {
            yielded += 1;
            cut.extend((run.at..run.at + run.len).map(|at| (run.source, at)));
          }).unwrap();
          prop_assert_eq!(&cut[..], &model[from as usize..(from + len) as usize]);
          let height = u64::from(trees.height(version).unwrap());
          prop_assert!(trees.visited() - visited <= yielded + 2 * height);
        }
      }
      // A fork is one more hold: released, the version reads as before.
      let held = trees.fork(&versions[0]).unwrap();
      trees.release(held);
      prop_assert_eq!(&places_of(&trees, &versions[0]), &models[0]);
      let mut order = order;
      while !versions.is_empty() {
        let at = (order % versions.len() as u64) as usize;
        order = order.rotate_left(7) ^ 0x9e37_79b9;
        trees.release(versions.swap_remove(at));
      }
      prop_assert_eq!(trees.live(), 0);
      prop_assert_eq!(trees.missing(), 0);
    }
  }

  /// A-818 piece 1, the sharing's non-vacuity. Do: a version of 4,096 runs, then 1,000 versions, each overwriting one
  /// run's bytes of the one before at a spread offset, every version kept. Expect: each change copied at most a few
  /// paths' worth of nodes and made the arena grow by about as many, not by the 4,096 a whole copy takes; the first
  /// version reads as it was.
  #[test]
  fn a_change_shares_every_subtree_it_did_not_touch() {
    const RUNS: u64 = 4096;
    const CHANGES: u64 = 1000;
    let mut trees = PieceTrees::default();
    let runs: Vec<Run> = (0..RUNS)
      .map(|n| Run {
        source: 0,
        at: n * 10,
        len: 10,
      })
      .collect();
    let first = trees.of_runs(&runs).unwrap();
    let first_places = places_of(&trees, &first);
    let mut versions = vec![first];
    for change in 0..CHANGES {
      let before = versions.last().unwrap();
      let len = trees.len(before).unwrap();
      let at = (change * 2_654_435_761) % (len - 10);
      let parts = [
        Part::Kept { from: 0, len: at },
        Part::Added(Run {
          source: 1,
          at: change * 10,
          len: 10,
        }),
        Part::Kept {
          from: at + 10,
          len: len - at - 10,
        },
      ];
      let made = trees.rebuild(before, &parts).unwrap();
      versions.push(made);
    }
    let height = u64::from(trees.height(versions.last().unwrap()).unwrap());
    let per_change = (trees.live() as u64 - RUNS) / CHANGES;
    assert!(trees.copied() > 0, "nothing was shared");
    assert!(
      per_change <= 6 * height,
      "{per_change} nodes a change at height {height}"
    );
    assert_eq!(places_of(&trees, &versions[0]), first_places);
    for version in versions {
      trees.release(version);
    }
    assert_eq!(trees.live(), 0);
  }

  /// A-818 piece 1. Do: read and slice past a version's end. Expect: refused out of range, the version as it was.
  #[test]
  fn a_range_past_the_end_is_refused() {
    let mut trees = PieceTrees::default();
    let version = trees
      .of_runs(&[Run {
        source: 0,
        at: 0,
        len: 10,
      }])
      .unwrap();
    assert_eq!(
      trees.read(&version, (5, 6), &mut |_| {}),
      Err(Refusal::OutOfRange)
    );
    assert_eq!(
      trees.slice(&version, 11, 0).err(),
      Some(Refusal::OutOfRange)
    );
    assert_eq!(
      trees.slice(&version, u64::MAX, 2).err(),
      Some(Refusal::Overflow)
    );
    assert_eq!(places_of(&trees, &version).len(), 10);
    trees.release(version);
    assert_eq!(trees.live(), 0);
  }
}
