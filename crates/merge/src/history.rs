//! A green's content history (W2; §4.16 "Splice": "splices accepted ops into a new version by extent-list surgery
//! without copying bytes"): each version of a file is a persistent tree of runs of *byte identities*
//! ([`crate::piece_tree`]), and one index a path chain says where each identity's bytes are now
//! ([`crate::byte_places`]). A change makes its version from the one before in O(the change's parts × log n) and walks
//! no other version, however long the path's history (A-818, A-803 (c); main's rulings of 2026-11-21 09:40 and 10:35).
//!
//! **Why.** The engine kept a whole copy of a file at every version that changed it, which the retention cap turned
//! into `BudgetExceeded` after cap / file size versions (2026-11-07). A-728 then shared runs between versions, and
//! A-793 anchored every earlier value on the newest block so the history kept only what each change removed, not the
//! earlier block: 73 KB of retention a version at 100k clients became 64 bytes a 64-byte edit. But A-793's change
//! re-anchored every value of its path's history: 101 to 2,551 values walked, 0.61 to 20.1 ms a change, at 100 to
//! 5,000 versions of a path changed by inserts (BENCHMARKS, "Offset-shift layers"). A-809's layers made an overwrite
//! flat, and A-803 (b)'s shift layers made an insert flat but an old read 58 ms (measured and rejected). This module
//! replaces the re-anchor: nothing in it walks the versions of a path.
//!
//! **What it holds.**
//! - [`Chunks`], the green's byte store: immutable chunks with a holder count each (handles and counts, no
//!   shared-ownership pointer, CLAUDE.md R2), each the history's own bytes or a file's block it holds one holder of
//!   (A-728: a file's block is named, never copied). It also keeps the arena of version trees and the path chains'
//!   anchors.
//! - An *anchor* a path chain: its [`BytePlaces`] (where each identity's bytes are), the chunk of its newest bytes
//!   (the path's current block), the number of changes it took, and its live values by the change that made them.
//! - A [`Value`]: one version of one file, its tree of identity runs, its length and its anchor.
//!
//! **A change** ([`change`]). The engine's own `segments` turn the change's ops into parts of the latest value (kept
//! ranges and added runs); the anchor's index copies the ranges the change removed out of the newest block into one
//! owned chunk (a range that is the whole file keeps the block itself, as a whole replace kept it) and gives the added
//! bytes new identities; the new version's tree is the latest's rebuilt from the parts; the anchor names the new block.
//! The history then holds what A-793's held: the newest block and the bytes each change removed, plus the trees' nodes
//! and the index, charged ([`Chunks::bytes`]). A change whose parts do not add up to the new block's length (an op
//! whose post-state was shorter than it declared, which `apply` clamps) is no change of the latest: the caller names
//! the new block whole as a new anchor, and the earlier block stays as the earlier versions' bytes, as before.
//!
//! **A read** of any version walks its tree's runs and locates each through the anchor: O(log n + the runs and the
//! places they cover), however many versions came after it.
//!
//! **Retirement.** The bytes a change removed are read only by the versions before it. An anchor keeps its live values
//! by the change that made each; when the earliest live one is past a change, what that change stored is retired
//! ([`BytePlaces::retire_through`]) and its chunks released. An anchor with no live value goes with its newest chunk.
//!
//! **Lifetimes.** The blocks live in the files' arena, which a lineage's greens share and lend ([`crate::dims`], "Arenas
//! are lent"), while the chunks are the green's own. A lineage member is read and written only while lent, its arenas
//! then the lineage's, so a block a chunk names is always in the arena at hand; a green made at another's version starts
//! with no history; a dropped value lets its block go into the same arena; and a retired green gives its chunks' holds
//! back a chunk a step ([`Chunks::release_step`]). Every function that reads a value's bytes takes the blocks with the
//! chunks.
//!
//! **The oracle** is today's history before this change, kept verbatim as `crate::history_reference` (test-only):
//! generated histories run through both, and every version's bytes and ranges must be equal, as must `apply`'s.

use std::collections::BTreeMap;

