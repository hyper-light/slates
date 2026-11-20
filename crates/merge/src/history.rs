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
//! pieces and the length. [`Value::spliced`] builds the value `next` that a path's accepted ops make of `self`, the
//! same walk as the engine's `apply`: each run of `self` the ops keep is shared, and the bytes the ops add are copied
//! out of `next` into one new chunk. The result is checked against `next` byte for byte, and a value that does not
//! reproduce it (an op whose post-state was shorter than it declared, which `apply` clamps) is stored whole instead,
//! so the history is exact whatever the ops say.
//!
//! **The bound.** A value whose piece list would cost more than its bytes is stored as one chunk instead (compaction):
//! a version never costs more than a whole copy, which is what every version cost before, and a file edited in many
//! places is rewritten once its pieces outweigh it. No constant: the rule is the two sizes.
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
//! **A splice's added bytes are the new block's (A-728 part 2).** A splice shares the runs it keeps, as before, and its
//! added runs are pieces of the file's new block at their place in it, so the latest value copies nothing at all. When a
//! later change supersedes it ([`Value::seal`]), the block is the map's no longer: a chunk named by that value alone
//! whose block nothing else holds is cut to the runs the value names, copied into one owned chunk, and the block let go,
//! when they are less than the block (the copy the splice made at once before, now made once it is history); a block
//! the value names whole, or that another holder keeps (a branch, the identity table), is kept as it is.
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
  /// A file's block, of which the chunk holds one holder, its length (a block never changes), and what the chunk is
  /// charged.
  Block {
    /// The block.
    block: Block,
    /// Its length.
    len: usize,
    /// The bytes the chunk counts in the history's charge: the runs of the block the value it was made for names (the
    /// whole block for a value named whole, a splice's added runs otherwise), so a splice's chunk is charged what the
    /// copy it replaces was, and admission's reservation (`Green::history_reservation`) is unchanged by it.
    charge: usize,
  },
}

