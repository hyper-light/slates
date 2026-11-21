//! Today's content history before A-818, kept verbatim as the reference the new one is held to (main's ruling of
//! 2026-11-21: "the oracle holds every version's reads equal to today's reanchor on generated histories"). Test-only:
//! nothing outside the oracle in `crate::history`'s tests names it. Its own module docs follow, unchanged.
#![allow(dead_code)]

//! A green's content history, copy-on-write (W2; §4.16 "Splice": "splices accepted ops into a new version by
//! extent-list surgery without copying bytes"): each version of a file is a list of pieces, each a run of bytes in an
//! immutable chunk, so a version that changes a few bytes of a file shares every unchanged run with the version
//! before it and holds only the bytes it added.
//!
//! **Why.** The engine kept a whole copy of a file's new value in its history at every version that changed it, and
//! admission charged the whole previous file to each submit (`Green::history_reservation`), so a one-byte overwrite
//! of a large file retained the file. Behind a work that holds the reachable floor, the retention cap then refused
//! submits after cap / file size versions however little each changed: 2,410 to 2,456 `BudgetExceeded` in every
//! 3,000-client rung over `kernel/` (2026-11-07, the Studio).
//!
//! **What it holds.** [`Chunks`] is an arena of immutable byte runs with a reference count each (handles and counts,
//! no shared-ownership pointer, CLAUDE.md R2); a chunk is freed when the last piece naming it goes. A [`Value`] is the
//! pieces and the length. A path's latest value is its file's current block, named whole; its earlier values are pieces
//! of that block where they still hold its bytes and of small owned chunks where they do not ([`reanchor`]).
//!
//! **The bound.** A value whose piece list would cost more than its bytes is stored as one owned chunk instead
//! (compaction, at each re-anchor): a version never costs more than a whole copy, which is what every version cost
//! before, and a file edited in many places is rewritten once its pieces outweigh it. No constant: the rule is the two
//! sizes.
//!
//! **A chunk may be a file's block (A-728, step S2 of A-723's content-once plan; D-28, §4.11).** A value stored whole
//! (a path's first change, the bytes it held before its first recorded change, a checkpoint's restored file, and every
//! change that shares nothing with the value before it: each push and import replaces files whole) names the file's
//! own block of [`crate::dims::Blocks`] as its one chunk, holding one holder of it, instead of copying the file's bytes
//! into a chunk of its own. The history's latest value then is the file's block, and the green holds the file once. When
//! a later version replaces the file, the map lets the block go and the history's hold keeps it, now as the earlier
//! version's bytes: nothing is copied either way. Before, the history copied every byte a change added beside the file:
//! a Subversion import of 1,000 revisions held 234.8 MiB of chunks beside about 585 MB of blocks (A-723).
//!
//! **Anchored on the newest block (A-793, replacing A-728 part 2's splice and seal).** A change to a file makes its
//! new block; [`reanchor`] walks the change's ops over the value it supersedes, as the engine's `apply` does, to find
//! the runs the change kept and where each now lies in the new block, checked against the new block byte for byte (a
//! change whose ops do not reproduce it, an op whose post-state was shorter than it declared, which `apply` clamps, is
//! treated as a whole replace). Every piece of every earlier value of the path that lies in a kept run is moved to the
//! same bytes in the new block; what is left of each earlier block is the bytes the change overwrote or removed, which
//! the earlier versions still read, and a block only this history holds is cut to them (copied into one owned chunk)
//! and let go. So a 64-byte overwrite of a 16 KiB file retains 64 bytes and a few pieces, not the file.
//!
//! *Why.* A-728 part 2 anchored the chain the other way: the first value named the earlier block whole and each later
//! one shared runs of it, so every file a change touched kept its earlier block while any reader held the floor. At
//! 100,000 clients over the Linux kernel that was about 73 KB of retention for each accepted version, which wrote 256
//! bytes, and the green's cap (373 MB) refused 94,877 of the 100,000 submits `BudgetExceeded` after 5,123 accepts
//! (goal 86b, 2026-11-20, the Studio). *Cost.* A change re-anchors every value its path's history holds, its pieces in
//! the kept runs; that is the path's own history since the floor, which the verdict's `intervening` already walks.
//! *Kept as before.* A whole replace keeps nothing of the earlier value, so the earlier block stays as that version's
//! bytes; a block another holder keeps (a branch, the identity table) or another path's value names is never cut.
//!
//! **A layered anchor (A-809, fixing A-803; main's order of 2026-11-21 02:37, w-fed-move's option 1).** The anchor is
//! re-anchored in full only by a change that moves bytes (an insert, a delete, a truncate or an extend), and by one that
//! keeps nothing or changes nothing. A change that only overwrites bytes inside the file keeps the path's anchor chunk
//! and changes what it names: the bytes it overwrote, as they were, are copied into one owned chunk as the anchor's
//! next *layer* (numbered, found by an interval index on their offsets), the anchor names the new block, the old block
//! is let go, and the superseded latest value's one piece reads the anchor at that layer's number (its *epoch*). No
//! other value is walked: O(log + the bytes overwritten), where the full re-anchor walked every value of the path's
//! history, which grew with history while a reader pinned the floor (A-803: 7.9 to 231 values a verdict over 10^5
//! versions).
//! - *Reading.* A piece at epoch `e` of an anchor reads the anchor's bytes overlaid by its layers numbered `e` or later,
//!   the lowest number covering a byte giving it (the bytes as they were when its value was its path's latest). A piece
//!   at [`CURRENT`] reads the chunk as it is; every reader resolves a value's pieces to concrete runs first
//!   ([`Value::concrete`]), O(log + the layers over each piece).
//! - *Epochs.* A value that becomes history reads its anchor at the anchor's next layer number: the superseded latest
//!   at the layer the change makes, and every value a full re-anchor moves to a new anchor at 0. Only the latest value
//!   reads its own anchor at `CURRENT`, so swapping the anchor's block changes only what history reads through layers.
//! - *A full re-anchor* first resolves every value's layered pieces into concrete runs naming the anchor and the
//!   layers' owned chunks ([`flatten`]), then runs as before; the layers no piece reads any more go as the pieces do.
//! - *Bounded.* A layer numbered below the least epoch any piece reads its anchor at is read by none and dropped
//!   ([`Chunks::prune`]), so folding the values that read it frees it; an anchor freed frees its layers. A layer costs
//!   its bytes (owned, as A-793's cut copied them) and its entries ([`LAYER_BYTES`], counted with the chunks, as a
//!   value's list is).
//! - *An SDK edit's replacement* (a delete and an insert of one length at one offset) is an overwrite there and takes
//!   the layered path (A-811); a read takes each start's layers in number order only while they reach further, so a
//!   range overwritten m times at one offset reads through one layer ([`Layers::candidates`], A-811).
//! - *A block another holder keeps* (a branch's map, the identity table) is not named by a layer: the change copies
//!   what it overwrote and lets its hold go, so the history holds the overwritten bytes whoever else holds the block
//!   (the full re-anchor keeps such a block whole).
//! - *The oracle* is the full re-anchor itself: `engine::history_block_tests`'
//!   `the_layered_anchor_reads_as_the_full_reanchor_through_folds_and_checkpoints` runs generated histories on a green
//!   taking layers and a twin taking only the full re-anchor (`FULL_REANCHOR_ONLY`, a test's switch).
//!
//! **Lifetimes.** The blocks live in the files' arena, which a lineage's greens share and lend ([`crate::dims`], "Arenas
//! are lent"), while the chunks are the green's own. A lineage member is read and written only while lent, its arenas
//! then the lineage's, so a block a chunk names is always in the arena at hand; a green made at another's version starts
//! with no history; a dropped value lets its block go into the same arena; and a retired green gives its chunks' holds
//! back a chunk a step ([`Chunks::release_step`]). Every function that reads a value's bytes takes the blocks with the
//! chunks.