use crate::byte_places::{BytePlaces, Place};
use crate::dims::{Block, Blocks};
use crate::ops_doc::{Op, OpKind};
use crate::piece_tree::{Part, PieceTrees, Run, Version};

/// Where a chunk's bytes are (module docs, "What it holds").
#[derive(Debug)]
enum Source {
  /// Bytes the history owns.
  Owned(Vec<u8>),
  /// A file's block, of which the chunk holds one holder, and its length (a block never changes): the bytes the chunk
  /// keeps alive, all of which it counts (A-728 part 4, the honest charge).
  Block {
    /// The block.
    block: Block,
    /// Its length.
    len: usize,
  },
}

impl Source {
  /// The bytes the chunk counts: an owned chunk's length, a block's whole length.
  fn len(&self) -> usize {
    match self {
      Source::Owned(bytes) => bytes.len(),
      Source::Block { len, .. } => *len,
    }
  }

  /// The chunk's bytes.
  fn bytes<'a>(&'a self, blocks: &'a Blocks) -> &'a [u8] {
    match self {
      Source::Owned(bytes) => bytes,
      Source::Block { block, .. } => blocks.bytes(*block),
    }
  }

  /// Gives back what the chunk holds: the block's holder, for a block.
  fn let_go(self, blocks: &mut Blocks) {
    if let Source::Block { block, .. } = self {
      blocks.release(block);
    }
  }
}

/// A path chain's anchor (module docs, "What it holds").
#[derive(Debug)]
struct Anchor {
  /// Where each identity's bytes are.
  places: BytePlaces,
  /// The chunk of the newest bytes: the path's current block, or the bytes of a value stored whole.
  current: usize,
  /// The changes taken: the number the next change's value gets.
  changes: u64,
  /// The live values, by the change that made each (0: the first value).
  live: BTreeMap<u64, usize>,
}

/// Format: the bytes a tree node is charged (module docs, "A change").
pub(crate) const NODE_BYTES: usize = crate::piece_tree::NODE_BYTES;

/// The arena of chunks, version trees and anchors a green's history values share (module docs).
#[derive(Debug, Default)]
pub(crate) struct Chunks {
  /// Each slot's source and its holders; `None` once freed.
  slots: Vec<Option<(Source, usize)>>,
  /// Freed slots, reused first.
  free: Vec<usize>,
  /// The bytes the live chunks hold, owned or named.
  bytes: usize,
  /// The bytes of the live chunks that are files' blocks (A-728).
  named: usize,
  /// The versions' trees.
  trees: PieceTrees,
  /// The path chains' anchors; `None` once freed.
  anchors: Vec<Option<Anchor>>,
  /// Freed anchor slots, reused first.
  anchors_free: Vec<usize>,
  /// The bytes the anchors' indexes are charged, a running total.
  places: usize,
  /// The tree nodes changes made, in all: the non-vacuity counter of a change's cost (module docs, "A change").
  made: u64,
}

impl Chunks {
  /// The bytes the history holds: the live chunks, owned or named, the trees' nodes and the anchors' indexes.
  pub(crate) fn bytes(&self) -> usize {
    self
      .bytes
      .saturating_add(self.trees.live().saturating_mul(NODE_BYTES))
      .saturating_add(self.places)
  }

  /// The bytes the live chunks own: what the history copied (A-728: none for a value stored whole).
  pub(crate) fn owned_bytes(&self) -> usize {
    self.bytes.saturating_sub(self.named)
  }

  /// The tree nodes changes made, in all (the engine counts each change's own, `Changed::nodes`).
  #[cfg(test)]
  pub(crate) fn nodes_made(&self) -> u64 {
    self.made
  }

  /// One chunk of a retired green's history given back: its slot freed and, for a file's block, the block's holder let
  /// go (module docs, "Lifetimes"). False once no chunk is left, when the trees and anchors go too. Every value naming
  /// the chunks is dropped with the green.
  pub(crate) fn release_step(&mut self, blocks: &mut Blocks) -> bool {
    let Some(slot) = self.slots.pop() else {
      *self = Chunks::default();
      return false;
    };
    if let Some((source, _)) = slot {
      source.let_go(blocks);
    }
    true
  }

