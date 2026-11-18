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
//! **Replicas reading in place (A-273).** A replica of the green on another shard names these chunks rather than
//! copying them (A-64: content once): each chunk is a sealed block in the green's store (`crate::store`,
//! `slates_mem::sealed`), the owner's own, read in place by its replicas through their own tables of the same chunks
//! ([`Chunks::replica`]). The owner's last piece leaving a chunk releases its slot here at once and its block in the
//! store, which keeps the block (its bytes unchanged, its room unreused) until every replica has passed the feed message
//! that released it, and charges it to the green's retained bytes meanwhile ([`Chunks::held_bytes`]). Positions, not
//! versions: a fold of the history releases chunks with no new version. With no replica a release frees at once, as it
//! always did.

use crate::ops_doc::{Op, OpKind};
use slates_mem::sealed::{Block, Changes, Left, Passed, ReaderRefusal};

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

/// The arena of immutable chunks a green's history values share (module docs): each chunk a sealed block in the
/// green's store (`crate::store`), its owner's own or, on a replica, its owner's read in place.
#[derive(Debug, Default)]
pub(crate) struct Chunks {
  /// Where the chunks' bytes are, and where the green's current values are too (A-273 step 3).
  store: crate::store::Store,
  /// Each slot's chunk; `None` once released.
  slots: Vec<Option<Entry>>,
  /// Released slots, reused first.
  free: Vec<usize>,
  /// The bytes the live chunks hold.
  bytes: usize,
}

/// A live chunk: its block, its length and the pieces naming it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
  block: Block,
  len: usize,
  refs: usize,
}

impl Chunks {
  /// The bytes the live chunks hold.
  pub(crate) fn bytes(&self) -> usize {
    self.bytes
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
      .map(|entry| entry.len)
      .fold(0, usize::saturating_add)
  }

  /// Adds a chunk holding `bytes`, named by no piece yet.
  fn add(&mut self, bytes: Vec<u8>) -> usize {
    let len = bytes.len();
    let block = self.store.seal(bytes);
    self.bytes = self.bytes.saturating_add(len);
    let entry = Some(Entry {
      block,
      len,
      refs: 0,
    });
    if let Some(chunk) = self.free.pop()
      && let Some(slot) = self.slots.get_mut(chunk)
    {
      *slot = entry;
      return chunk;
    }
    self.slots.push(entry);
    self.slots.len().saturating_sub(1)
  }

  /// One more piece names `chunk`.
  fn retain(&mut self, chunk: usize) {
    if let Some(Some(entry)) = self.slots.get_mut(chunk) {
      entry.refs = entry.refs.saturating_add(1);
    }
  }

  /// One piece naming `chunk` is gone; the chunk is released with its last, its block released in the store (kept there
  /// for the green's replicas until each has passed this release, A-273) and its slot reused.
  fn release(&mut self, chunk: usize) {
    let Some(slot) = self.slots.get_mut(chunk) else {
      return;
    };
    let Some(entry) = slot.as_mut() else {
      return;
    };
    entry.refs = entry.refs.saturating_sub(1);
    if entry.refs > 0 {
      return;
    }
    let (block, len) = (entry.block, entry.len);
    *slot = None;
    self.free.push(chunk);
    self.bytes = self.bytes.saturating_sub(len);
    self.store.release(block);
  }

  /// A current value's block holding `bytes` (the green's current files live in the store beside its chunks).
  pub(crate) fn seal(&mut self, bytes: Vec<u8>) -> Block {
    self.store.seal(bytes)
  }

  /// A current value's bytes, read where the store keeps them.
  pub(crate) fn block_bytes(&self, block: Block) -> &[u8] {
    self.store.bytes(block)
  }

  /// A current value the green names no more.
  pub(crate) fn release_block(&mut self, block: Block) {
    self.store.release(block);
  }

  /// The feed position: on the owner, the position of its next message, which a release is stamped with; on a replica,
  /// the next message it applies (A-273).
  pub(crate) fn position(&self) -> u64 {
    self.store.position()
  }

  /// The owner's blocks sealed and released since its last feed message, at its position, which then moves past it.
  pub(crate) fn take_changes(&mut self) -> Option<Changes> {
    self.store.take_changes()
  }