use crate::dims::{Block, Blocks};
use crate::ops_doc::{Op, OpKind};

/// A run of a value's bytes: `len` bytes at `at` within chunk `chunk`, read as of `epoch` (module docs, "A layered
/// anchor"): [`CURRENT`] reads the chunk's bytes as they are; an earlier epoch of an anchor chunk reads them as they
/// were before the layers made since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Piece {
  /// The chunk.
  chunk: usize,
  /// The run's offset within the chunk.
  at: usize,
  /// The run's length.
  len: usize,
  /// The first of its anchor's layers it reads through; [`CURRENT`] for none.
  epoch: u32,
}

/// Format: the epoch of a piece that reads its chunk as it is, through no layer (module docs, "A layered anchor").
const CURRENT: u32 = u32::MAX;

/// One length-preserving change's bytes of an anchor (module docs, "A layered anchor"): the `len` bytes the change
/// overwrote at its start offset (the key it is held under), as they were, kept at `at` in the owned chunk `chunk`.
#[derive(Clone, Copy, Debug)]
struct Layer {
  /// Its length.
  len: usize,
  /// The owned chunk holding the bytes as they were.
  chunk: usize,
  /// Their offset in it.
  at: usize,
}

/// An anchor chunk's layers (module docs, "A layered anchor"), found by offset through an interval index: by start
/// offset and layer number, the longest layer's length bounding how far before a range a query starts.
#[derive(Debug, Default)]
struct Layers {
  /// Each layer, by (start offset in the anchor, layer number).
  by_start: std::collections::BTreeMap<(usize, u32), Layer>,
  /// Each layer number's start offsets: what a prune drops, oldest first.
  by_number: std::collections::BTreeMap<u32, Vec<usize>>,
  /// The longest layer held: a query for `[from, to)` scans the starts from `from` less this.
  longest: usize,
  /// The longest layer at each start offset: a read takes a start's layers in number order only until one reaches it.
  start_longest: std::collections::BTreeMap<usize, usize>,
  /// The next layer's number.
  next: u32,
  /// How many pieces name the anchor at each epoch below [`CURRENT`]: a layer numbered below the least is read by none.
  epochs: std::collections::BTreeMap<u32, usize>,
}

impl Layers {
  /// The layers that may give a byte of `[start, end)` to a piece read at `epoch`, with how many were examined: each
  /// start offset's layers from `epoch` on, in number order, kept only while each reaches further than those before it
  /// (a byte at a start is given by the lowest-numbered layer there that covers it, so a layer no longer than an earlier
  /// one gives nothing), a start's walk ending at its longest layer. A range overwritten m times at one offset is read
  /// through one layer, not m (A-809's leftover (a)). O(the starts over the range × log).
  fn candidates(
    &self,
    epoch: u32,
    (start, end): (usize, usize),
  ) -> (Vec<(u32, usize, Layer)>, u64) {
    let mut found = Vec::new();
    let mut examined = 0u64;
    let mut cursor = start.saturating_sub(self.longest);
    while let Some(at) = self
      .by_start
      .range((cursor, 0)..(end, 0))
      .next()
      .map(|((at, _), _)| *at)
    {
      let longest_here = self.start_longest.get(&at).copied().unwrap_or(usize::MAX);
      let mut reached = 0usize;
      for (&(_, number), layer) in self.by_start.range((at, epoch)..=(at, CURRENT)) {
        examined = examined.saturating_add(1);
        if layer.len > reached {
          if at.saturating_add(layer.len) > start {
            found.push((number, at, *layer));
          }
          reached = layer.len;
        }
        if reached >= longest_here {
          break;
        }
      }
      let Some(next) = at.checked_add(1) else {
        break;
      };
      cursor = next;
    }
    (found, examined)
  }
}

/// Format: the bytes one layer's entries cost (its record and its keys in both indexes), counted with the chunks.
const LAYER_BYTES: usize = size_of::<Layer>() + 2 * size_of::<(usize, u32)>();

/// Format: the bytes one piece costs in a value's list, the measure compaction weighs against the value's bytes.
pub(crate) const PIECE_BYTES: usize = size_of::<Piece>();

/// Where a chunk's bytes are (module docs, "A chunk may be a file's block").
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

/// The arena of immutable chunks a green's history values share (module docs).
#[derive(Debug, Default)]
pub(crate) struct Chunks {
  /// Each slot's source and the pieces naming it; `None` once freed.
  slots: Vec<Option<(Source, usize)>>,
  /// Freed slots, reused first.
  free: Vec<usize>,
  /// The bytes the live chunks hold, owned or named.
  bytes: usize,
  /// The bytes of the live chunks that are files' blocks (A-728).
  named: usize,
  /// Each anchor chunk's layers (module docs, "A layered anchor"), by chunk.
  layers: std::collections::BTreeMap<usize, Layers>,
  /// The bytes the layers' entries cost ([`LAYER_BYTES`] each): retained, as a value's list is, never owned.
  entries: usize,
  /// The layers reads have examined, in all: the non-vacuity counter of a read's cost through layers.
  examined: std::cell::Cell<u64>,
}

impl Chunks {
  /// The layers reads have examined, in all ([`Chunks::resolve_into`]).
  pub(crate) fn layers_examined(&self) -> u64 {
    self.examined.get()
  }

  /// The bytes the live chunks hold, owned or named, and the layers' entries.
  pub(crate) fn bytes(&self) -> usize {
    self.bytes.saturating_add(self.entries)
  }

  /// The bytes the live chunks own: what the history copied (A-728: none for a value stored whole).
  pub(crate) fn owned_bytes(&self) -> usize {
    self.bytes.saturating_sub(self.named)
  }

  /// One chunk of a retired green's history given back: its slot freed and, for a file's block, the block's holder let
  /// go (module docs, "Lifetimes"). False once no chunk is left. Every value naming the chunks is dropped with the green.
  pub(crate) fn release_step(&mut self, blocks: &mut Blocks) -> bool {
    let Some(slot) = self.slots.pop() else {
      self.free.clear();
      self.layers.clear();
      self.entries = 0;
      self.bytes = 0;
      self.named = 0;
      return false;
    };
    if let Some((source, _)) = slot {
      source.let_go(blocks);
    }
    true
  }

  /// Whether no chunk is left to give back ([`Chunks::release_step`]).
  pub(crate) fn is_released(&self) -> bool {
    self.slots.is_empty()
  }