impl Source {
  /// The bytes the chunk counts: an owned chunk's length, a block's charge.
  fn len(&self) -> usize {
    match self {
      Source::Owned(bytes) => bytes.len(),
      Source::Block { charge, .. } => *charge,
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
}

/// A run of the value being built: one of the previous value's, or bytes of `next` the ops added.
enum Pending {
  /// A run of the previous value, shared.
  Shared(Piece),
  /// `len` bytes of `next` at `at`, to be copied into the new chunk.
  Added { at: usize, len: usize },
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
    let chunk = chunks.add_source(Source::Block {
      block,
      len,
      charge: len,
    });
    chunks.retain(chunk);
    Value {
      pieces: vec![Piece { chunk, at: 0, len }],
      len,
    }
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
    (ops, post_state): (&[Op], &[u8]),
    other: &Value,
  ) -> bool {
    let Some(runs) = self.applied_runs(ops, post_state.len()) else {
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
            post_state.get(src..src.saturating_add(step))
              == Some(chunks.bytes_of(blocks, &Piece { len: step, ..piece }))
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

  /// The value `next` that `ops` (a path's accepted ops in this value's coordinates, as the engine's `apply` takes
  /// them) make of this one, sharing every run of this value they keep and copying only the bytes they add (module
  /// docs). `None` when it is to be stored whole, which the caller does by naming the file's block
  /// ([`Value::named`]): when the splice shares no run of this value (a whole replace), does not reproduce `next`, or
  /// its pieces would outweigh its bytes.
  ///
  /// `next` is the file's new block, `block` (A-728 part 2): the added runs are pieces of it at their place, no byte
  /// copied. The answer's flag says whether it names the block, for which the caller then takes the chunk's holder
  /// (`Blocks::hold`); the splice is checked against `next` before any chunk is made, so a refused one leaves nothing.
  pub(crate) fn spliced(
    &self,
    chunks: &mut Chunks,
    (blocks, block): (&Blocks, Block),
    ops: &[Op],
    next: &[u8],
  ) -> Option<(Value, bool)> {
    let pending = self.walk(ops, next.len())?;
    if pending.len().saturating_mul(PIECE_BYTES) > next.len()
      || !pending.iter().any(|run| matches!(run, Pending::Shared(_)))
    {
      return None;
    }
    // The shared runs must be `next`'s bytes where they land; the added runs are `next` itself.
    let mut out = 0usize;
    for run in &pending {
      match run {
        Pending::Shared(piece) => {
          let end = out.saturating_add(piece.len);
          if next.get(out..end) != Some(chunks.bytes_of(blocks, piece)) {
            return None;
          }
          out = end;
        }
        Pending::Added { at, len } => {
          if *at != out {
            return None;
          }
          out = out.saturating_add(*len);
        }
      }
    }
    let added: usize = pending
      .iter()
      .map(|run| match run {
        Pending::Added { len, .. } => *len,
        Pending::Shared(_) => 0,
      })
      .fold(0usize, usize::saturating_add);
    let named = (added > 0).then(|| {
      chunks.add_source(Source::Block {
        block,
        len: next.len(),
        charge: added,
      })
    });
    let mut value = Value::default();
    for run in pending {
      let piece = match run {
        Pending::Shared(piece) => piece,
        Pending::Added { at, len } => {
          let Some(chunk) = named else {
            continue;
          };
          Piece { chunk, at, len }
        }
      };
      value.push(chunks, piece);
    }
    Some((value, named.is_some()))
  }

  /// Seals this value as a later change supersedes it (module docs, "A splice's added bytes are the new block's"): each
  /// chunk only this value names, whose block only the history holds, and of which it names less than the whole, is cut
  /// to the runs it names, copied into one owned chunk, and its block let go. The bytes copied.
  pub(crate) fn seal(&mut self, chunks: &mut Chunks, blocks: &mut Blocks) -> usize {
    let mut copied = 0usize;
    let mut distinct: Vec<usize> = self.pieces.iter().map(|piece| piece.chunk).collect();
    distinct.sort_unstable();
    distinct.dedup();
    for chunk in distinct {
      let mine = self
        .pieces
        .iter()
        .filter(|piece| piece.chunk == chunk)
        .count();
      let named: usize = self
        .pieces
        .iter()
        .filter(|piece| piece.chunk == chunk)
        .map(|piece| piece.len)
        .fold(0usize, usize::saturating_add);
      let Some(Some((Source::Block { block, len, .. }, refs))) = chunks.slots.get(chunk) else {
        continue;
      };
      if *refs != mine || named >= *len || blocks.holders(*block) != 1 {
        continue;
      }
      let mut owned = Vec::with_capacity(named);
      for piece in self.pieces.iter_mut().filter(|piece| piece.chunk == chunk) {
        let at = owned.len();
        owned.extend_from_slice(chunks.bytes_of(blocks, piece));
        piece.at = at;
      }
      let cut = chunks.add(owned);
      for piece in self.pieces.iter_mut().filter(|piece| piece.chunk == chunk) {
        piece.chunk = cut;
        chunks.retain(cut);
        chunks.release(chunk, blocks);
      }
      copied = copied.saturating_add(named);
    }
    copied
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

  /// One version as the engine makes it (A-728): the file set to `next`, the previous latest value sealed, then `next`'s
  /// value spliced from it into the file's block, or the block named whole.
  fn version(
    chunks: &mut Chunks,
    files: &mut Files,
    latest: &mut Value,
    (ops, next): (&[Op], &[u8]),
  ) -> Value {
    files.insert("f", next.to_vec());
    latest.seal(chunks, files.blocks_mut());
    let block = files.block_of("f").unwrap();
    match latest.spliced(chunks, (files.blocks(), block), ops, next) {
      Some((value, named)) => {
        if named {
          files.blocks_mut().hold(block);
        }
        value
      }
      None => {
        let block = files.hold_block_of("f").unwrap();
        Value::named(chunks, files.blocks_mut(), block, next.len())
      }
    }
  }

  /// The bytes `ops` add.
  fn added_by(ops: &[Op]) -> usize {
    ops
      .iter()
      .filter(|op| matches!(op.kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend))
      .map(|op| op.len as usize)
      .sum()
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// W2, A-728. Do: from a generated first value, apply generated op chains version after version as the engine
    /// does (the file set, the previous value sealed, the new one spliced into the file's block or named whole),
    /// checking every value kept so far; then release every value and remove the file. Expect: every version
    /// materializes to `apply`'s bytes; the history owns no more than the ops of the versions before the latest added
    /// (the latest's added bytes are the file's block, not a copy); every list costs at most its value's bytes; the
    /// arena's live bytes are exactly what the kept values' pieces name; and after the release neither the arena nor
    /// the blocks hold anything.
    #[test]
    fn every_version_is_apply_and_shares_what_it_kept(
      first in prop::collection::vec(any::<u8>(), 0..512),
      chain in prop::collection::vec(prop::collection::vec((any::<u8>(), any::<u16>(), any::<u16>(), any::<u16>()), 0..6), 1..12),
      post in prop::collection::vec(any::<u8>(), 256..512),
    ) {
      let mut chunks = Chunks::default();
      let mut files = Files::default();
      let value = first_value(&mut chunks, &mut files, &first);
      let mut kept = vec![(first.clone(), value)];
      let mut added_before = 0usize;
      let mut added_last = 0usize;
      for raw in &chain {
        let (bytes, mut latest) = kept.pop().unwrap();
        let ops = ops_over(bytes.len() as u64, raw);
        let next = apply(&bytes, &ops, &post);
        let value = version(&mut chunks, &mut files, &mut latest, (&ops, &next));
        kept.push((bytes, latest));
        added_before += added_last;
        added_last = added_by(&ops);
        prop_assert!(chunks.owned_bytes() <= added_before, "owned {} for {} added before", chunks.owned_bytes(), added_before);
        prop_assert!(value.list_bytes() <= next.len().max(PIECE_BYTES));
        kept.push((next, value));
        for (bytes, value) in &kept {
          prop_assert!(value.equals((&chunks, files.blocks()), bytes));
          prop_assert_eq!(&value.materialize(&chunks, files.blocks()), bytes);
        }
        prop_assert_eq!(chunks.referenced_bytes(kept.iter().map(|(_, value)| value)), chunks.bytes());
      }
      for (_, value) in kept {
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
      let value = first_value(&mut chunks, &mut files, &first);
      let mut kept = vec![(first.clone(), value, Vec::new())];
      for (n, step) in chain.iter().enumerate() {
        let (bytes, mut latest, last_ops) = kept.pop().unwrap();
        let ops = ops_over(bytes.len() as u64, step);
        let next = apply(&bytes, &ops, &post);
        let made = if n == usize::from(whole_at) % chain.len() {
          files.insert("f", next.clone());
          Value::whole(&mut chunks, &next)
        } else {
          version(&mut chunks, &mut files, &mut latest, (&ops, &next))
        };
        kept.push((bytes, latest, last_ops));
        kept.push((next, made, ops));
      }
      let (head_bytes, head, head_ops) = kept.last().unwrap();
      // Converging: the version before the head, with the ops that made the head of it.
      let at = if converge { kept.len() - 2 } else { usize::from(pick) % kept.len() };
      let (base_bytes, base, _) = &kept[at];
      let ops = if converge {
        head_ops.clone()
      } else {
        ops_over(base_bytes.len() as u64, &raw)
      };
      let expected = apply(base_bytes, &ops, &post) == *head_bytes;
      prop_assert_eq!(base.applied_equals((&chunks, files.blocks()), (&ops, &post), head), expected);
    }
  }

  /// W2, A-728. Do: a 1 MiB file, then 64 versions each overwriting 64 bytes of its own range, made as the engine
  /// makes them; then release everything. Expect: the chunks hold the first file's block once (every version shares
  /// it), the 63 superseded versions' 64 bytes each (sealed: copied out once they were history) and the current file's
  /// block, which the latest names, charged its 64 added bytes; not 64 copies of the file, and no copy of the latest's
  /// bytes. Nothing is left after the release.
  #[test]
  fn small_overwrites_of_a_large_file_cost_what_they_wrote() {
    const FILE: usize = 1 << 20;
    const EDIT: u64 = 64;
    const EDITS: usize = 64;
    let mut chunks = Chunks::default();
    let mut files = Files::default();
    let mut bytes: Vec<u8> = (0..FILE).map(|n| (n % 251) as u8).collect();
    let mut values = vec![first_value(&mut chunks, &mut files, &bytes)];
    for n in 0..EDITS {
      let at = (n * FILE / EDITS) as u64;
      let post = vec![n as u8; EDIT as usize];
      let ops = [op_of(0, at, EDIT, 0)];
      let next = apply(&bytes, &ops, &post);
      let mut latest = values.pop().unwrap();
      let value = version(&mut chunks, &mut files, &mut latest, (&ops, &next));
      values.push(latest);
      values.push(value);
      bytes = next;
    }
    assert_eq!(chunks.owned_bytes(), (EDITS - 1) * EDIT as usize);
    // Charged: the first block whole, and every version's 64 bytes (the latest's chunk is its block, charged what it
    // added, as the copy it replaces was).
    assert_eq!(chunks.bytes(), FILE + EDITS * EDIT as usize);
    assert_eq!(
      files.blocks().live(),
      2,
      "the first file's block and the current one"
    );
    assert_eq!(
      values.last().unwrap().materialize(&chunks, files.blocks()),
      bytes
    );
    for value in values {
      value.release(&mut chunks, files.blocks_mut());
    }
    files.remove("f");
    assert_eq!((chunks.bytes(), files.blocks().live()), (0, 0));
  }

  /// A-728. Do: a file's block named as a value; a small overwrite made over it; then a whole replace of the result;
  /// then the file removed and every value released. Expect: naming copies no byte; the splice's added bytes are the
  /// new block's (nothing owned); the whole replace seals the splice (its 8 bytes copied, its block let go) and names
  /// the replacing file's block; every value reads back exactly, the first one after the file is gone; nothing is left
  /// after the release.
  #[test]
  fn a_value_named_by_a_files_block_copies_nothing_and_keeps_the_block_while_named() {
    let mut files = Files::default();
    let first: Vec<u8> = (0..4096u32).map(|n| (n % 251) as u8).collect();
    let mut chunks = Chunks::default();
    let mut named = first_value(&mut chunks, &mut files, &first);
    assert_eq!(chunks.owned_bytes(), 0, "naming the block copies nothing");
    assert_eq!(chunks.bytes(), first.len());
    let ops = [op_of(0, 100, 8, 0)];
    let next = apply(&first, &ops, &[7u8; 8]);
    let mut spliced = version(&mut chunks, &mut files, &mut named, (&ops, &next));
    assert_eq!(
      chunks.owned_bytes(),
      0,
      "the splice's added bytes are the new block's"
    );
    assert_eq!(spliced.materialize(&chunks, files.blocks()), next);
    let replace = [op_of(4, 0, 0, 0), op_of(3, 0, 16, 0)];
    let replaced = apply(&next, &replace, &[9u8; 16]);
    let third = version(&mut chunks, &mut files, &mut spliced, (&replace, &replaced));
    assert_eq!(
      chunks.owned_bytes(),
      8,
      "the superseded splice's 8 bytes copied"
    );
    assert_eq!(
      files.blocks().live(),
      2,
      "the first block (named whole) and the replacing file's"
    );
    assert_eq!(third.materialize(&chunks, files.blocks()), replaced);
    assert_eq!(spliced.materialize(&chunks, files.blocks()), next);
    files.remove("f");
    assert_eq!(named.materialize(&chunks, files.blocks()), first);
    for value in [named, spliced, third] {
      value.release(&mut chunks, files.blocks_mut());
    }
    assert_eq!((chunks.bytes(), files.blocks().live()), (0, 0));
  }

  /// A-728 part 2. Do: splice a small edit into a file's block, let another holder keep that block (as the identity
  /// table or a branch's map would), replace the file and seal the value; then let the other holder go and seal again.
  /// Expect: the first seal copies nothing (the bytes live anyway), the second cuts the value to its 4 added bytes, and
  /// the value reads back exactly both times.
  #[test]
  fn a_seal_keeps_a_block_another_holder_keeps() {
    let mut files = Files::default();
    let first = vec![1u8; 512];
    let mut chunks = Chunks::default();
    let mut latest = first_value(&mut chunks, &mut files, &first);
    let ops = [op_of(0, 10, 4, 0)];
    let next = apply(&first, &ops, &[2u8; 4]);
    let mut spliced = version(&mut chunks, &mut files, &mut latest, (&ops, &next));
    let held = files.hold_block_of("f").unwrap();
    files.insert("f", vec![3u8; 8]);
    assert_eq!(spliced.seal(&mut chunks, files.blocks_mut()), 0);
    assert_eq!(chunks.owned_bytes(), 0);
    assert_eq!(spliced.materialize(&chunks, files.blocks()), next);
    files.blocks_mut().release(held);
    assert_eq!(
      spliced.seal(&mut chunks, files.blocks_mut()),
      4,
      "cut once nothing else holds it"
    );
    assert_eq!(spliced.materialize(&chunks, files.blocks()), next);
    for value in [latest, spliced] {
      value.release(&mut chunks, files.blocks_mut());
    }
    files.remove("f");
    assert_eq!((chunks.bytes(), files.blocks().live()), (0, 0));
  }
}