  /// A replica of these chunks now (A-273): registered with the owner's blocks, every chunk named as the owner names it,
  /// read in place. Refused past `limit` replicas, and on a replica, which has no blocks of its own to give.
  pub(crate) fn replica(&mut self, limit: usize) -> Result<Chunks, crate::replica::ReplicaRefusal> {
    let joined = self.store.join(limit)?;
    Ok(Chunks {
      store: crate::store::Store::reading(joined),
      slots: self.slots.clone(),
      free: self.free.clone(),
      bytes: self.bytes,
    })
  }

  /// A replica passed a feed message: the owner frees what every replica has passed.
  pub(crate) fn acknowledge(&mut self, passed: Passed) {
    self.store.acknowledge(passed);
  }

  /// A replica is done: the owner keeps nothing for it.
  pub(crate) fn leave(&mut self, left: Left) {
    self.store.leave(left);
  }

  /// A replica's registration given back for its owner's [`Chunks::leave`].
  pub(crate) fn into_left(self) -> Option<Left> {
    self.store.into_left()
  }

  /// A replica about to replay the message carrying `changes` (`crate::store::Store::begin`).
  pub(crate) fn begin(&mut self, changes: &Changes) -> Result<(), ReaderRefusal> {
    self.store.begin(changes)
  }

  /// The bytes the owner keeps for its replicas past which the slowest is evicted (A-273 step 4).
  pub(crate) fn set_held_limit(&mut self, bytes: usize) {
    self.store.set_held_limit(bytes);
  }

  /// At the start of a change on the owner: every replica evicted if the last change was never sent to them.
  pub(crate) fn evict_if_unfed(&mut self) {
    self.store.evict_if_unfed();
  }

  /// Replicas the owner evicted: for what it kept for them, and for changes left unfed.
  pub(crate) fn evictions(&self) -> (u64, u64) {
    self.store.evictions()
  }

  /// Whether `changes` says the owner evicted this replica.
  pub(crate) fn evicted_by(&self, changes: &Changes) -> bool {
    self.store.evicted_by(changes)
  }

  /// The bytes a replica's own table of these chunks costs (A-273 step 4): its slots.
  pub(crate) fn table_bytes(&self) -> usize {
    self
      .slots
      .len()
      .saturating_add(self.free.len())
      .saturating_mul(size_of::<Option<Entry>>())
  }

  /// A replica done replaying it (`crate::store::Store::finish`).
  pub(crate) fn finish(&mut self, changes: &Changes) -> Option<Passed> {
    self.store.finish(changes)
  }

  /// The replicas registered with these chunks' blocks (A-273), evicted ones that have not left among them.
  pub(crate) fn replicas(&self) -> usize {
    self.store.replicas()
  }

  /// The bytes of chunks and values kept on the owner only for its replicas (A-273): its memory, charged with its own.
  pub(crate) fn held_bytes(&self) -> usize {
    self.store.held_bytes()
  }

  /// [`Chunks::held_bytes`] recounted from the owner's kept list itself.
  pub(crate) fn held_bytes_recounted(&self) -> usize {
    self.store.held_bytes_recounted()
  }

  /// The frees kept for a replica since the green was made (A-273's non-vacuity counter).
  pub(crate) fn frees_held(&self) -> u64 {
    self.store.frees_held()
  }

  /// Reads the store could not answer (`crate::store`): never on a green whose replay is its owner's.
  pub(crate) fn reads_refused(&self) -> u64 {
    self.store.reads_refused()
  }

  /// Whether these chunks are a replica's reading of its owner's.
  pub(crate) fn is_replica(&self) -> bool {
    self.store.is_replica()
  }