  /// Whether every chunk has been given back.
  pub(crate) fn is_released(&self) -> bool {
    self.slots.is_empty()
  }

  /// What `values` hold, counted from the values themselves: the distinct chunks their anchors name, the distinct tree
  /// nodes they reach and their anchors' indexes. The recount the running totals are held to (a chunk no value names is
  /// a leak, a value naming a freed chunk a double release).
  pub(crate) fn referenced_bytes<'a>(&self, values: impl Iterator<Item = &'a Value>) -> usize {
    let mut anchors = std::collections::BTreeSet::new();
    let mut roots = Vec::new();
    for value in values {
      anchors.extend(value.anchor);
      roots.push(&value.tree);
    }
    let mut chunks = std::collections::BTreeSet::new();
    let mut places = 0usize;
    for anchor in anchors.iter().filter_map(|slot| self.anchor(*slot)) {
      chunks.insert(anchor.current);
      chunks.extend(anchor.places.stored_chunks());
      places = places.saturating_add(anchor.places.bytes());
    }
    chunks
      .into_iter()
      .filter_map(|chunk| self.slots.get(chunk).and_then(Option::as_ref))
      .map(|(source, _)| source.len())
      .fold(0usize, usize::saturating_add)
      .saturating_add(
        self
          .trees
          .reachable(roots.into_iter())
          .saturating_mul(NODE_BYTES),
      )
      .saturating_add(places)
  }

  /// Adds a chunk from `source`, held by nobody yet.
  fn add_source(&mut self, source: Source) -> usize {
    let len = source.len();
    self.bytes = self.bytes.saturating_add(len);
    if matches!(source, Source::Block { .. }) {
      self.named = self.named.saturating_add(len);
    }
    if let Some(chunk) = self.free.pop()
      && let Some(slot) = self.slots.get_mut(chunk)
    {
      *slot = Some((source, 0));
      return chunk;
    }
    self.slots.push(Some((source, 0)));
    self.slots.len().saturating_sub(1)
  }

  /// One more holder of `chunk`.
  fn retain(&mut self, chunk: usize) {
    if let Some(Some((_, holders))) = self.slots.get_mut(chunk) {
      *holders = holders.saturating_add(1);
    }
  }

  /// One holder of `chunk` fewer; the chunk is freed with its last, a block's holder let go with it.
  fn release(&mut self, chunk: usize, blocks: &mut Blocks) {
    let Some(slot) = self.slots.get_mut(chunk) else {
      return;
    };
    let last = match slot {
      Some((_, holders)) => {
        *holders = holders.saturating_sub(1);
        *holders == 0
      }
      None => false,
    };
    if !last {
      return;
    }
    if let Some((source, _)) = slot.take() {
      let len = source.len();
      self.bytes = self.bytes.saturating_sub(len);
      if matches!(source, Source::Block { .. }) {
        self.named = self.named.saturating_sub(len);
      }
      source.let_go(blocks);
      self.free.push(chunk);
    }
  }

  /// The bytes of `len` at `at` of `chunk`, or none if the chunk is gone (never, while a value names it).
  fn chunk_bytes<'a>(
    &'a self,
    blocks: &'a Blocks,
    (chunk, at, len): (usize, u64, u64),
  ) -> &'a [u8] {
    let start = usize::try_from(at).unwrap_or(usize::MAX);
    let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX));
    self
      .slots
      .get(chunk)
      .and_then(Option::as_ref)
      .and_then(|(source, _)| source.bytes(blocks).get(start..end))
      .unwrap_or_default()
  }

  fn anchor(&self, slot: usize) -> Option<&Anchor> {
    self.anchors.get(slot).and_then(Option::as_ref)
  }

  /// A new anchor over the chunk `current` (held for it) of `len` bytes, with its first value's tree.
  fn new_anchor(&mut self, current: usize, len: usize) -> Value {
    self.retain(current);
    let (places, run) = BytePlaces::first(u64::try_from(len).unwrap_or(u64::MAX));
    self.places = self.places.saturating_add(places.bytes());
    let tree = self
      .trees
      .of_runs(&run.into_iter().collect::<Vec<Run>>())
      .unwrap_or_default();
    let anchor = Anchor {
      places,
      current,
      changes: 0,
      live: BTreeMap::from([(0, 1)]),
    };
    let slot = match self.anchors_free.pop() {
      Some(slot) if self.anchors.get(slot).is_some() => {
        if let Some(place) = self.anchors.get_mut(slot) {
          *place = Some(anchor);
        }
        slot
      }
      _ => {
        self.anchors.push(Some(anchor));
        self.anchors.len().saturating_sub(1)
      }
    };
    let structure = self.structure_of(&tree, slot);
    Value {
      tree,
      len,
      anchor: Some(slot),
      index: 0,
      own: None,
      structure,
    }
  }

  /// The structure a latest value of `tree` over `anchor` counts as current: its tree's nodes and the anchor's latest
  /// order and entries.
  fn structure_of(&self, tree: &Version, anchor: usize) -> usize {
    self
      .trees
      .nodes_in(tree)
      .saturating_mul(NODE_BYTES)
      .saturating_add(
        self
          .anchor(anchor)
          .map_or(0, |anchor| anchor.places.current_bytes()),
      )
  }

  /// Calls `each` with the bytes of `value` in `[from, to)`, in order, from the newest block or the stored chunks.
  fn each_run(
    &self,
    blocks: &Blocks,
    value: &Value,
    (from, to): (usize, usize),
    each: &mut impl FnMut(&[u8]),
  ) {
    let Some(anchor) = value.anchor.and_then(|slot| self.anchor(slot)) else {
      return;
    };
    let from = u64::try_from(from).unwrap_or(u64::MAX);
    let len = u64::try_from(to).unwrap_or(u64::MAX).saturating_sub(from);
    let _ = self.trees.read(&value.tree, (from, len), &mut |run| {
      let _ = anchor
        .places
        .locate((run.at, run.len), &mut |place| match place {
          Place::Current { at, len } => each(self.chunk_bytes(blocks, (anchor.current, at, len))),
          Place::Stored { chunk, at, len } => each(self.chunk_bytes(blocks, (chunk, at, len))),
        });
    });
  }
}

