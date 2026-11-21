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
//! **Lifetimes.** The blocks live in the files' arena, which a lineage's greens share and lend ([`crate::dims`], "Arenas
//! are lent"), while the chunks are the green's own. A lineage member is read and written only while lent, its arenas
//! then the lineage's, so a block a chunk names is always in the arena at hand; a green made at another's version starts
//! with no history; a dropped value lets its block go into the same arena; and a retired green gives its chunks' holds
//! back a chunk a step ([`Chunks::release_step`]). Every function that reads a value's bytes takes the blocks with the
//! chunks.

use crate::dims::{Block, Blocks};
use crate::ops_doc::{Op, OpKind};

/// A run of a value's bytes: `len` bytes at `at` within chunk `chunk`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Piece {
  /// The chunk.
  chunk: usize,
  /// The run's offset within the chunk.
  at: usize,
  /// The run's length.
  len: usize,
}

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
}

impl Chunks {
  /// The bytes the live chunks hold, owned or named.
  pub(crate) fn bytes(&self) -> usize {
    self.bytes
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
    seen
      .into_iter()
      .filter_map(|chunk| self.slots.get(chunk).and_then(Option::as_ref))
      .map(|(source, _)| source.len())
      .sum()
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
    }
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
      pieces: vec![Piece { chunk, at: 0, len }],
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
      pieces: vec![Piece { chunk, at: 0, len }],
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
      pieces: vec![Piece { chunk, at: 0, len }],
      len,
      own: None,
    }
  }

  /// The value's bytes, copied out of its chunks.
  pub(crate) fn materialize(&self, chunks: &Chunks, blocks: &Blocks) -> Vec<u8> {
    let mut out = Vec::with_capacity(self.len);
    for piece in &self.pieces {
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
    for piece in &self.pieces {
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
    for piece in &self.pieces {
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
    let Some(runs) = self.applied_runs(ops, post_len) else {
      return false;
    };
    let len = runs
      .iter()
      .fold(0usize, |sum, run| sum.saturating_add(run.len()));
    if len != other.len {
      return false;
    }
    let mut theirs = other.pieces.iter().copied();
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
        });
      }
    }
    true
  }

  /// The runs the engine's `apply` writes when it applies `ops` to this value over a post-state of `post_len` bytes:
  /// runs of this value it keeps and runs of the post-state it adds, each clamped as `apply` clamps them. `None` when
  /// an offset does not fit a `usize`.
  fn applied_runs(&self, ops: &[Op], post_len: usize) -> Option<Vec<Applied>> {
    let base_len = u64::try_from(self.len).ok()?;
    let mut runs = Vec::new();
    let mut shared = Vec::new();
    let mut pos = 0u64;
    for op in ops {
      if op.at > pos {
        self.share(pos.min(base_len), op.at.min(base_len), 0, &mut shared)?;
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
      self.share(pos, base_len, 0, &mut shared)?;
    }
    runs.extend(shared.drain(..).filter_map(Applied::of_shared));
    runs.retain(|run| run.len() > 0);
    Some(runs)
  }

  /// Gives the value's chunks back: each piece's reference released, a chunk's block let go with its last.
  pub(crate) fn release(self, chunks: &mut Chunks, blocks: &mut Blocks) {
    for piece in self.pieces {
      chunks.release(piece.chunk, blocks);
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
    let pending = self.walk(ops, next.len())?;
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
          out.push(Piece {
            chunk: anchor,
            at: run.out.saturating_add(cursor.saturating_sub(run.at)),
            len: stop.saturating_sub(cursor),
          });
          cursor = stop;
        } else {
          let stop = kept.next_start(piece.chunk, cursor).unwrap_or(end).min(end);
          out.push(Piece {
            chunk: piece.chunk,
            at: cursor,
            len: stop.saturating_sub(cursor),
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
      && last.at.saturating_add(last.len) == piece.at
    {
      last.len = last.len.saturating_add(piece.len);
      return;
    }
    chunks.retain(piece.chunk);
    self.pieces.push(piece);
  }

  /// The runs the engine's `apply` would produce over this value, in order: shared runs of this value and runs of
  /// the output the ops add. `None` when the runs do not cover `next_len` exactly.
  fn walk(&self, ops: &[Op], next_len: usize) -> Option<Vec<Pending>> {
    let base_len = u64::try_from(self.len).ok()?;
    let mut runs = Vec::new();
    let mut pos = 0u64;
    let mut out = 0usize;
    for op in ops {
      if op.at > pos {
        let start = pos.min(base_len);
        let end = op.at.min(base_len);
        out = self.share(start, end, out, &mut runs)?;
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
      out = self.share(pos, base_len, out, &mut runs)?;
    }
    (out == next_len).then_some(runs)
  }

  /// Adds the runs of this value covering `[start, end)` as shared runs; the output length after them.
  fn share(&self, start: u64, end: u64, out: usize, runs: &mut Vec<Pending>) -> Option<usize> {
    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?;
    if end <= start {
      return Some(out);
    }
    let mut logical = 0usize;
    for piece in &self.pieces {
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
  let latest = values.last()?;
  let kept = latest.kept_runs((chunks, blocks), ops, blocks.bytes(block))?;
  let len = blocks.bytes(block).len();
  let latest_before = latest.list_bytes();
  let lists_before = values
    .iter()
    .map(|value| value.list_bytes())
    .fold(0usize, usize::saturating_add);
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
  Some(Reanchored {
    value: latest_value,
    lists: (lists_before, lists_after),
    latest_list: (latest_before, latest_after),
    blocks_cut,
    bytes_cut,
  })
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
        piece.at = place.saturating_add(piece.at.saturating_sub(*start));
        piece.chunk = cut;
        chunks.retain(cut);
        chunks.release(chunk, blocks);
      }
    }
    cut_blocks = cut_blocks.saturating_add(1);
    cut_bytes = cut_bytes.saturating_add(copied);
  }
  (cut_blocks, cut_bytes)
}

#[cfg(test)]
mod tests {
  //! The history's oracle: over generated chains of ops on one file, every version's value materializes to exactly
  //! what the engine's `apply` made of the version before it; the chunks a chain holds are its pieces' chunks and no
  //! more (released values free them, and the file blocks they name with them); a version that changes a few bytes of a
  //! large file costs those bytes and its list, not the file; the latest value copies nothing (A-728); and compaction
  //! keeps every version's cost within a whole copy.
  #![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
  )]

  use super::*;
  use crate::dims::Files;
  use proptest::prelude::*;

  /// The remap as it was before [`KeptIndex`]: each piece's cursor looked up by a scan of every kept run, the first in
  /// order covering it. The oracle the index must equal.
  fn remapped_by_scan(value: &Value, kept: &[Kept], anchor: usize) -> Vec<Piece> {
    let mut out = Vec::new();
    for piece in &value.pieces {
      let end = piece.at + piece.len;
      let mut cursor = piece.at;
      while cursor < end {
        let covering = kept
          .iter()
          .find(|run| run.chunk == piece.chunk && run.at <= cursor && cursor < run.at + run.len);
        if let Some(run) = covering {
          let stop = end.min(run.at + run.len);
          out.push(Piece {
            chunk: anchor,
            at: run.out + (cursor - run.at),
            len: stop - cursor,
          });
          cursor = stop;
        } else {
          let stop = kept
            .iter()
            .filter(|run| run.chunk == piece.chunk && run.at > cursor)
            .map(|run| run.at)
            .min()
            .unwrap_or(end)
            .min(end);
          out.push(Piece {
            chunk: piece.chunk,
            at: cursor,
            len: stop - cursor,
          });
          cursor = stop;
        }
      }
    }
    out
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// A-787 part 2's red (a re-anchor O(pieces x runs)): do remap generated pieces against generated kept runs over a
    /// few chunks, disjoint or overlapping, by the index; expect exactly what the scan makes.
    #[test]
    fn the_indexed_remap_is_the_scans(
      pieces in prop::collection::vec((0usize..3, 0usize..200, 1usize..40), 0..40),
      runs in prop::collection::vec((0usize..3, 0usize..200, 0usize..40, 0usize..500), 0..40),
    ) {
      let value = Value {
        pieces: pieces.iter().map(|&(chunk, at, len)| Piece { chunk, at, len }).collect(),
        len: 0,
        own: None,
      };
      let kept: Vec<Kept> = runs.iter().map(|&(chunk, at, len, out)| Kept { chunk, at, len, out }).collect();
      let index = KeptIndex::new(&kept);
      let indexed = value.remapped(&index, 9);
      let scanned = remapped_by_scan(&value, &kept, 9);
      prop_assert_eq!(indexed, scanned);
    }
  }

  /// A generated op over a file of `len` bytes: kind, offset share, length, and the post-state offset.
  fn op_of(kind: u8, at: u64, len: u64, src: u64) -> Op {
    let kind = match kind % 5 {
      0 => OpKind::Overwrite,
      1 => OpKind::Insert,
      2 => OpKind::Delete,
      3 => OpKind::Extend,
      _ => OpKind::Truncate,
    };
    Op {
      kind,
      flags: 0,
      path: 0,
      at,
      len,
      src,
    }
  }

  /// The engine's `apply`, restated: the oracle the values are held to.
  fn apply(base: &[u8], ops: &[Op], post: &[u8]) -> Vec<u8> {
    crate::engine::apply_for_tests(base, ops, post)
  }

  /// Ops sorted by offset (the deriver's shape) over a file of `len` bytes, from generated tuples.
  fn ops_over(len: u64, raw: &[(u8, u16, u16, u16)]) -> Vec<Op> {
    let mut ops: Vec<Op> = raw
      .iter()
      .map(|(kind, at, op_len, src)| {
        let at = if len == 0 {
          0
        } else {
          u64::from(*at) % (len + 1)
        };
        op_of(*kind, at, u64::from(*op_len) % 64, u64::from(*src) % 256)
      })
      .collect();
    ops.sort_by_key(|op| op.at);
    ops
  }

  /// The file `f`'s first value: `bytes` set and its block named.
  fn first_value(chunks: &mut Chunks, files: &mut Files, bytes: &[u8]) -> Value {
    files.insert("f", bytes.to_vec());
    let block = files.hold_block_of("f").unwrap();
    Value::named(chunks, files.blocks_mut(), block, bytes.len())
  }

  /// One version as the engine makes it (A-793): the file set to `next`, the latest value superseded, the path's history
  /// re-anchored on the new block, or that block named whole when the ops keep nothing of the latest; the new value
  /// pushed. Whether it re-anchored.
  fn version(
    chunks: &mut Chunks,
    files: &mut Files,
    values: &mut Vec<Value>,
    (ops, next): (&[Op], &[u8]),
  ) -> bool {
    files.insert("f", next.to_vec());
    if let Some(latest) = values.last_mut() {
      latest.supersede();
    }
    let block = files.block_of("f").unwrap();
    let anchored = reanchor(
      values.iter_mut().collect(),
      chunks,
      files.blocks_mut(),
      (block, ops),
    );
    let (value, anchored) = match anchored {
      Some(done) => (done.value, true),
      None => {
        let block = files.hold_block_of("f").unwrap();
        (
          Value::named(chunks, files.blocks_mut(), block, next.len()),
          false,
        )
      }
    };
    values.push(value);
    anchored
  }

  /// The most of a value of `len` bytes that `ops` can leave unkept: what they overwrite, delete or truncate.
  fn removed_by(ops: &[Op], len: usize) -> usize {
    ops
      .iter()
      .map(|op| match op.kind {
        OpKind::Overwrite | OpKind::Delete => op.len as usize,
        OpKind::Truncate => len.saturating_sub(op.at as usize),
        _ => 0,
      })
      .sum::<usize>()
      .min(len)
  }

  /// Releases every value, then removes the file: neither the chunks nor the blocks may hold anything after.
  fn release_all(chunks: &mut Chunks, files: &mut Files, values: Vec<Value>) {
    for value in values {
      value.release(chunks, files.blocks_mut());
    }
    files.remove("f");
    assert_eq!((chunks.bytes(), files.blocks().live()), (0, 0));
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]
    /// W2, A-728, A-793. Do: from a generated first value, apply generated op chains version after version as the
    /// engine does (the file set, the latest superseded, the history re-anchored on the new block or the block named
    /// whole), checking every value kept so far; then release every value and remove the file. Expect: every version
    /// materializes to `apply`'s bytes; the history owns no more than what the changes removed of the values they
    /// superseded, and the small values compaction stored whole; every list costs at most its value's bytes (or one
    /// piece); the arena's live bytes are exactly what the values' pieces name; the files' arena holds no more blocks
    /// than the current one, the first, and one for each change that kept nothing; and after the release neither the
    /// arena nor the blocks hold anything.
    #[test]
    fn every_version_is_apply_and_shares_what_it_kept(
      first in prop::collection::vec(any::<u8>(), 0..512),
      chain in prop::collection::vec(prop::collection::vec((any::<u8>(), any::<u16>(), any::<u16>(), any::<u16>()), 0..6), 1..12),
      post in prop::collection::vec(any::<u8>(), 256..512),
    ) {
      let mut chunks = Chunks::default();
      let mut files = Files::default();
      let mut values = vec![first_value(&mut chunks, &mut files, &first)];
      let mut bytes = vec![first.clone()];
      let (mut removed, mut ops_total, mut whole_changes) = (0usize, 0usize, 0usize);
      for raw in &chain {
        let previous = bytes.last().unwrap().clone();
        let ops = ops_over(previous.len() as u64, raw);
        let next = apply(&previous, &ops, &post);
        if version(&mut chunks, &mut files, &mut values, (&ops, &next)) {
          removed += removed_by(&ops, previous.len());
        } else {
          whole_changes += 1;
        }
        ops_total += ops.len();
        bytes.push(next);
        // A value compacts only when its pieces outweigh it, and each op adds at most two pieces to it.
        let small = PIECE_BYTES * (2 * ops_total + 3);
        let compacted: usize = bytes.iter().map(Vec::len).filter(|len| *len < small).sum();
        prop_assert!(
          chunks.owned_bytes() <= removed + compacted,
          "owned {} for {} removed and {} in small values", chunks.owned_bytes(), removed, compacted
        );
        prop_assert!(files.blocks().live() <= 2 + whole_changes, "{} blocks", files.blocks().live());
        for (value, expected) in values.iter().zip(&bytes) {
          prop_assert!(value.list_bytes() <= value.len().max(PIECE_BYTES));
          prop_assert!(value.equals((&chunks, files.blocks()), expected));
          prop_assert_eq!(&value.materialize(&chunks, files.blocks()), expected);
        }
        prop_assert_eq!(chunks.referenced_bytes(values.iter()), chunks.bytes());
      }
      for value in values {
        value.release(&mut chunks, files.blocks_mut());
      }
      files.remove("f");
      prop_assert_eq!(chunks.bytes(), 0);
      prop_assert_eq!(files.blocks().live(), 0);
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]
    /// §4.16 "The verdict" (the whole-file identity check). Do: from a generated first value, build a head by a
    /// generated chain of versions (each made as the engine makes it, or, at a generated version, stored whole as an
    /// owned copy so it shares nothing); pick a generated base among the versions and generated ops over it, the
    /// head's own last ops among them so the two converge. Expect: `applied_equals` answers exactly whether `apply`
    /// of the ops to the base's bytes equals the head's bytes, for every pick.
    #[test]
    fn applied_equals_is_the_bytes_compared(
      first in prop::collection::vec(any::<u8>(), 0..256),
      chain in prop::collection::vec(prop::collection::vec((any::<u8>(), any::<u16>(), any::<u16>(), any::<u16>()), 0..5), 1..8),
      whole_at in any::<u8>(),
      pick in any::<u8>(),
      raw in prop::collection::vec((any::<u8>(), any::<u16>(), any::<u16>(), any::<u16>()), 0..5),
      converge in any::<bool>(),
      post in prop::collection::vec(any::<u8>(), 0..300),
    ) {
      let mut chunks = Chunks::default();
      let mut files = Files::default();
      let mut values = vec![first_value(&mut chunks, &mut files, &first)];
      let mut made = vec![(first.clone(), Vec::new())];
      for (n, step) in chain.iter().enumerate() {
        let previous = made.last().unwrap().0.clone();
        let ops = ops_over(previous.len() as u64, step);
        let next = apply(&previous, &ops, &post);
        if n == usize::from(whole_at) % chain.len() {
          files.insert("f", next.clone());
          if let Some(latest) = values.last_mut() {
            latest.supersede();
          }
          values.push(Value::whole(&mut chunks, &next));
        } else {
          version(&mut chunks, &mut files, &mut values, (&ops, &next));
        }
        made.push((next, ops));
      }
      let (head_bytes, head_ops) = made.last().unwrap();
      // Converging: the version before the head, with the ops that made the head of it.
      let at = if converge { made.len() - 2 } else { usize::from(pick) % made.len() };
      let base_bytes = &made[at].0;
      let ops = if converge {
        head_ops.clone()
      } else {
        ops_over(base_bytes.len() as u64, &raw)
      };
      let expected = apply(base_bytes, &ops, &post) == *head_bytes;
      prop_assert_eq!(values[at].applied_equals(
        (&chunks, files.blocks()),
        (&ops, crate::named::Post::of(&post, &[])),
        values.last().unwrap(),
      ), expected);
    }
  }

  /// W2, A-793. Do: a 1 MiB file, then 64 versions each overwriting 64 bytes of its own range, made as the engine
  /// makes them; then release everything. Expect: the history holds the current file's block, which the latest names
  /// and every earlier version shares, and the 64 bytes each change overwrote, owned; not the first file's block (A-728
  /// part 2 kept it whole), and not 64 copies of the file; every version reads back; nothing is left after the release.
  #[test]
  fn small_overwrites_of_a_large_file_cost_what_they_wrote() {
    const FILE: usize = 1 << 20;
    const EDIT: u64 = 64;
    const EDITS: usize = 64;
    let mut chunks = Chunks::default();
    let mut files = Files::default();
    let mut bytes: Vec<u8> = (0..FILE).map(|n| (n % 251) as u8).collect();
    let mut values = vec![first_value(&mut chunks, &mut files, &bytes)];
    let mut versions = vec![bytes.clone()];
    for n in 0..EDITS {
      let at = (n * FILE / EDITS) as u64;
      let post = vec![n as u8; EDIT as usize];
      let ops = [op_of(0, at, EDIT, 0)];
      let next = apply(&bytes, &ops, &post);
      assert!(
        version(&mut chunks, &mut files, &mut values, (&ops, &next)),
        "edit {n} re-anchored"
      );
      bytes = next;
      versions.push(bytes.clone());
    }
    assert_eq!(
      chunks.owned_bytes(),
      EDITS * EDIT as usize,
      "each change's overwritten bytes, once"
    );
    assert_eq!(chunks.bytes(), FILE + EDITS * EDIT as usize);
    assert_eq!(files.blocks().live(), 1, "the current file's block alone");
    for (value, expected) in values.iter().zip(&versions) {
      assert_eq!(&value.materialize(&chunks, files.blocks()), expected);
    }
    release_all(&mut chunks, &mut files, values);
  }

  /// A-728, A-793. Do: a file's block named as a value; a small overwrite made over it; then a whole replace of the
  /// result; then the file removed and every value released. Expect: naming copies no byte; the overwrite cuts the
  /// first block to its 8 overwritten bytes and lets it go; the whole replace keeps nothing of the value it supersedes,
  /// so that version's block stays as its bytes beside the replacing file's; every value reads back exactly, the first
  /// ones after the file is gone; nothing is left after the release.
  #[test]
  fn a_whole_replace_keeps_the_block_it_replaced() {
    let mut files = Files::default();
    let first: Vec<u8> = (0..4096u32)
      .map(|n| u8::try_from(n % 251).unwrap())
      .collect();
    let mut chunks = Chunks::default();
    let mut values = vec![first_value(&mut chunks, &mut files, &first)];
    assert_eq!(chunks.owned_bytes(), 0, "naming the block copies nothing");
    assert_eq!(chunks.bytes(), first.len());
    let ops = [op_of(0, 100, 8, 0)];
    let next = apply(&first, &ops, &[7u8; 8]);
    assert!(version(&mut chunks, &mut files, &mut values, (&ops, &next)));
    assert_eq!(
      chunks.owned_bytes(),
      8,
      "the first block cut to the 8 bytes overwritten"
    );
    assert_eq!(files.blocks().live(), 1, "the first block let go");
    let replace = [op_of(4, 0, 0, 0), op_of(3, 0, 16, 0)];
    let replaced = apply(&next, &replace, &[9u8; 16]);
    assert!(
      !version(&mut chunks, &mut files, &mut values, (&replace, &replaced)),
      "a whole replace keeps nothing"
    );
    assert_eq!(chunks.owned_bytes(), 8);
    assert_eq!(
      files.blocks().live(),
      2,
      "the replaced version's block and the replacing file's"
    );
    files.remove("f");
    read_back_then_released(&mut chunks, &mut files, values, &[&first, &next, &replaced]);
  }

  /// Every one of `values` reads back as its `expected` bytes; then each is released, and nothing is left.
  fn read_back_then_released(
    chunks: &mut Chunks,
    files: &mut Files,
    values: Vec<Value>,
    expected: &[&Vec<u8>],
  ) {
    for (value, expected) in values.iter().zip(expected) {
      assert_eq!(&&value.materialize(chunks, files.blocks()), expected);
    }
    for value in values {
      value.release(chunks, files.blocks_mut());
    }
    assert_eq!((chunks.bytes(), files.blocks().live()), (0, 0));
  }

  /// A-728 part 4, A-793. Do: overwrite 4 bytes of a file while another holder keeps its first block (as the identity
  /// table or a branch's map would); then let that holder go. Expect the re-anchor to move the kept runs to the new
  /// block but not cut the first block, which the other holder keeps: nothing copied, the 4 overwritten bytes still read
  /// from it; every value reads back; nothing is left after the release.
  #[test]
  fn a_reanchor_leaves_a_block_another_holder_keeps() {
    let mut files = Files::default();
    let mut chunks = Chunks::default();
    let first = vec![1u8; 512];
    let mut values = vec![first_value(&mut chunks, &mut files, &first)];
    let held = files.hold_block_of("f").unwrap();
    let ops = [op_of(0, 10, 4, 0)];
    let next = apply(&first, &ops, &[2u8; 4]);
    assert!(version(&mut chunks, &mut files, &mut values, (&ops, &next)));
    assert_eq!(
      chunks.owned_bytes(),
      0,
      "a block another holder keeps is not cut"
    );
    assert_eq!(
      files.blocks().live(),
      2,
      "the held first block and the current one"
    );
    files.blocks_mut().release(held);
    for (value, expected) in values.iter().zip([&first, &next]) {
      assert_eq!(&value.materialize(&chunks, files.blocks()), expected);
    }
    release_all(&mut chunks, &mut files, values);
  }
}