  /// The bytes of the distinct chunks `values` name, counted from the values themselves: the recount the arena's
  /// running total is held to (a chunk no value names is a leak, a value naming a freed chunk a double release).
  pub(crate) fn referenced_bytes<'a>(&self, values: impl Iterator<Item = &'a Value>) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
      for piece in &value.pieces {
        seen.insert(piece.chunk);
      }
    }
    // An anchor's layers are held by the anchor: their owned chunks and their entries.
    let mut entries = 0usize;
    let anchors: Vec<usize> = seen.iter().copied().collect();
    for anchor in anchors {
      if let Some(layers) = self.layers.get(&anchor) {
        entries = entries.saturating_add(layers.by_start.len());
        seen.extend(layers.by_start.values().map(|layer| layer.chunk));
      }
    }
    seen
      .into_iter()
      .filter_map(|chunk| self.slots.get(chunk).and_then(Option::as_ref))
      .map(|(source, _)| source.len())
      .sum::<usize>()
      .saturating_add(entries.saturating_mul(LAYER_BYTES))
  }

  /// Adds a chunk holding `bytes`, named by no piece yet.
  fn add(&mut self, bytes: Vec<u8>) -> usize {
    self.add_source(Source::Owned(bytes))
  }

  /// Adds a chunk from `source`, named by no piece yet.
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

  /// One more piece names `chunk`.
  fn retain(&mut self, chunk: usize) {
    if let Some(Some((_, refs))) = self.slots.get_mut(chunk) {
      *refs = refs.saturating_add(1);
    }
  }

  /// One piece naming `chunk` is gone; the chunk is freed with its last, a block's holder let go with it.
  fn release(&mut self, chunk: usize, blocks: &mut Blocks) {
    let Some(slot) = self.slots.get_mut(chunk) else {
      return;
    };
    let last = match slot {
      Some((_, refs)) => {
        *refs = refs.saturating_sub(1);
        *refs == 0
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
      // An anchor goes with its layers: their holds on their owned chunks given back.
      if let Some(layers) = self.layers.remove(&chunk) {
        self.drop_layers(layers, blocks);
      }
    }
  }

  /// Gives back what `layers` hold: each entry's hold on its owned chunk, and the entries' count.
  fn drop_layers(&mut self, layers: Layers, blocks: &mut Blocks) {
    for layer in layers.by_start.values() {
      self.entries = self.entries.saturating_sub(LAYER_BYTES);
      self.release(layer.chunk, blocks);
    }
  }

  /// One more piece names `piece`'s chunk at its epoch.
  fn retain_piece(&mut self, piece: &Piece) {
    self.retain(piece.chunk);
    if piece.epoch != CURRENT {
      let count = self
        .layers
        .entry(piece.chunk)
        .or_default()
        .epochs
        .entry(piece.epoch)
        .or_default();
      *count = count.saturating_add(1);
    }
  }

  /// `piece` no longer names its chunk: its epoch's count falls, the layers no piece reads any more are dropped
  /// ([`Chunks::prune`]), and the chunk is released.
  fn release_piece(&mut self, piece: &Piece, blocks: &mut Blocks) {
    if piece.epoch != CURRENT
      && let Some(layers) = self.layers.get_mut(&piece.chunk)
      && let Some(count) = layers.epochs.get_mut(&piece.epoch)
    {
      *count = count.saturating_sub(1);
      if *count == 0 {
        layers.epochs.remove(&piece.epoch);
      }
      self.prune(piece.chunk, blocks);
    }
    self.release(piece.chunk, blocks);
  }

  /// Drops `anchor`'s layers numbered below the least epoch a piece reads it at: read by none. With no piece reading
  /// it at an earlier epoch, every layer goes and the numbering starts again.
  fn prune(&mut self, anchor: usize, blocks: &mut Blocks) {
    let Some(layers) = self.layers.get_mut(&anchor) else {
      return;
    };
    let least = layers.epochs.keys().next().copied().unwrap_or(CURRENT);
    let mut gone = Vec::new();
    while let Some(entry) = layers.by_number.first_entry() {
      if *entry.key() >= least {
        break;
      }
      let (number, starts) = entry.remove_entry();
      for start in starts {
        if let Some(layer) = layers.by_start.remove(&(start, number)) {
          gone.push(layer.chunk);
        }
        if layers
          .by_start
          .range((start, 0)..=(start, CURRENT))
          .next()
          .is_none()
        {
          layers.start_longest.remove(&start);
        }
      }
    }
    if layers.by_start.is_empty() {
      layers.longest = 0;
      layers.start_longest.clear();
      if layers.epochs.is_empty() {
        layers.next = 0;
      }
    }
    for chunk in gone {
      self.entries = self.entries.saturating_sub(LAYER_BYTES);
      self.release(chunk, blocks);
    }
  }

  /// Whether `piece` reads its anchor through a layer: one numbered at or past its epoch exists.
  fn layered(&self, piece: &Piece) -> bool {
    piece.epoch != CURRENT
      && self
        .layers
        .get(&piece.chunk)
        .is_some_and(|layers| layers.next > piece.epoch && !layers.by_start.is_empty())
  }

  /// The concrete pieces `piece` reads, appended to `out` in order: itself where it reads its chunk as it is; else its
  /// anchor's bytes as they are, overlaid by the layers numbered at or past its epoch, the oldest covering a byte
  /// giving it (the bytes as they were when the value it belongs to was its path's latest). O(log + the layers over its
  /// range).
  fn resolve_into(&self, piece: &Piece, out: &mut Vec<Piece>) {
    let layers = if self.layered(piece) {
      self.layers.get(&piece.chunk)
    } else {
      None
    };
    let Some(layers) = layers else {
      out.push(Piece {
        epoch: CURRENT,
        ..*piece
      });
      return;
    };
    let start = piece.at;
    let end = piece.at.saturating_add(piece.len);
    let (mut found, examined) = layers.candidates(piece.epoch, (start, end));
    self
      .examined
      .set(self.examined.get().saturating_add(examined));
    found.sort_unstable_by_key(|(number, _, _)| *number);
    let mut uncovered = vec![(start, end)];
    let mut painted: Vec<(usize, Piece)> = Vec::new();
    for (_, at, layer) in found {
      let layer_end = at.saturating_add(layer.len);
      let mut left = Vec::with_capacity(uncovered.len());
      for (lo, hi) in uncovered {
        let a = lo.max(at);
        let b = hi.min(layer_end);
        if a < b {
          painted.push((
            a,
            Piece {
              chunk: layer.chunk,
              at: layer.at.saturating_add(a.saturating_sub(at)),
              len: b.saturating_sub(a),
              epoch: CURRENT,
            },
          ));
          if lo < a {
            left.push((lo, a));
          }
          if b < hi {
            left.push((b, hi));
          }
        } else {
          left.push((lo, hi));
        }
      }
      uncovered = left;
      if uncovered.is_empty() {
        break;
      }
    }
    for (lo, hi) in uncovered {
      painted.push((
        lo,
        Piece {
          chunk: piece.chunk,
          at: lo,
          len: hi.saturating_sub(lo),
          epoch: CURRENT,
        },
      ));
    }
    painted.sort_unstable_by_key(|(at, _)| *at);
    out.extend(painted.into_iter().map(|(_, piece)| piece));
  }

  /// The bytes of `piece`, or none if its chunk is gone (never, while a value names it).
  fn bytes_of<'a>(&'a self, blocks: &'a Blocks, piece: &Piece) -> &'a [u8] {
    self
      .slots
      .get(piece.chunk)
      .and_then(Option::as_ref)
      .and_then(|(source, _)| {
        source
          .bytes(blocks)
          .get(piece.at..piece.at.saturating_add(piece.len))
      })
      .unwrap_or_default()
  }
}