/// One version of a file in the history: its tree of identity runs, its length and its anchor (module docs).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Value {
  /// The identity runs whose bytes, in order, are the value.
  tree: Version,
  /// The value's length.
  len: usize,
  /// The anchor its identities are placed by; none for a value of no bytes.
  anchor: Option<usize>,
  /// The change of its anchor that made it (0: the first value).
  index: u64,
  /// While the value is its path's latest and names the file's current block, that block's length: the bytes the value
  /// shares with the current file, counted as current, not history (A-728 part 4). Cleared when a change supersedes
  /// it ([`Value::supersede`]).
  own: Option<usize>,
  /// What describes this value alone as it was made: its tree's nodes and its anchor's latest order and entries, the
  /// structure counted as current while it is its path's latest ([`Value::structure`]).
  structure: usize,
}

/// A run of a value by identity, for comparing two values without reading what they share: `len` identities from
/// `identity` of the anchor `anchor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Piece {
  anchor: usize,
  identity: u64,
  len: usize,
}

/// A run of the bytes the engine's `apply` writes ([`Value::applied_runs`]): a run of the value it keeps, or bytes of the
/// increment's post-state.
#[derive(Clone, Copy)]
enum Applied {
  /// A run of the value, by identity.
  Shared(Piece),
  /// `len` bytes of the post-state at `src`.
  Post { src: usize, len: usize },
}

impl Applied {
  /// The post-state run `op` adds, clamped to the post-state's `post_len` bytes as `apply`'s `push_slice` clamps it.
  fn post(op: &Op, post_len: usize) -> Option<Applied> {
    let src = usize::try_from(op.src).unwrap_or(usize::MAX).min(post_len);
    let end = src
      .saturating_add(usize::try_from(op.len).ok()?)
      .min(post_len);
    Some(Applied::Post {
      src,
      len: end.saturating_sub(src),
    })
  }

