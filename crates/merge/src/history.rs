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

/// The arena of immutable chunks a green's history values share (module docs).
#[derive(Debug, Default)]
pub(crate) struct Chunks {
  /// Each slot's bytes and the pieces naming it; `None` once freed.
  slots: Vec<Option<(Vec<u8>, usize)>>,
  /// Freed slots, reused first.
  free: Vec<usize>,
  /// The bytes the live chunks hold.
  bytes: usize,
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
      .map(|(bytes, _)| bytes.len())
      .sum()
  }

  /// Adds a chunk holding `bytes`, named by no piece yet.
  fn add(&mut self, bytes: Vec<u8>) -> usize {
    let len = bytes.len();
    self.bytes = self.bytes.saturating_add(len);
    if let Some(chunk) = self.free.pop()
      && let Some(slot) = self.slots.get_mut(chunk)
    {
      *slot = Some((bytes, 0));
      return chunk;
    }
    self.slots.push(Some((bytes, 0)));
    self.slots.len().saturating_sub(1)
  }

  /// One more piece names `chunk`.
  fn retain(&mut self, chunk: usize) {
    if let Some(Some((_, refs))) = self.slots.get_mut(chunk) {
      *refs = refs.saturating_add(1);
    }
  }

  /// One piece naming `chunk` is gone; the chunk is freed with its last.
  fn release(&mut self, chunk: usize) {
    let Some(slot) = self.slots.get_mut(chunk) else {
      return;
    };
    let freed = match slot {
      Some((bytes, refs)) => {
        *refs = refs.saturating_sub(1);
        (*refs == 0).then_some(bytes.len())
      }
      None => None,
    };
    if let Some(len) = freed {
      *slot = None;
      self.bytes = self.bytes.saturating_sub(len);
      self.free.push(chunk);
    }
  }

  /// The bytes of `piece`, or none if its chunk is gone (never, while a value names it).
  fn bytes_of(&self, piece: &Piece) -> &[u8] {
    self
      .slots
      .get(piece.chunk)
      .and_then(Option::as_ref)
      .and_then(|(bytes, _)| bytes.get(piece.at..piece.at.saturating_add(piece.len)))
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

impl Value {
  /// The value's length.
  pub(crate) fn len(&self) -> usize {
    self.len
  }

  /// The bytes its piece list costs.
  pub(crate) fn list_bytes(&self) -> usize {
    self.pieces.len().saturating_mul(PIECE_BYTES)
  }

  /// The pieces it has.
  pub(crate) fn pieces(&self) -> usize {
    self.pieces.len()
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
}