/// One version of a file in the history: its pieces and its length (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Value {
  /// The runs whose concatenation is the value, in order.
  pieces: Vec<Piece>,
  /// The value's length.
  len: usize,
  /// While the value is its path's latest, the chunk naming the file's current block and that block's length: the
  /// bytes the value shares with the current file, counted as current, not history (A-728 part 4). Cleared when a
  /// change supersedes it ([`Value::supersede`]).
  own: Option<(usize, usize)>,
}

/// A run of the value being built: one of the previous value's, or bytes of `next` the ops added.
enum Pending {
  /// A run of the previous value, shared.
  Shared(Piece),
  /// `len` bytes of `next` at `at`, to be copied into the new chunk.
  Added { at: usize, len: usize },
}

/// A run of the superseded value a change kept: its `len` bytes at `at` in `chunk` are the new block's at `out`
/// ([`reanchor`]).
#[derive(Clone, Copy)]
struct Kept {
  /// The chunk the kept run lies in.
  chunk: usize,
  /// Its offset in the chunk.
  at: usize,
  /// Its length.
  len: usize,
  /// Where the same bytes lie in the new block.
  out: usize,
}

/// The kept runs of a re-anchor, looked up by chunk and offset ([`Value::remapped`]). Sorted once by (chunk, offset) and
/// binary-searched when no two runs of one chunk overlap (the common case), so remapping a value of n pieces against k
/// runs costs O((n + k) log k). Until 2026-11-21 every piece scanned every run, O(n k): a refined re-indent of a 2 MiB
/// file (92,354 ops) re-anchored in over 3.5 s of one step in a debug build, and the shard's watchdog ended the daemon
/// (A-787 part 2's red; 10,000 ops 0.4 s, 40,000 ops 22.5 s). Runs that overlap are looked up by the scan, which picks
/// the first in order, so the result is the scan's either way.
struct KeptIndex<'k> {
  kept: &'k [Kept],
  /// `kept`'s indices sorted by (chunk, offset); empty when runs overlap (then the scan).
  order: Vec<usize>,
}

impl<'k> KeptIndex<'k> {
  fn new(kept: &'k [Kept]) -> KeptIndex<'k> {
    let mut order: Vec<usize> = (0..kept.len()).collect();
    order.sort_by_key(|index| kept.get(*index).map(|run| (run.chunk, run.at)));
    let disjoint = order.windows(2).all(|pair| {
      match (
        pair.first().and_then(|a| kept.get(*a)),
        pair.get(1).and_then(|b| kept.get(*b)),
      ) {
        (Some(a), Some(b)) => a.chunk != b.chunk || a.at.saturating_add(a.len) <= b.at,
        _ => true,
      }
    });
    if !disjoint {
      order.clear();
    }
    KeptIndex { kept, order }
  }

  fn run(&self, position: usize) -> Option<&'k Kept> {
    self
      .order
      .get(position)
      .and_then(|index| self.kept.get(*index))
  }

  /// The position in `order` of the first run after (chunk, cursor).
  fn after(&self, chunk: usize, cursor: usize) -> usize {
    self.order.partition_point(|index| {
      self
        .kept
        .get(*index)
        .is_some_and(|run| (run.chunk, run.at) <= (chunk, cursor))
    })
  }

  /// The run of `chunk` covering offset `cursor`, the first in `kept`'s order.
  fn covering(&self, chunk: usize, cursor: usize) -> Option<&'k Kept> {
    if self.order.is_empty() {
      return self.kept.iter().find(|run| {
        run.chunk == chunk && run.at <= cursor && cursor < run.at.saturating_add(run.len)
      });
    }
    let before = self.after(chunk, cursor).checked_sub(1)?;
    self
      .run(before)
      .filter(|run| run.chunk == chunk && cursor < run.at.saturating_add(run.len))
  }

  /// The least offset of a run of `chunk` past `cursor`.
  fn next_start(&self, chunk: usize, cursor: usize) -> Option<usize> {
    if self.order.is_empty() {
      return self
        .kept
        .iter()
        .filter(|run| run.chunk == chunk && run.at > cursor)
        .map(|run| run.at)
        .min();
    }
    self
      .run(self.after(chunk, cursor))
      .filter(|run| run.chunk == chunk)
      .map(|run| run.at)
  }
}

/// What a re-anchor did ([`reanchor`]): the path's new latest value, the earlier values' piece lists before and after
/// (the engine's running totals move by the difference), the superseded latest value's list before and after (counted
/// as current until the new value replaces it), and the blocks it let go with the bytes it copied out of them.
pub(crate) struct Reanchored {
  /// The new latest value: the file's new block, named whole.
  pub(crate) value: Value,
  /// Every earlier value's piece list bytes, before and after.
  pub(crate) lists: (usize, usize),
  /// The superseded latest value's piece list bytes, before and after.
  pub(crate) latest_list: (usize, usize),
  /// Earlier blocks let go, their kept runs now the new block's.
  pub(crate) blocks_cut: u64,
  /// The bytes copied out of those blocks: the runs the change overwrote or removed, which earlier versions still read.
  pub(crate) bytes_cut: usize,
  /// The values the re-anchor walked: the path's history (A-803's measure of what a change costs as history grows).
  pub(crate) values: usize,
}

/// A run of the bytes the engine's `apply` writes ([`Value::applied_runs`]): a run of the value it keeps, or bytes of the
/// increment's post-state.
#[derive(Clone, Copy)]
enum Applied {
  /// A run of the value, by its piece.
  Shared(Piece),
  /// `len` bytes of the post-state at `src`.
  Post { src: usize, len: usize },
}

impl Applied {
  /// A kept run of the value, from what [`Value::share`] collected.
  fn of_shared(run: Pending) -> Option<Applied> {
    match run {
      Pending::Shared(piece) => Some(Applied::Shared(piece)),
      Pending::Added { .. } => None,
    }
  }

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
      Applied::Shared(piece) => Applied::Shared(Piece {
        chunk: piece.chunk,
        at: piece.at.saturating_add(step),
        len: piece.len.saturating_sub(step),
        epoch: piece.epoch,
      }),
      Applied::Post { src, len } => Applied::Post {
        src: src.saturating_add(step),
        len: len.saturating_sub(step),
      },
    }
  }
}

impl Value {
  /// The value's length.
  pub(crate) fn len(&self) -> usize {
    self.len
  }