  /// The bytes of `piece`, read where its chunk's block is.
  fn bytes_of(&self, piece: &Piece) -> &[u8] {
    self
      .slots
      .get(piece.chunk)
      .and_then(Option::as_ref)
      .map(|entry| self.store.bytes(entry.block))
      .and_then(|bytes| bytes.get(piece.at..piece.at.saturating_add(piece.len)))
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

  /// `bytes` as one new chunk (none for no bytes).
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
  pub(crate) fn materialize(&self, chunks: &Chunks) -> Vec<u8> {
    let mut out = Vec::with_capacity(self.len);
    for piece in &self.pieces {
      out.extend_from_slice(chunks.bytes_of(piece));
    }
    out
  }

  /// The value's bytes in `[from, to)` (clamped to its length), copied out of only the pieces that hold them: what a
  /// paged read of an earlier version takes, without the whole file.
  pub(crate) fn read(&self, chunks: &Chunks, from: usize, to: usize) -> Vec<u8> {
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
      let run = chunks.bytes_of(piece);
      let lo = from.max(start).saturating_sub(start);
      let hi = to.min(end).saturating_sub(start);
      out.extend_from_slice(run.get(lo..hi).unwrap_or_default());
    }
    out
  }

  /// Whether the value's bytes are `bytes`, compared piece by piece without a copy.
  pub(crate) fn equals(&self, chunks: &Chunks, bytes: &[u8]) -> bool {
    if self.len != bytes.len() {
      return false;
    }
    let mut at = 0usize;
    for piece in &self.pieces {
      let run = chunks.bytes_of(piece);
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
    chunks: &Chunks,
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
              || chunks.bytes_of(&Piece { len: step, ..mine })
                == chunks.bytes_of(&Piece { len: step, ..piece })
          }
          Applied::Post { src, .. } => {
            post_state.get(src..src.saturating_add(step))
              == Some(chunks.bytes_of(&Piece { len: step, ..piece }))
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

  /// Gives the value's chunks back: each piece's reference released.
  pub(crate) fn release(self, chunks: &mut Chunks) {
    for piece in self.pieces {
      chunks.release(piece.chunk);
    }
  }

  /// The value `next` that `ops` (a path's accepted ops in this value's coordinates, as the engine's `apply` takes
  /// them) make of this one, sharing every run of this value they keep and copying only the bytes they add (module
  /// docs). Stored whole when the splice does not reproduce `next` or its pieces would outweigh its bytes.
  pub(crate) fn spliced(&self, chunks: &mut Chunks, ops: &[Op], next: &[u8]) -> Value {
    let Some(pending) = self.walk(ops, next.len()) else {
      return Value::whole(chunks, next);
    };
    if pending.len().saturating_mul(PIECE_BYTES) > next.len() {
      return Value::whole(chunks, next);
    }
    let mut added = Vec::new();
    for run in &pending {
      if let Pending::Added { at, len } = run {
        match next.get(*at..at.saturating_add(*len)) {
          Some(bytes) => added.extend_from_slice(bytes),
          None => return Value::whole(chunks, next),
        }
      }
    }
    let new_chunk = (!added.is_empty()).then(|| chunks.add(added));
    let mut value = Value::default();
    let mut in_new = 0usize;
    for run in pending {
      let piece = match run {
        Pending::Shared(piece) => piece,
        Pending::Added { len, .. } => {
          let Some(chunk) = new_chunk else {
            continue;
          };
          let piece = Piece {
            chunk,
            at: in_new,
            len,
          };
          in_new = in_new.saturating_add(len);
          piece
        }
      };
      value.push(chunks, piece);
    }
    if !value.equals(chunks, next) {
      value.release(chunks);
      return Value::whole(chunks, next);
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

#[cfg(test)]
mod tests {
  //! The history's oracle: over generated chains of ops on one file, every version's value materializes to exactly
  //! what the engine's `apply` made of the version before it; the chunks a chain holds are its pieces' chunks and no
  //! more (released values free them); a version that changes a few bytes of a large file costs those bytes and its
  //! list, not the file; and compaction keeps every version's cost within a whole copy.
  #![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
  )]

  use super::*;
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

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// W2. Do: from a generated first value, apply generated op chains version after version, each splicing the
    /// history's previous value and checking every value kept so far; then release every value. Expect: every
    /// version materializes to `apply`'s bytes; each version's added chunk bytes are at most its ops' added bytes
    /// (or, when stored whole, the value); every list costs at most its value's bytes; the arena's live bytes are
    /// exactly what the kept values' pieces name; and after the release the arena holds nothing.
    #[test]
    fn every_version_is_apply_and_shares_what_it_kept(
      first in prop::collection::vec(any::<u8>(), 0..512),
      chain in prop::collection::vec(prop::collection::vec((any::<u8>(), any::<u16>(), any::<u16>(), any::<u16>()), 0..6), 1..12),
      post in prop::collection::vec(any::<u8>(), 256..512),
    ) {
      let mut chunks = Chunks::default();
      let mut kept = vec![(first.clone(), Value::whole(&mut chunks, &first))];
      for raw in &chain {
        let (bytes, value) = kept.last().unwrap();
        let ops = ops_over(bytes.len() as u64, raw);
        let next = apply(bytes, &ops, &post);
        let before = chunks.bytes();
        let spliced = value.spliced(&mut chunks, &ops, &next);
        let added: u64 = ops
          .iter()
          .filter(|op| matches!(op.kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend))
          .map(|op| op.len)
          .sum();
        let grew = chunks.bytes() - before;
        prop_assert!(grew as u64 <= added || grew == next.len(), "grew {} for {} added", grew, added);
        prop_assert!(spliced.list_bytes() <= next.len().max(PIECE_BYTES));
        kept.push((next, spliced));
        for (bytes, value) in &kept {
          prop_assert!(value.equals(&chunks, bytes));
          prop_assert_eq!(&value.materialize(&chunks), bytes);
        }
      }
      for (_, value) in kept {
        value.release(&mut chunks);
      }
      prop_assert_eq!(chunks.bytes(), 0);
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// §4.16 "The verdict" (the whole-file identity check). Do: from a generated first value, build a head by a
    /// generated chain of versions (each spliced from the one before, or, at a generated version, stored whole from
    /// its bytes so it shares nothing); pick a generated base among the versions and generated ops over it, the
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
      let mut kept = vec![(first.clone(), Value::whole(&mut chunks, &first), Vec::new())];
      for (n, step) in chain.iter().enumerate() {
        let (bytes, value, _) = kept.last().unwrap();
        let ops = ops_over(bytes.len() as u64, step);
        let next = apply(bytes, &ops, &post);
        let made = if n == usize::from(whole_at) % chain.len() {
          Value::whole(&mut chunks, &next)
        } else {
          value.spliced(&mut chunks, &ops, &next)
        };
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
      prop_assert_eq!(base.applied_equals(&chunks, (&ops, &post), head), expected);
    }
  }

  /// W2. Do: a 1 MiB value, then 64 versions each overwriting 64 bytes of its own range. Expect: the chunks hold the
  /// file once plus the 64 × 64 bytes the versions added, not 64 copies of the file.
  #[test]
  fn small_overwrites_of_a_large_file_cost_what_they_wrote() {
    const FILE: usize = 1 << 20;
    const EDIT: u64 = 64;
    const EDITS: usize = 64;
    let mut chunks = Chunks::default();
    let mut bytes: Vec<u8> = (0..FILE).map(|n| (n % 251) as u8).collect();
    let mut values = vec![Value::whole(&mut chunks, &bytes)];
    for n in 0..EDITS {
      let at = (n * FILE / EDITS) as u64;
      let post = vec![n as u8; EDIT as usize];
      let ops = [op_of(0, at, EDIT, 0)];
      let next = apply(&bytes, &ops, &post);
      let value = values.last().unwrap().spliced(&mut chunks, &ops, &next);
      values.push(value);
      bytes = next;
    }
    assert_eq!(chunks.bytes(), FILE + EDITS * EDIT as usize);
    assert_eq!(values.last().unwrap().materialize(&chunks), bytes);
  }

  /// Shape: the replicas a test's owner takes, more than any test makes.
  const READERS: usize = 4;

  /// `value` read through `chunks`: `bytes`.
  fn reads(value: &Value, chunks: &Chunks, bytes: &[u8]) {
    assert_eq!(value.materialize(chunks), bytes);
  }

  /// The bytes the owner's chunks keep for its replicas: `held`, equal to their recount.
  fn keeps(chunks: &Chunks, held: usize) {
    assert_eq!(
      (chunks.held_bytes(), chunks.held_bytes_recounted()),
      (held, held)
    );
  }

  /// A-273 "Frees deferred until every replica has passed them", step 3. Do: an owner's value of 64 bytes; a replica's
  /// chunks are made from the owner's, so its copy of the value reads the owner's block in place; the owner releases
  /// its value and stores another of other bytes, and takes the feed message's changes; the replica replays the same
  /// two steps, adopting the owner's new block, and passes the message. Expect: until the replica passes, its copy reads
  /// its 64 bytes and the free is held and charged; the owner frees the block at the replica's acknowledgement, not
  /// before; the replica's new value reads the owner's new bytes in place, and the replica sealed nothing of its own.
  #[test]
  fn a_chunk_released_while_a_replica_reads_it_stays_until_the_replica_passes_its_release() {
    let mut chunks = Chunks::default();
    let first = vec![7u8; 64];
    let owner = Value::whole(&mut chunks, &first);
    let mut replica_chunks = chunks.replica(READERS).unwrap();
    let replica = owner.clone();
    owner.release(&mut chunks);
    let second = Value::whole(&mut chunks, &[9u8; 64]);
    let changes = chunks.take_changes().unwrap();
    // The replica reads the bytes it named, in place, and the owner keeps them for it.
    reads(&replica, &replica_chunks, &first);
    assert_eq!(chunks.frees_held(), 1);
    keeps(&chunks, first.len());
    replica_chunks.begin(&changes).unwrap();
    replica.release(&mut replica_chunks);
    let adopted = Value::whole(&mut replica_chunks, &[9u8; 64]);
    let passed = replica_chunks.finish(&changes).unwrap();
    // Passed, but not yet acknowledged: still kept; freed at the acknowledgement.
    keeps(&chunks, first.len());
    chunks.acknowledge(passed);
    keeps(&chunks, 0);
    reads(&adopted, &replica_chunks, &[9u8; 64]);
    assert_eq!(adopted, second, "the replica names the owner's chunk");
    assert_eq!(replica_chunks.reads_refused(), 0);
    chunks.leave(replica_chunks.into_left().unwrap());
    second.release(&mut chunks);
    assert_eq!(chunks.bytes(), 0);
  }

  /// A-273. Do: as above, but the replica leaves (dropped, rebuilt or ended) instead of acknowledging. Expect: the
  /// held chunk is freed at the leave, and a later release with no replica is freed at once, nothing held.
  #[test]
  fn a_replica_that_leaves_holds_no_free() {
    let mut chunks = Chunks::default();
    let owner = Value::whole(&mut chunks, &[1u8; 32]);
    let replica = chunks.replica(READERS).unwrap();
    owner.release(&mut chunks);
    assert_eq!(chunks.held_bytes(), 32);
    chunks.leave(replica.into_left().unwrap());
    assert_eq!(chunks.held_bytes(), 0);
    let later = Value::whole(&mut chunks, &[2u8; 32]);
    later.release(&mut chunks);
    assert_eq!(
      (chunks.bytes(), chunks.held_bytes(), chunks.frees_held()),
      (0, 0, 1)
    );
  }

  /// A-273 step 3: a replica whose replay seals bytes the owner did not seal, or seals more blocks than the owner did,
  /// is refused at the message's end: it is not the owner's green.
  #[test]
  fn a_replay_that_seals_other_bytes_than_its_owner_is_refused() {
    let mut chunks = Chunks::default();
    let mut replica_chunks = chunks.replica(READERS).unwrap();
    let owner = Value::whole(&mut chunks, &[3u8; 16]);
    let changes = chunks.take_changes().unwrap();
    replica_chunks.begin(&changes).unwrap();
    let other = Value::whole(&mut replica_chunks, &[4u8; 16]);
    assert!(replica_chunks.finish(&changes).is_none(), "other bytes");
    let mut twice = chunks.replica(READERS).unwrap();
    let again = Value::whole(&mut chunks, &[5u8; 8]);
    let changes = chunks.take_changes().unwrap();
    twice.begin(&changes).unwrap();
    let first = Value::whole(&mut twice, &[5u8; 8]);
    let extra = Value::whole(&mut twice, &[6u8; 8]);
    assert!(
      twice.finish(&changes).is_none(),
      "more blocks than the owner"
    );
    drop((other, first, extra));
    chunks.leave(replica_chunks.into_left().unwrap());
    chunks.leave(twice.into_left().unwrap());
    owner.release(&mut chunks);
    again.release(&mut chunks);
    assert_eq!((chunks.bytes(), chunks.held_bytes()), (0, 0));
  }
}