  /// Its length.
  fn len(&self) -> usize {
    match self {
      Applied::Shared(piece) => piece.len,
      Applied::Post { len, .. } => *len,
    }
  }

  /// What is left of it past its first `step` bytes.
  fn after(self, step: usize) -> Applied {
    match self {
      Applied::Shared(piece) => Applied::Shared(piece.after(step)),
      Applied::Post { src, len } => Applied::Post {
        src: src.saturating_add(step),
        len: len.saturating_sub(step),
      },
    }
  }
}

impl Piece {
  /// What is left of it past its first `step` bytes.
  fn after(self, step: usize) -> Piece {
    Piece {
      identity: self
        .identity
        .saturating_add(u64::try_from(step).unwrap_or(u64::MAX)),
      len: self.len.saturating_sub(step),
      ..self
    }
  }
}

impl Value {
  /// The value's length.
  pub(crate) fn len(&self) -> usize {
    self.len
  }

  /// A file's `block` of `len` bytes as the newest bytes of a new anchor, the caller's holder of it now the history's:
  /// no byte copied (A-728). The holder is let go at once for no bytes, a value of none.
  pub(crate) fn named(chunks: &mut Chunks, blocks: &mut Blocks, block: Block, len: usize) -> Value {
    if len == 0 {
      blocks.release(block);
      return Value::default();
    }
    let chunk = chunks.add_source(Source::Block { block, len });
    let mut value = chunks.new_anchor(chunk, len);
    value.own = Some(len);
    value
  }

  /// `bytes` as the newest bytes of a new anchor, owned (none for no bytes).
  #[cfg(test)]
  pub(crate) fn whole(chunks: &mut Chunks, bytes: &[u8]) -> Value {
    if bytes.is_empty() {
      return Value::default();
    }
    let chunk = chunks.add_source(Source::Owned(bytes.to_vec()));
    chunks.new_anchor(chunk, bytes.len())
  }

  /// The length of the file block the value shares while it is its path's latest, the bytes counted as current (none
  /// once superseded, or for a value naming no block of its own).
  pub(crate) fn own_len(&self) -> usize {
    self.own.unwrap_or(0)
  }

  /// The structure this value counts as current while it is its path's latest (as the current value's piece list was
  /// counted before A-818): its tree's nodes and its anchor's latest order and entries, as they were when it was made.
  pub(crate) fn structure(&self) -> usize {
    self.structure
  }

  /// Whether the value is `block` whole: its anchor's newest bytes, read through one run.
  pub(crate) fn is_block(&self, chunks: &Chunks, block: Block) -> bool {
    let Some(anchor) = self.anchor.and_then(|slot| chunks.anchor(slot)) else {
      return false;
    };
    let names = matches!(
      chunks.slots.get(anchor.current),
      Some(Some((Source::Block { block: named, len }, _))) if *named == block && *len == self.len
    );
    names && self.index == anchor.changes && chunks.trees.height(&self.tree) == Ok(1)
  }

  /// The value's bytes, copied out.
  pub(crate) fn materialize(&self, chunks: &Chunks, blocks: &Blocks) -> Vec<u8> {
    self.read((chunks, blocks), 0, self.len)
  }

  /// The value's bytes in `[from, to)` (clamped to its length), copied out of only the runs that hold them: what a
  /// paged read of an earlier version takes, without the whole file.
  pub(crate) fn read(
    &self,
    (chunks, blocks): (&Chunks, &Blocks),
    from: usize,
    to: usize,
  ) -> Vec<u8> {
    let to = to.min(self.len);
    let from = from.min(to);
    let mut out = Vec::with_capacity(to.saturating_sub(from));
    chunks.each_run(blocks, self, (from, to), &mut |bytes| {
      out.extend_from_slice(bytes)
    });
    out
  }

  /// The value's runs by identity, in order.
  fn pieces(&self, chunks: &Chunks) -> Vec<Piece> {
    let mut out = Vec::new();
    let Some(anchor) = self.anchor else {
      return out;
    };
    let len = u64::try_from(self.len).unwrap_or(u64::MAX);
    let _ = chunks.trees.read(&self.tree, (0, len), &mut |run| {
      out.push(Piece {
        anchor,
        identity: run.at,
        len: usize::try_from(run.len).unwrap_or(usize::MAX),
      });
    });
    out
  }