  /// The value's pieces as concrete runs (module docs, "A layered anchor"): as they are where none reads an anchor
  /// through its layers, else each such piece resolved ([`Chunks::resolve_into`]). What every reader of its bytes walks.
  fn concrete<'v>(&'v self, chunks: &Chunks) -> std::borrow::Cow<'v, [Piece]> {
    let layered = self.pieces.iter().any(|piece| chunks.layered(piece));
    if !layered {
      return std::borrow::Cow::Borrowed(&self.pieces);
    }
    let mut out = Vec::with_capacity(self.pieces.len());
    for piece in &self.pieces {
      chunks.resolve_into(piece, &mut out);
    }
    std::borrow::Cow::Owned(out)
  }

  /// The bytes its piece list costs.
  pub(crate) fn list_bytes(&self) -> usize {
    self.pieces.len().saturating_mul(PIECE_BYTES)
  }

  /// What [`Value::whole`] of `len` bytes holds, without making it: its bytes and its one piece's list entry (nothing
  /// for no bytes), as the engine counts a current value (A-534: what a path's first change records whole).
  pub(crate) fn whole_cost(len: usize) -> usize {
    if len == 0 {
      0
    } else {
      len.saturating_add(PIECE_BYTES)
    }
  }

  /// A file's `block` of `len` bytes as the value's one chunk, the caller's holder of it now the chunk's (module docs,
  /// "A chunk may be a file's block"): no byte copied. The holder is let go at once for no bytes, a value of none.
  pub(crate) fn named(chunks: &mut Chunks, blocks: &mut Blocks, block: Block, len: usize) -> Value {
    if len == 0 {
      blocks.release(block);
      return Value::default();
    }
    let chunk = chunks.add_source(Source::Block { block, len });
    chunks.retain(chunk);
    Value {
      pieces: vec![Piece {
        chunk,
        at: 0,
        len,
        epoch: CURRENT,
      }],
      len,
      own: Some((chunk, len)),
    }
  }

  /// The length of the file block the value shares while it is its path's latest, the bytes counted as current (none
  /// once superseded, or for a value naming no block of its own).
  pub(crate) fn own_len(&self) -> usize {
    self.own.map_or(0, |(_, len)| len)
  }

  /// Whether the value is `block` whole, named as its one chunk (A-728).
  pub(crate) fn is_block(&self, chunks: &Chunks, block: Block) -> bool {
    match self.pieces.as_slice() {
      [piece] => matches!(
        chunks.slots.get(piece.chunk),
        Some(Some((Source::Block { block: named, len, .. }, _)))
          if *named == block && piece.at == 0 && piece.len == *len
      ),
      _ => false,
    }
  }

  /// `bytes` as one new chunk the history owns, taken without a copy (none for no bytes): a value whose pieces outweigh
  /// its bytes, stored whole ([`reanchor`]).
  fn owned(chunks: &mut Chunks, bytes: Vec<u8>) -> Value {
    let len = bytes.len();
    if len == 0 {
      return Value::default();
    }
    let chunk = chunks.add(bytes);
    chunks.retain(chunk);
    Value {
      pieces: vec![Piece {
        chunk,
        at: 0,
        len,
        epoch: CURRENT,
      }],
      len,
      own: None,
    }
  }

  /// `bytes` as one new chunk the history owns (none for no bytes).
  #[cfg(test)]
  pub(crate) fn whole(chunks: &mut Chunks, bytes: &[u8]) -> Value {
    let len = bytes.len();
    if len == 0 {
      return Value::default();
    }
    let chunk = chunks.add(bytes.to_vec());
    chunks.retain(chunk);
    Value {
      pieces: vec![Piece {
        chunk,
        at: 0,
        len,
        epoch: CURRENT,
      }],
      len,
      own: None,
    }
  }

  /// The value's bytes, copied out of its chunks.
  pub(crate) fn materialize(&self, chunks: &Chunks, blocks: &Blocks) -> Vec<u8> {
    let mut out = Vec::with_capacity(self.len);
    for piece in self.concrete(chunks).iter() {
      out.extend_from_slice(chunks.bytes_of(blocks, piece));
    }
    out
  }

  /// The value's bytes in `[from, to)` (clamped to its length), copied out of only the pieces that hold them: what a
  /// paged read of an earlier version takes, without the whole file.
  pub(crate) fn read(
    &self,
    (chunks, blocks): (&Chunks, &Blocks),
    from: usize,
    to: usize,
  ) -> Vec<u8> {
    let to = to.min(self.len);
    let mut out = Vec::with_capacity(to.saturating_sub(from));
    let mut logical = 0usize;
    for piece in self.concrete(chunks).iter() {
      let start = logical;
      let end = logical.saturating_add(piece.len);
      logical = end;
      if end <= from || start >= to {
        continue;
      }
      let run = chunks.bytes_of(blocks, piece);
      let lo = from.max(start).saturating_sub(start);
      let hi = to.min(end).saturating_sub(start);
      out.extend_from_slice(run.get(lo..hi).unwrap_or_default());
    }
    out
  }

  /// Whether the value's bytes are `bytes`, compared piece by piece without a copy.
  #[cfg(test)]
  pub(crate) fn equals(&self, (chunks, blocks): (&Chunks, &Blocks), bytes: &[u8]) -> bool {
    if self.len != bytes.len() {
      return false;
    }
    let mut at = 0usize;
    for piece in self.concrete(chunks).iter() {
      let run = chunks.bytes_of(blocks, piece);
      let end = at.saturating_add(run.len());
      if run.len() != piece.len || bytes.get(at..end) != Some(run) {
        return false;
      }
      at = end;
    }
    at == bytes.len()
  }

  /// Whether the bytes `ops` make of this value, their added bytes read from `post_state` as the engine's `apply` reads
  /// them, are `other`'s bytes: compared run against run, so a stretch both name in the same chunk at the same offset
  /// is equal unread, and only the runs the ops added and the runs the two values do not share are read. What the
  /// verdict's whole-file identity check needs (`Green::lands_on_head`): it materialized this value, applied the ops
  /// to a second copy and compared that with the head, the file's size three times over, where the two values share
  /// every run neither changed.
  pub(crate) fn applied_equals(
    &self,
    (chunks, blocks): (&Chunks, &Blocks),
    (ops, post_state): (&[Op], crate::named::Post<'_>),
    other: &Value,
  ) -> bool {
    let post_len = usize::try_from(post_state.len()).unwrap_or(usize::MAX);
    let Some(runs) = Value::applied_runs(&self.concrete(chunks), self.len, ops, post_len) else {
      return false;
    };
    let len = runs
      .iter()
      .fold(0usize, |sum, run| sum.saturating_add(run.len()));
    if len != other.len {
      return false;
    }
    let other_pieces = other.concrete(chunks);
    let mut theirs = other_pieces.iter().copied();
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
        let same = match run {
          Applied::Shared(mine) => {
            (mine.chunk == piece.chunk && mine.at == piece.at)
              || chunks.bytes_of(blocks, &Piece { len: step, ..mine })
                == chunks.bytes_of(blocks, &Piece { len: step, ..piece })
          }
          Applied::Post { src, .. } => {
            let added = post_state.span(
              u64::try_from(src).unwrap_or(u64::MAX),
              u64::try_from(step).unwrap_or(u64::MAX),
            );
            added.len() == step && *added == *chunks.bytes_of(blocks, &Piece { len: step, ..piece })
          }
        };
        if !same {
          return false;
        }
        run = run.after(step);
        current = Some(Piece {
          chunk: piece.chunk,
          at: piece.at.saturating_add(step),
          len: piece.len.saturating_sub(step),
          epoch: piece.epoch,
        });
      }
    }
    true
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
    let mut shared = Vec::new();
    let mut pos = 0u64;
    for op in ops {
      if op.at > pos {
        Value::share(
          pieces,
          pos.min(base_len),
          op.at.min(base_len),
          0,
          &mut shared,
        )?;
        pos = op.at;
      }
      runs.extend(shared.drain(..).filter_map(Applied::of_shared));
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
      Value::share(pieces, pos, base_len, 0, &mut shared)?;
    }
    runs.extend(shared.drain(..).filter_map(Applied::of_shared));
    runs.retain(|run| run.len() > 0);
    Some(runs)
  }

  /// Gives the value's chunks back: each piece's reference released, a chunk's block let go with its last.
  pub(crate) fn release(self, chunks: &mut Chunks, blocks: &mut Blocks) {
    for piece in self.pieces {
      chunks.release_piece(&piece, blocks);
    }
  }

  /// Marks this value superseded by a later change: its own block, which the map has let go, is no longer the current
  /// file's, so the bytes it names are history from here (A-728 part 4). The re-anchor that follows moves its runs to the
  /// new block and cuts what is left ([`reanchor`]); a whole replace leaves it naming its block.
  pub(crate) fn supersede(&mut self) {
    self.own = None;
  }

  /// The runs of this value that `ops` keep, each with where it lies in `next` (the file's new bytes), checked against
  /// `next` byte for byte. `None` when the ops do not reproduce `next` from this value
  /// or keep none of it (a whole replace).
  fn kept_runs(
    &self,
    (chunks, blocks): (&Chunks, &Blocks),
    ops: &[Op],
    next: &[u8],
  ) -> Option<Vec<Kept>> {
    let pending = Value::walk(&self.concrete(chunks), self.len, ops, next.len())?;
    let mut out = 0usize;
    let mut kept = Vec::new();
    for run in pending {
      match run {
        Pending::Shared(piece) => {
          let end = out.saturating_add(piece.len);
          if next.get(out..end) != Some(chunks.bytes_of(blocks, &piece)) {
            return None;
          }
          kept.push(Kept {
            chunk: piece.chunk,
            at: piece.at,
            len: piece.len,
            out,
          });
          out = end;
        }
        Pending::Added { at, len } => {
          if at != out {
            return None;
          }
          out = out.saturating_add(len);
        }
      }
    }
    (!kept.is_empty()).then_some(kept)
  }

  /// This value's pieces with every run a kept run covers moved to the same bytes in `anchor` (the new block's chunk)
  /// and the rest left where they are, in order, unmerged and unreferenced (the caller builds the value with
  /// [`Value::push`]).
  fn remapped(&self, kept: &KeptIndex<'_>, anchor: usize) -> Vec<Piece> {
    let mut out = Vec::with_capacity(self.pieces.len());
    for piece in &self.pieces {
      let end = piece.at.saturating_add(piece.len);
      let mut cursor = piece.at;
      while cursor < end {
        if let Some(run) = kept.covering(piece.chunk, cursor) {
          let stop = end.min(run.at.saturating_add(run.len));
          // History now, it reads the new anchor through every layer a later change makes (the anchor is fresh: its
          // first layer is numbered 0).
          out.push(Piece {
            chunk: anchor,
            at: run.out.saturating_add(cursor.saturating_sub(run.at)),
            len: stop.saturating_sub(cursor),
            epoch: 0,
          });
          cursor = stop;
        } else {
          let stop = kept.next_start(piece.chunk, cursor).unwrap_or(end).min(end);
          out.push(Piece {
            chunk: piece.chunk,
            at: cursor,
            len: stop.saturating_sub(cursor),
            epoch: piece.epoch,
          });
          cursor = stop;
        }
      }
    }
    out
  }

  /// This value rebuilt from `pieces`, each referenced, its own block cleared (it is no path's latest).
  fn rebuilt(chunks: &mut Chunks, pieces: Vec<Piece>) -> Value {
    let mut value = Value::default();
    for piece in pieces {
      value.push(chunks, piece);
    }
    value
  }

  /// Appends `piece`, merged into the last when it continues it in the same chunk, and takes its reference.
  fn push(&mut self, chunks: &mut Chunks, piece: Piece) {
    if piece.len == 0 {
      return;
    }
    self.len = self.len.saturating_add(piece.len);
    if let Some(last) = self.pieces.last_mut()
      && last.chunk == piece.chunk
      && last.epoch == piece.epoch
      && last.at.saturating_add(last.len) == piece.at
    {
      last.len = last.len.saturating_add(piece.len);
      return;
    }
    chunks.retain_piece(&piece);
    self.pieces.push(piece);
  }

  /// The runs the engine's `apply` would produce over this value, in order: shared runs of this value and runs of
  /// the output the ops add. `None` when the runs do not cover `next_len` exactly.
  fn walk(pieces: &[Piece], len: usize, ops: &[Op], next_len: usize) -> Option<Vec<Pending>> {
    let base_len = u64::try_from(len).ok()?;
    let mut runs = Vec::new();
    let mut pos = 0u64;
    let mut out = 0usize;
    for op in ops {
      if op.at > pos {
        let start = pos.min(base_len);
        let end = op.at.min(base_len);
        out = Value::share(pieces, start, end, out, &mut runs)?;
        pos = op.at;
      }
      match op.kind {
        OpKind::Delete => pos = pos.saturating_add(op.len),
        OpKind::Truncate => pos = base_len,
        OpKind::Overwrite => {
          out = Value::add(op.len, out, &mut runs)?;
          pos = pos.saturating_add(op.len);
        }
        OpKind::Insert | OpKind::Extend => out = Value::add(op.len, out, &mut runs)?,
        _ => {}
      }
    }
    if pos < base_len {
      out = Value::share(pieces, pos, base_len, out, &mut runs)?;
    }
    (out == next_len).then_some(runs)
  }

  /// Adds the runs of this value covering `[start, end)` as shared runs; the output length after them.
  fn share(
    pieces: &[Piece],
    start: u64,
    end: u64,
    out: usize,
    runs: &mut Vec<Pending>,
  ) -> Option<usize> {
    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?;
    if end <= start {
      return Some(out);
    }
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
      runs.push(Pending::Shared(Piece {
        chunk: piece.chunk,
        at: piece.at.saturating_add(from.saturating_sub(piece_start)),
        len: to.saturating_sub(from),
        epoch: piece.epoch,
      }));
    }
    out.checked_add(end.saturating_sub(start))
  }

  /// Adds a run of `len` bytes of the output at `out`; the output length after it.
  fn add(len: u64, out: usize, runs: &mut Vec<Pending>) -> Option<usize> {
    let len = usize::try_from(len).ok()?;
    if len > 0 {
      runs.push(Pending::Added { at: out, len });
    }
    out.checked_add(len)
  }
}

/// Re-anchors a path's history on its new block as a change lands (A-793; module docs, "Anchored on the newest
/// block"): `values` are the path's history values, its latest (the value the change supersedes) last, and `block` is
/// the file's new block, which `ops` made of the latest. Every run of every value that the change kept is moved to the
/// same bytes in the new block; every block the latest named that only this history then holds is cut to the runs the
/// values still name of it (what the change overwrote or removed), copied into one owned chunk, and let go; and a value
/// whose pieces then outweigh its bytes is stored whole. The new latest value names the new block whole, one holder of
/// it taken. `None`, with nothing changed, when the ops do not reproduce the new block from the latest or keep none of
/// it: the caller then names the new block whole and the history keeps the earlier one, as for a whole replace.
pub(crate) fn reanchor(
  mut values: Vec<&mut Value>,
  chunks: &mut Chunks,
  blocks: &mut Blocks,
  (block, ops): (Block, &[Op]),
) -> Option<Reanchored> {
  if layered_allowed()
    && let Some(overlaid) = overlay(&mut values, chunks, blocks, (block, ops))
  {
    return Some(overlaid);
  }
  let latest = values.last()?;
  let kept = latest.kept_runs((chunks, blocks), ops, blocks.bytes(block))?;
  let len = blocks.bytes(block).len();
  let latest_before = latest.list_bytes();
  let lists_before = values
    .iter()
    .map(|value| value.list_bytes())
    .fold(0usize, usize::saturating_add);
  // A change that is not length-preserving re-anchors every value in full, as before layers: their anchors' layers
  // resolved into concrete runs first (module docs, "A layered anchor").
  flatten(&mut values, chunks, blocks);
  blocks.hold(block);
  let anchor = chunks.add_source(Source::Block { block, len });
  let latest_value = Value {
    pieces: Vec::new(),
    len: 0,
    own: Some((anchor, len)),
  };
  let mut latest_value = latest_value;
  latest_value.push(
    chunks,
    Piece {
      chunk: anchor,
      at: 0,
      len,
      epoch: CURRENT,
    },
  );
  // Every new piece referenced before any old one is released, so no chunk a value still names is freed between.
  let index = KeptIndex::new(&kept);
  for value in &mut values {
    let pieces = value.remapped(&index, anchor);
    let rebuilt = Value::rebuilt(chunks, pieces);
    let old = std::mem::replace(&mut **value, rebuilt);
    old.release(chunks, blocks);
  }
  let (blocks_cut, bytes_cut) = cut_superseded_blocks(&mut values, chunks, blocks, &kept);
  for value in &mut values {
    if value.list_bytes() > value.len {
      let bytes = value.materialize(chunks, blocks);
      let whole = Value::owned(chunks, bytes);
      let old = std::mem::replace(&mut **value, whole);
      old.release(chunks, blocks);
    }
  }
  let lists_after = values
    .iter()
    .map(|value| value.list_bytes())
    .fold(0usize, usize::saturating_add);
  let latest_after = values.last().map_or(0, |value| value.list_bytes());
  let walked = values.len();
  Some(Reanchored {
    values: walked,
    value: latest_value,
    lists: (lists_before, lists_after),
    latest_list: (latest_before, latest_after),
    blocks_cut,
    bytes_cut,
  })
}