  /// The bytes of `piece`.
  fn piece_bytes(chunks: &Chunks, blocks: &Blocks, piece: Piece) -> Vec<u8> {
    let mut out = Vec::with_capacity(piece.len);
    let Some(anchor) = chunks.anchor(piece.anchor) else {
      return out;
    };
    let len = u64::try_from(piece.len).unwrap_or(u64::MAX);
    let _ = anchor
      .places
      .locate((piece.identity, len), &mut |place| match place {
        Place::Current { at, len } => {
          out.extend_from_slice(chunks.chunk_bytes(blocks, (anchor.current, at, len)))
        }
        Place::Stored { chunk, at, len } => {
          out.extend_from_slice(chunks.chunk_bytes(blocks, (chunk, at, len)))
        }
      });
    out
  }

  /// Whether the bytes `ops` make of this value, their added bytes read from `post_state` as the engine's `apply` reads
  /// them, are `other`'s bytes: compared run against run, so a stretch both name by the same identities is equal
  /// unread, and only the runs the ops added and the runs the two values do not share are read. What the verdict's
  /// whole-file identity check needs (`Green::lands_on_head`): it materialized this value, applied the ops to a second
  /// copy and compared that with the head, the file's size three times over (until 2026-11-09).
  pub(crate) fn applied_equals(
    &self,
    (chunks, blocks): (&Chunks, &Blocks),
    (ops, post_state): (&[Op], crate::named::Post<'_>),
    other: &Value,
  ) -> bool {
    let post_len = usize::try_from(post_state.len()).unwrap_or(usize::MAX);
    let Some(runs) = Value::applied_runs(&self.pieces(chunks), self.len, ops, post_len) else {
      return false;
    };
    let len = runs
      .iter()
      .fold(0usize, |sum, run| sum.saturating_add(run.len()));
    if len != other.len {
      return false;
    }
    let mut theirs = other.pieces(chunks).into_iter();
    let mut current: Option<Piece> = None;
    for run in runs {
      let mut run = run;
      while run.len() > 0 {
        let piece = match current.take().filter(|piece| piece.len > 0) {
          Some(piece) => piece,
          None => match theirs.next() {
            Some(piece) => piece,
            None => return false,
          },
        };
        let step = run.len().min(piece.len);
        if !Value::same((chunks, blocks), run, piece, (step, &post_state)) {
          return false;
        }
        run = run.after(step);
        current = Some(piece.after(step));
      }
    }
    true
  }

  /// Whether the first `step` bytes of `run` are those of `piece`: equal unread when both name the same identities.
  fn same(
    (chunks, blocks): (&Chunks, &Blocks),
    run: Applied,
    piece: Piece,
    (step, post_state): (usize, &crate::named::Post<'_>),
  ) -> bool {
    let theirs = || Value::piece_bytes(chunks, blocks, Piece { len: step, ..piece });
    match run {
      Applied::Shared(mine) => {
        (mine.anchor == piece.anchor && mine.identity == piece.identity)
          || Value::piece_bytes(chunks, blocks, Piece { len: step, ..mine }) == theirs()
      }
      Applied::Post { src, .. } => {
        let added = post_state.span(
          u64::try_from(src).unwrap_or(u64::MAX),
          u64::try_from(step).unwrap_or(u64::MAX),
        );
        added.len() == step && *added == *theirs()
      }
    }
  }

  /// The runs the engine's `apply` writes when it applies `ops` to this value over a post-state of `post_len` bytes:
  /// runs of this value it keeps and runs of the post-state it adds, each clamped as `apply` clamps them. `None` when
  /// an offset does not fit a `usize`.
  fn applied_runs(
    pieces: &[Piece],
    len: usize,
    ops: &[Op],
    post_len: usize,
  ) -> Option<Vec<Applied>> {
    let base_len = u64::try_from(len).ok()?;
    let mut runs = Vec::new();
    let mut pos = 0u64;
    for op in ops {
      if op.at > pos {
        Value::share(pieces, (pos.min(base_len), op.at.min(base_len)), &mut runs)?;
        pos = op.at;
      }
      match op.kind {
        OpKind::Delete => pos = pos.saturating_add(op.len),
        OpKind::Truncate => pos = base_len,
        OpKind::Overwrite => {
          runs.push(Applied::post(op, post_len)?);
          pos = pos.saturating_add(op.len);
        }
        OpKind::Insert | OpKind::Extend => runs.push(Applied::post(op, post_len)?),
        _ => {}
      }
    }
    if pos < base_len {
      Value::share(pieces, (pos, base_len), &mut runs)?;
    }
    runs.retain(|run| run.len() > 0);
    Some(runs)
  }

  /// Pushes the runs of `pieces` covering `[start, end)` of the value.
  fn share(pieces: &[Piece], (start, end): (u64, u64), runs: &mut Vec<Applied>) -> Option<()> {
    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?;
    let mut logical = 0usize;
    for piece in pieces {
      let piece_start = logical;
      let piece_end = logical.saturating_add(piece.len);
      logical = piece_end;
      if piece_end <= start || piece_start >= end {
        continue;
      }
      let from = start.max(piece_start);
      let to = end.min(piece_end);
      let cut = piece.after(from.saturating_sub(piece_start));
      runs.push(Applied::Shared(Piece {
        len: to.saturating_sub(from),
        ..cut
      }));
    }
    Some(())
  }

  /// Gives the value back: its tree's hold, its place among its anchor's live values (retiring what no live value reads
  /// any more), and its anchor with its last value.
  pub(crate) fn release(self, chunks: &mut Chunks, blocks: &mut Blocks) {
    chunks.trees.release(self.tree);
    let Some(slot) = self.anchor else {
      return;
    };
    let Some(mut anchor) = chunks.anchors.get_mut(slot).and_then(Option::take) else {
      return;
    };
    if let Some(count) = anchor.live.get_mut(&self.index) {
      *count = count.saturating_sub(1);
      if *count == 0 {
        anchor.live.remove(&self.index);
      }
    }
    let floor = anchor.live.keys().next().copied().unwrap_or(u64::MAX);
    let before = anchor.places.bytes();
    anchor
      .places
      .retire_through(floor, &mut |chunk| chunks.release(chunk, blocks));
    chunks.places = chunks
      .places
      .saturating_sub(before)
      .saturating_add(anchor.places.bytes());
    if anchor.live.is_empty() {
      chunks.places = chunks.places.saturating_sub(anchor.places.bytes());
      chunks.release(anchor.current, blocks);
      chunks.anchors_free.push(slot);
      return;
    }
    if let Some(place) = chunks.anchors.get_mut(slot) {
      *place = Some(anchor);
    }
  }

  /// Marks this value superseded by a later change: its block, which the map has let go, is no longer the current
  /// file's, so the bytes it names are history from here (A-728 part 4).
  pub(crate) fn supersede(&mut self) {
    self.own = None;
  }
}

/// What a change did ([`change`]).
#[derive(Debug)]
pub(crate) struct Changed {
  /// The new latest value.
  pub(crate) value: Value,
  /// The bytes the change copied out of the block it superseded: what it removed.
  pub(crate) stored: usize,
  /// The tree nodes the change made.
  pub(crate) nodes: u64,
}

/// The parts the engine's `segments` make of a change by `ops` to a value of `len` bytes, its added runs by length;
/// `None` when they do not add up to `next_len` (an op whose post-state was shorter than declared, clamped by `apply`).
fn parts_of(len: usize, ops: &[Op], next_len: usize) -> Option<Vec<Part>> {
  use crate::engine::Segment;
  let segments = crate::engine::segments(u64::try_from(len).ok()?, ops);
  let parts: Vec<Part> = segments
    .iter()
    .map(|segment| match *segment {
      Segment::Base { start, len } => Part::Kept { from: start, len },
      Segment::Post { src, len } => Part::Added(Run {
        source: 0,
        at: src,
        len,
      }),
    })
    .collect();
  let total = parts
    .iter()
    .map(|part| match part {
      Part::Kept { len, .. } => *len,
      Part::Added(run) => run.len,
    })
    .try_fold(0u64, u64::checked_add)?;
  (total == u64::try_from(next_len).ok()?).then_some(parts)
}

/// The change `ops` made of `latest` (its path's latest value, superseded), the file's new bytes in `block` (module
/// docs, "A change"): the new value, its anchor's newest bytes the new block, one holder of it taken; the ranges the
/// change removed copied out of the block it superseded. Walks no other value. `None`, with nothing changed, when
/// `latest` is not its anchor's latest or the ops do not add up to the new block: the caller names the new block whole.
pub(crate) fn change(
  latest: &mut Value,
  chunks: &mut Chunks,
  blocks: &mut Blocks,
  (block, ops): (Block, &[Op]),
) -> Option<Changed> {
  let slot = latest.anchor?;
  let next_len = blocks.bytes(block).len();
  let parts = parts_of(latest.len, ops, next_len)?;
  let mut anchor = chunks.anchors.get_mut(slot).and_then(Option::take)?;
  if anchor.changes != latest.index {
    if let Some(place) = chunks.anchors.get_mut(slot) {
      *place = Some(anchor);
    }
    return None;
  }
  let index = anchor.changes.saturating_add(1);
  let old = anchor.current;
  let whole = latest.len;
  let mut stored = 0usize;
  let before = anchor.places.bytes();
  let made = anchor.places.change(index, &parts, &mut |at, len| {
    stored = stored.saturating_add(usize::try_from(len).unwrap_or(usize::MAX));
    if at == 0 && usize::try_from(len).ok() == Some(whole) {
      chunks.retain(old);
      return Ok((old, 0));
    }
    let bytes = chunks.chunk_bytes(blocks, (old, at, len)).to_vec();
    let chunk = chunks.add_source(Source::Owned(bytes));
    chunks.retain(chunk);
    Ok((chunk, 0))
  });
  chunks.places = chunks
    .places
    .saturating_sub(before)
    .saturating_add(anchor.places.bytes());
  let live_before = chunks.trees.live();
  let tree = made.and_then(|made| chunks.trees.rebuild(&latest.tree, &made));
  let tree = match tree {
    Ok(tree) => tree,
    Err(_) => {
      if let Some(place) = chunks.anchors.get_mut(slot) {
        *place = Some(anchor);
      }
      return None;
    }
  };
  let nodes = u64::try_from(chunks.trees.live().saturating_sub(live_before)).unwrap_or(u64::MAX);
  chunks.made = chunks.made.saturating_add(nodes);
  blocks.hold(block);
  let current = chunks.add_source(Source::Block {
    block,
    len: next_len,
  });
  chunks.retain(current);
  anchor.current = current;
  anchor.changes = index;
  let live = anchor.live.entry(index).or_default();
  *live = live.saturating_add(1);
  if let Some(place) = chunks.anchors.get_mut(slot) {
    *place = Some(anchor);
  }
  chunks.release(old, blocks);
  latest.own = None;
  let structure = chunks.structure_of(&tree, slot);
  Some(Changed {
    value: Value {
      tree,
      len: next_len,
      anchor: Some(slot),
      index,
      own: Some(next_len),
      structure,
    },
    stored,
    nodes,
  })
}

/// The most a change by `ops` ops of a value of `len` bytes can add to what the history holds, without making it: what
/// it removes (at most the value) and the structure its parts make (`crate::piece_tree::rebuild_bound`,
/// `crate::byte_places::change_bound`), for admission (`Green::history_reservation`).
pub(crate) fn change_bound(len: usize, ops: usize) -> usize {
  // A change's parts: each op adds at most one run and splits at most one kept range (`segments`).
  let parts = ops.saturating_mul(2).saturating_add(1);
  let runs = u64::try_from(len).unwrap_or(u64::MAX);
  len
    .saturating_add(crate::piece_tree::rebuild_bound(parts, runs).saturating_mul(NODE_BYTES))
    .saturating_add(BytePlaces::change_bound(parts))
}

#[cfg(test)]
mod tests;