#[cfg(test)]
thread_local! {
  /// A test's switch to the full re-anchor for every change (the oracle the layered one is held to).
  pub(crate) static FULL_REANCHOR_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a change may take the layered path ([`overlay`]): always, but in a test that asks for the full re-anchor.
fn layered_allowed() -> bool {
  #[cfg(test)]
  if FULL_REANCHOR_ONLY.with(std::cell::Cell::get) {
    return false;
  }
  true
}

/// Whether `before` and `after` (of one length) hold the same bytes everywhere outside `ranges` (sorted, disjoint):
/// `Some` when they do.
fn unchanged_outside(before: &[u8], after: &[u8], ranges: &[(usize, usize)]) -> Option<()> {
  let len = before.len();
  let mut cursor = 0usize;
  for (start, end) in ranges.iter().copied().chain(std::iter::once((len, len))) {
    if before.get(cursor..start)? != after.get(cursor..start)? {
      return None;
    }
    cursor = end;
  }
  Some(())
}

/// A length-preserving change taken by its path's anchor (module docs, "A layered anchor"; A-803): where the latest
/// value is its anchor whole and `ops` only overwrite bytes inside it, which the new block holds where they did not
/// write, the bytes they overwrote are copied into one owned chunk as the anchor's next layer, the superseded latest
/// value reads the anchor at that layer, and the anchor names the new block: no other value is walked. O(log + the
/// bytes overwritten), against the full re-anchor's every value of the history. `None`, nothing changed, for any other
/// change: the caller re-anchors in full.
fn overlay(
  values: &mut [&mut Value],
  chunks: &mut Chunks,
  blocks: &mut Blocks,
  (block, ops): (Block, &[Op]),
) -> Option<Reanchored> {
  let latest = values.last()?;
  let latest_list = latest.list_bytes();
  let [piece] = latest.pieces.as_slice() else {
    return None;
  };
  if piece.epoch != CURRENT || piece.at != 0 || piece.len != latest.len {
    return None;
  }
  let anchor = piece.chunk;
  let Some(Some((Source::Block { block: old, len }, _))) = chunks.slots.get(anchor) else {
    return None;
  };
  let (old, len) = (*old, *len);
  if len != latest.len || old == block || blocks.bytes(block).len() != len {
    return None;
  }
  let ranges = overwritten(ops, len)?;
  // A change that overwrote every byte kept nothing: a whole replace, which keeps the earlier block as it is.
  if ranges.first() == Some(&(0, len)) {
    return None;
  }
  let (before, after) = (blocks.bytes(old), blocks.bytes(block));
  // A change that leaves every byte as it was has nothing to layer: the full re-anchor keeps it as before (its new block
  // may be the old one, where content is held once).
  if before == after {
    return None;
  }
  unchanged_outside(before, after, &ranges)?;
  let number = chunks.layers.get(&anchor).map_or(0, |layers| layers.next);
  if number >= CURRENT.saturating_sub(1) {
    return None;
  }
  // The bytes the change overwrote, as they were, copied into one owned chunk, and the old block let go: whoever else
  // holds it (a branch's map, the identity table), this history then holds only what the change overwrote.
  let mut owned = Vec::with_capacity(
    ranges
      .iter()
      .map(|(start, end)| end.saturating_sub(*start))
      .sum(),
  );
  let mut places = Vec::with_capacity(ranges.len());
  for (start, end) in &ranges {
    places.push(owned.len());
    owned.extend_from_slice(before.get(*start..*end)?);
  }
  let copied = owned.len();
  let held = chunks.add(owned);
  for ((start, end), place) in ranges.iter().zip(places) {
    chunks.retain(held);
    chunks.entries = chunks.entries.saturating_add(LAYER_BYTES);
    let layers = chunks.layers.entry(anchor).or_default();
    let layer_len = end.saturating_sub(*start);
    layers.by_start.insert(
      (*start, number),
      Layer {
        len: layer_len,
        chunk: held,
        at: place,
      },
    );
    layers.by_number.entry(number).or_default().push(*start);
    layers.longest = layers.longest.max(layer_len);
    let at_start = layers.start_longest.entry(*start).or_default();
    *at_start = (*at_start).max(layer_len);
  }
  let layers = chunks.layers.entry(anchor).or_default();
  layers.next = number.saturating_add(1);
  // The superseded latest value reads the anchor as it was: through the layer just made.
  let count = layers.epochs.entry(number).or_default();
  *count = count.saturating_add(1);
  if let Some(superseded) = values.last_mut()
    && let Some(piece) = superseded.pieces.first_mut()
  {
    piece.epoch = number;
  }
  // The anchor names the new block; the old one is let go (its overwritten bytes are the layer's).
  blocks.hold(block);
  if let Some(Some((source, _))) = chunks.slots.get_mut(anchor) {
    let previous = std::mem::replace(source, Source::Block { block, len });
    previous.let_go(blocks);
  }
  let mut value = Value {
    pieces: Vec::new(),
    len: 0,
    own: Some((anchor, len)),
  };
  value.push(
    chunks,
    Piece {
      chunk: anchor,
      at: 0,
      len,
      epoch: CURRENT,
    },
  );
  Some(Reanchored {
    value,
    lists: (0, 0),
    latest_list: (latest_list, latest_list),
    blocks_cut: 1,
    bytes_cut: copied,
    values: 1,
  })
}

/// The byte ranges `ops` overwrite in a file of `len` bytes, in order and merged where they touch: an overwrite's, or a
/// same-length replacement's (a delete and an insert of its length at its offset, as an SDK edit composes it). `None`
/// where an op is anything else or lies past the file, where the ops are out of order, or where they write nothing.
fn overwritten(ops: &[Op], len: usize) -> Option<Vec<(usize, usize)>> {
  let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(ops.len());
  let mut ops = ops.iter().peekable();
  while let Some(op) = ops.next() {
    if replaced_here(op, ops.peek().copied()) {
      ops.next();
    } else if op.kind != OpKind::Overwrite {
      return None;
    }
    let start = usize::try_from(op.at).ok()?;
    let end = start.checked_add(usize::try_from(op.len).ok()?)?;
    if end > len {
      return None;
    }
    if end == start {
      continue;
    }
    match ranges.last_mut() {
      Some((last_start, _)) if start < *last_start => return None,
      Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
      _ => ranges.push((start, end)),
    }
  }
  (!ranges.is_empty()).then_some(ranges)
}

/// Whether `op` and `next` are a same-length replacement at one offset, a delete and an insert of its length, as an SDK
/// edit composes one ([`overwritten`]): an overwrite there, which the caller checks byte for byte against the new block.
fn replaced_here(op: &Op, next: Option<&Op>) -> bool {
  op.kind == OpKind::Delete
    && next
      .is_some_and(|next| next.kind == OpKind::Insert && next.at == op.at && next.len == op.len)
}

/// Every value of `values` whose pieces read an anchor through its layers rebuilt of concrete runs ([`Value::concrete`]),
/// the layers' owned chunks held by the runs now, so the full re-anchor walks concrete pieces alone; the layers no piece
/// reads any more are dropped as the old pieces go ([`Chunks::release_piece`]).
fn flatten(values: &mut [&mut Value], chunks: &mut Chunks, blocks: &mut Blocks) {
  for value in values.iter_mut() {
    let concrete = match value.concrete(chunks) {
      std::borrow::Cow::Borrowed(_) => continue,
      std::borrow::Cow::Owned(pieces) => pieces,
    };
    let own = value.own;
    let mut rebuilt = Value::rebuilt(chunks, concrete);
    rebuilt.own = own;
    let old = std::mem::replace(&mut **value, rebuilt);
    old.release(chunks, blocks);
  }
}

/// Cuts each block the kept runs lay in that only `values` still name, and only this history holds, to the runs they
/// name of it: the union of those runs copied into one owned chunk, the pieces moved to it, the block let go
/// ([`reanchor`]). A block another holder keeps (the identity table, a branch) or another path's value names is left
/// whole. The blocks let go and the bytes copied.
fn cut_superseded_blocks(
  values: &mut [&mut Value],
  chunks: &mut Chunks,
  blocks: &mut Blocks,
  kept: &[Kept],
) -> (u64, usize) {
  let mut candidates: Vec<usize> = kept.iter().map(|run| run.chunk).collect();
  candidates.sort_unstable();
  candidates.dedup();
  let (mut cut_blocks, mut cut_bytes) = (0u64, 0usize);
  for chunk in candidates {
    let Some(Some((Source::Block { block, .. }, refs))) = chunks.slots.get(chunk) else {
      continue;
    };
    let named_here = values
      .iter()
      .flat_map(|value| value.pieces.iter())
      .filter(|piece| piece.chunk == chunk)
      .count();
    if blocks.holders(*block) != 1 || named_here != *refs {
      continue;
    }
    // The union of the runs still named, in chunk order: each run's place in the owned copy.
    let mut runs: Vec<(usize, usize)> = values
      .iter()
      .flat_map(|value| value.pieces.iter())
      .filter(|piece| piece.chunk == chunk)
      .map(|piece| (piece.at, piece.at.saturating_add(piece.len)))
      .collect();
    runs.sort_unstable();
    let mut union: Vec<(usize, usize, usize)> = Vec::new();
    let mut owned = Vec::new();
    for (start, end) in runs {
      match union.last_mut() {
        Some((_, last_end, _)) if start <= *last_end => {
          if end > *last_end {
            let from = *last_end;
            owned.extend_from_slice(chunks.bytes_of(
              blocks,
              &Piece {
                chunk,
                at: from,
                len: end.saturating_sub(from),
                epoch: CURRENT,
              },
            ));
            *last_end = end;
          }
        }
        _ => {
          let place = owned.len();
          owned.extend_from_slice(chunks.bytes_of(
            blocks,
            &Piece {
              chunk,
              at: start,
              len: end.saturating_sub(start),
              epoch: CURRENT,
            },
          ));
          union.push((start, end, place));
        }
      }
    }
    let copied = owned.len();
    let cut = chunks.add(owned);
    for value in values.iter_mut() {
      for piece in value.pieces.iter_mut().filter(|piece| piece.chunk == chunk) {
        // The union is sorted by start and disjoint: the run covering the piece is the last starting at or before it,
        // found by a binary search (a scan per piece was O(pieces x runs), A-787 part 2's red).
        let Some((start, _, place)) = union
          .partition_point(|(start, _, _)| *start <= piece.at)
          .checked_sub(1)
          .and_then(|last| union.get(last))
          .filter(|(_, end, _)| piece.at < *end)
        else {
          continue;
        };
        let before = *piece;
        piece.at = place.saturating_add(piece.at.saturating_sub(*start));
        piece.chunk = cut;
        // An owned chunk has no layers: its pieces read it as it is.
        piece.epoch = CURRENT;
        chunks.retain_piece(piece);
        chunks.release_piece(&before, blocks);
      }
    }
    cut_blocks = cut_blocks.saturating_add(1);
    cut_bytes = cut_bytes.saturating_add(copied);
  }
  (cut_blocks, cut_bytes)
}
