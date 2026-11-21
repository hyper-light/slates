//! The content deriver (§4.16 "Composition at seal", "Declared operations"; D-27). One path's
//! declared content operations — recorded in the journal in the order they happened (§4.5) —
//! compose by interval algebra into the canonical net op set relative to the base version's
//! content. Overlapping overwrites merge into one; an insert followed by an overlapping delete
//! cancels or splits; a truncate cancels operations beyond the new length; a whole-file rewrite
//! composes to one delete of the base length and one insert of the new bytes.
//!
//! Composition is arithmetic on the declared ranges. Comparing file states to reconstruct
//! operations is inference and does not exist here (D-27, the never-diff clause): the deriver
//! never reads a content byte. The sealed post-state — the work volume's final file content —
//! is `slates`' ground truth for the bytes an operation adds, so every net op that adds content
//! names its bytes by their offset in that post-state (`src`), which the splice resolves later
//! (§4.16 "Splice"). Because the post-state is the final file, the deriver assigns `src` as the
//! byte's offset in the composed result, computed from the declared ranges alone.
//!
//! The same journal yields the same net ops on every platform — "its identity is the test"
//! (the ops document of [`crate::ops_doc`] serializes them, and its BLAKE3 is the increment's
//! identity). This module is pure: no I/O, no clock, no randomness.
//!
//! Shape: composition tracks the file as a list of pieces in current-file coordinates, each a
//! run of surviving base bytes or a run of new bytes; every declared op splits and rewrites the
//! list at its coordinates, and the readout walks the final list once to emit the net ops. The
//! list is kept in blocks of about `√ops` pieces (`PieceList`), so an op finds its offset in
//! about `√pieces` steps, and adjacent new runs merge as they are made. Measured, a flat list
//! walked from its front made a 128 MiB file written a few KiB a write compose in over a second of
//! one shard's CPU, and the watchdog ended the daemon (A-697's release bench, 2026-11-20).

use crate::ops_doc::{Op, OpKind};

/// A declared content operation on one path, in the order it was recorded in the journal
/// (§4.5). Coordinates are current-file at the moment the operation happened. Bytes are never
/// carried: the deriver composes ranges and names added content by its post-state offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentOp {
  /// A `write` that stays within the file: the current range `[at, at + len)` is replaced in
  /// place by `len` new bytes (the size does not change).
  Overwrite {
    /// The current-file offset the write starts at.
    at: u64,
    /// The bytes written.
    len: u64,
  },
  /// A `write` that reaches past the current end: `len` new bytes are appended (`at` is the old
  /// end). Treated as an insertion at the end during composition.
  Extend {
    /// The current end (where the appended bytes begin).
    at: u64,
    /// The bytes appended.
    len: u64,
  },
  /// A `truncate` to `len`: bytes beyond it are dropped; a `len` past the current end grows the
  /// file with zero bytes (which are real content in the post-state).
  Truncate {
    /// The new length.
    len: u64,
  },
  /// An `edit`'s inserted bytes: `len` new bytes are inserted at `at`, shifting the rest right.
  Insert {
    /// The current-file offset the bytes are inserted at.
    at: u64,
    /// The bytes inserted.
    len: u64,
  },
  /// An `edit`'s deleted bytes: the current range `[at, at + len)` is removed, shifting the
  /// rest left.
  Delete {
    /// The current-file offset the removal starts at.
    at: u64,
    /// The bytes removed.
    len: u64,
  },
}

/// One run of the composed file, in current-file coordinates. A `Base` run is bytes that
/// survive unchanged from the base version (named by their base offset); a `New` run is bytes an
/// operation added (their post-state offset is their offset in the final file, computed at
/// readout).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Piece {
  /// Surviving base bytes: `len` of them, starting at `base_at` in the base version.
  Base {
    /// The offset in the base version these bytes start at.
    base_at: u64,
    /// The run length.
    len: u64,
  },
  /// New bytes an operation added: `len` of them (the post-state offset is the run's position in
  /// the final file).
  New {
    /// The run length.
    len: u64,
  },
}

impl Piece {
  /// The run length.
  fn len(self) -> u64 {
    match self {
      Piece::Base { len, .. } | Piece::New { len } => len,
    }
  }

  /// Splits the run at `offset` bytes from its start into a left and a right run.
  fn split(self, offset: u64) -> (Piece, Piece) {
    match self {
      Piece::Base { base_at, len } => (
        Piece::Base {
          base_at,
          len: offset,
        },
        Piece::Base {
          base_at: base_at.saturating_add(offset),
          len: len.saturating_sub(offset),
        },
      ),
      Piece::New { len } => (
        Piece::New { len: offset },
        Piece::New {
          len: len.saturating_sub(offset),
        },
      ),
    }
  }
}

/// The composed file as a list of pieces kept in blocks (A-697's release bench, exit 4 at 128 MiB, 2026-11-20): each
/// block knows its pieces' total length, so an offset is found by walking the blocks, then the pieces of one block,
/// about `√pieces` steps an op where a flat list took `pieces`. Adjacent new runs are merged as they are made, since a
/// new run is a length alone (its post-state offset is its position at readout), so a file written by appends is one
/// run however many writes made it. Before, every op walked the flat list from its front and inserted into it: a 128
/// MiB file written a few KiB a write composed in over a second of one shard's CPU, and the watchdog ended the daemon.
struct PieceList {
  /// The blocks, in file order; none is empty.
  blocks: Vec<Block>,
  /// The file's current length: every piece's.
  total: u64,
  /// The most pieces a block keeps before it is split in two: derived from the work, about the square root of the
  /// journal's length ([`PieceList::for_journal`]), so a block walk and a piece walk cost alike.
  block_pieces: usize,
  /// Blocks and pieces examined across every op: the witness that an op's cost is bounded by `√pieces`, not `pieces`.
  examined: u64,
}

/// One block of a [`PieceList`].
struct Block {
  pieces: Vec<Piece>,
  len: u64,
}

impl Block {
  /// A block of `pieces`, its length summed.
  fn of(pieces: Vec<Piece>) -> Block {
    let len = pieces.iter().map(|piece| piece.len()).sum();
    Block { pieces, len }
  }
}

impl PieceList {
  /// The list for a journal of `ops` over `base_len` bytes of base content: one base run, its blocks sized from the
  /// journal (`⌈√(ops + 1)⌉` pieces, at least one).
  fn for_journal(base_len: u64, ops: usize) -> PieceList {
    let block_pieces = ops.saturating_add(1).isqrt().max(1);
    let mut list = PieceList {
      blocks: Vec::new(),
      total: 0,
      block_pieces,
      examined: 0,
    };
    if base_len > 0 {
      list.blocks.push(Block::of(vec![Piece::Base {
        base_at: 0,
        len: base_len,
      }]));
      list.total = base_len;
    }
    list
  }

  /// Ensures a piece boundary at `offset` (clamped to the end), splitting the piece that straddles it, and returns
  /// where the piece beginning there is: its block and its index in that block; at the end, the last block and its
  /// length (or `(0, 0)` for an empty list).
  fn split_at(&mut self, offset: u64) -> (usize, usize) {
    let offset = offset.min(self.total);
    let mut acc = 0u64;
    for (block_at, block) in self.blocks.iter_mut().enumerate() {
      self.examined = self.examined.saturating_add(1);
      let end = acc.saturating_add(block.len);
      if offset >= end {
        acc = end;
        continue;
      }
      let mut piece_acc = acc;
      let mut index = 0;
      while let Some(piece) = block.pieces.get_mut(index) {
        self.examined = self.examined.saturating_add(1);
        if piece_acc == offset {
          return (block_at, index);
        }
        let piece_len = piece.len();
        if piece_acc.saturating_add(piece_len) > offset {
          let (left, right) = piece.split(offset.saturating_sub(piece_acc));
          *piece = left;
          let after = index.saturating_add(1);
          block.pieces.insert(after, right);
          return (block_at, after);
        }
        piece_acc = piece_acc.saturating_add(piece_len);
        index = index.saturating_add(1);
      }
      return (block_at, block.pieces.len());
    }
    match self.blocks.len().checked_sub(1) {
      Some(last) => (
        last,
        self.blocks.get(last).map_or(0, |block| block.pieces.len()),
      ),
      None => (0, 0),
    }
  }

  /// Inserts `len` new bytes at `offset`, merged into the new run just before it when there is one.
  fn insert_new(&mut self, offset: u64, len: u64) {
    if len == 0 {
      return;
    }
    let (block_at, index) = self.split_at(offset);
    self.total = self.total.saturating_add(len);
    if self.blocks.is_empty() {
      self.blocks.push(Block::of(vec![Piece::New { len }]));
      return;
    }
    // The run just before the insertion point: the previous piece in this block, or the last of the block before.
    let before = match index.checked_sub(1) {
      Some(previous) => Some((block_at, previous)),
      None => block_at.checked_sub(1).and_then(|previous_block| {
        self
          .blocks
          .get(previous_block)
          .and_then(|block| block.pieces.len().checked_sub(1))
          .map(|last| (previous_block, last))
      }),
    };
    if let Some((at_block, at_piece)) = before
      && let Some(block) = self.blocks.get_mut(at_block)
      && let Some(Piece::New { len: run }) = block.pieces.get_mut(at_piece)
    {
      *run = run.saturating_add(len);
      block.len = block.len.saturating_add(len);
      return;
    }
    let Some(block) = self.blocks.get_mut(block_at) else {
      return;
    };
    block.pieces.insert(index, Piece::New { len });
    block.len = block.len.saturating_add(len);
    self.rebalance(block_at);
  }

  /// Removes `[from, to)`.
  fn remove(&mut self, from: u64, to: u64) {
    let to = to.min(self.total);
    if from >= to {
      return;
    }
    let (first_block, first_index) = self.split_at(from);
    // Splitting at `to` inserts at or after `first_index` in the same block, so the first position stands.
    let (last_block, last_index) = self.split_at(to);
    if first_block == last_block {
      if let Some(block) = self.blocks.get_mut(first_block) {
        block
          .pieces
          .drain(first_index..last_index.min(block.pieces.len()));
        *block = Block::of(std::mem::take(&mut block.pieces));
      }
    } else {
      if let Some(block) = self.blocks.get_mut(last_block) {
        block.pieces.drain(..last_index.min(block.pieces.len()));
        *block = Block::of(std::mem::take(&mut block.pieces));
      }
      if let Some(block) = self.blocks.get_mut(first_block) {
        block.pieces.truncate(first_index);
        *block = Block::of(std::mem::take(&mut block.pieces));
      }
      let between = first_block.saturating_add(1)..last_block;
      if between.start < between.end && between.end <= self.blocks.len() {
        self.blocks.drain(between);
      }
    }
    self.total = self.total.saturating_sub(to.saturating_sub(from));
    self.blocks.retain(|block| !block.pieces.is_empty());
  }

  /// Truncates to `len`, or grows to it with new bytes.
  fn truncate(&mut self, len: u64) {
    if len < self.total {
      self.remove(len, self.total);
    } else if len > self.total {
      let grow = len.saturating_sub(self.total);
      self.insert_new(self.total, grow);
    }
  }

  /// Splits every block holding more than twice the block size: an op's splits add at most a few pieces, so after
  /// each op every block is back within its bound.
  fn rebalance_all(&mut self) {
    let mut block_at = 0;
    while block_at < self.blocks.len() {
      self.examined = self.examined.saturating_add(1);
      self.rebalance(block_at);
      block_at = block_at.saturating_add(1);
    }
  }

  /// Splits block `block_at` in two once it holds more than twice the block size.
  fn rebalance(&mut self, block_at: usize) {
    let Some(block) = self.blocks.get_mut(block_at) else {
      return;
    };
    if block.pieces.len() <= self.block_pieces.saturating_mul(2) {
      return;
    }
    let tail = block.pieces.split_off(self.block_pieces);
    *block = Block::of(std::mem::take(&mut block.pieces));
    self
      .blocks
      .insert(block_at.saturating_add(1), Block::of(tail));
  }

  /// The pieces, in file order.
  fn pieces(&self) -> Vec<Piece> {
    self
      .blocks
      .iter()
      .flat_map(|block| block.pieces.iter().copied())
      .collect()
  }
}

/// Applies one declared operation to the piece list, in current-file coordinates.
fn apply(pieces: &mut PieceList, op: &ContentOp) {
  match *op {
    ContentOp::Overwrite { at, len } => {
      if len == 0 {
        return;
      }
      pieces.remove(at, at.saturating_add(len));
      pieces.insert_new(at, len);
    }
    ContentOp::Extend { at, len } | ContentOp::Insert { at, len } => pieces.insert_new(at, len),
    ContentOp::Delete { at, len } => {
      if len == 0 {
        return;
      }
      pieces.remove(at, at.saturating_add(len));
    }
    ContentOp::Truncate { len } => pieces.truncate(len),
  }
}

/// One net op, before it is placed in an ops document (its path index is filled by the
/// assembler that composes every path; `flags` is zero).
fn op(kind: OpKind, at: u64, len: u64, src: u64) -> Op {
  Op {
    kind,
    flags: 0,
    path: 0,
    at,
    len,
    src,
  }
}

/// Composes one path's declared content operations, in journal order, into the canonical net op
/// set relative to `base_len` bytes of base content. The net ops are in base coordinates and,
/// for content they add, name the bytes by their post-state offset (`src`); a `Delete` and a
/// `Truncate` add no content and carry `src = u64::MAX`.
///
/// The net set is minimal and canonical: an in-place replacement of equal length is one
/// `Overwrite`; a replacement of unequal length is a `Delete` then an `Insert` (a whole-file
/// rewrite is the whole-length case); bytes added past the base end are an `Extend`, added
/// within it an `Insert`; a removal that runs to the base end is a `Truncate`, one within it a
/// `Delete`.
pub fn compose_content(base_len: u64, journal: &[ContentOp]) -> Vec<Op> {
  compose_content_sized(base_len, journal).0
}

/// Composes as [`compose_content`] does and also returns the final content length (the bytes the
/// path holds at seal). The multi-path assembler needs it to lay out the increment's post-state:
/// each path occupies a contiguous region of the post-state, and an op's `src` is its offset
/// within that region plus the region's base offset.
pub fn compose_content_sized(base_len: u64, journal: &[ContentOp]) -> (Vec<Op>, u64) {
  let (ops, final_len, _) = compose_counted(base_len, journal);
  (ops, final_len)
}

/// [`compose_content_sized`] with the blocks and pieces its ops examined ([`PieceList`]'s witness).
fn compose_counted(base_len: u64, journal: &[ContentOp]) -> (Vec<Op>, u64, u64) {
  let mut pieces = PieceList::for_journal(base_len, journal.len());
  for declared in journal {
    apply(&mut pieces, declared);
    pieces.rebalance_all();
  }
  let final_len = pieces.total;
  (
    read_out(base_len, &pieces.pieces()),
    final_len,
    pieces.examined,
  )
}

/// [`compose_content_sized`]'s count of the blocks and pieces its ops examined, for a test to hold its cost to its
/// bound.
#[doc(hidden)]
pub fn compose_examined(base_len: u64, journal: &[ContentOp]) -> u64 {
  compose_counted(base_len, journal).2
}

/// Walks the composed piece list once, left to right, and emits the net ops. `base_cursor` is
/// how much base content has been accounted for (in base order, non-decreasing); `final_offset`
/// is the position in the post-state (the final file).
fn read_out(base_len: u64, pieces: &[Piece]) -> Vec<Op> {
  let mut ops = Vec::new();
  let mut base_cursor = 0u64;
  let mut final_offset = 0u64;
  let mut index = 0;
  while index < pieces.len() {
    // Gather a maximal run of new bytes; its post-state offset is where it starts in the final
    // file.
    let run_src = final_offset;
    let mut run_len = 0u64;
    while let Some(Piece::New { len }) = pieces.get(index) {
      run_len = run_len.saturating_add(*len);
      index = index.saturating_add(1);
    }
    // The base bytes covered before the next surviving base piece (or the base tail at the end)
    // are gone: replaced by the new run, or deleted.
    let at_end = index >= pieces.len();
    let base_skip = match pieces.get(index) {
      Some(Piece::Base { base_at, .. }) => base_at.saturating_sub(base_cursor),
      _ => base_len.saturating_sub(base_cursor),
    };
    emit(
      &mut ops,
      base_cursor,
      base_len,
      base_skip,
      run_len,
      run_src,
      at_end,
    );
    base_cursor = base_cursor.saturating_add(base_skip);
    final_offset = final_offset.saturating_add(run_len);
    if let Some(surviving) = pieces.get(index) {
      let len = surviving.len();
      base_cursor = base_cursor.saturating_add(len);
      final_offset = final_offset.saturating_add(len);
      index = index.saturating_add(1);
    }
  }
  // Any base beyond the last surviving piece was truncated away, with nothing added after it: a
  // removal that runs to the base end, which is a truncate to `base_cursor`.
  if base_cursor < base_len {
    ops.push(op(
      OpKind::Truncate,
      base_cursor,
      base_len.saturating_sub(base_cursor),
      u64::MAX,
    ));
  }
  ops
}

/// Emits the net op(s) for one boundary: `base_skip` base bytes at `at` are gone and `run_len`
/// new bytes at post-state offset `run_src` take their place (either may be zero).
fn emit(
  ops: &mut Vec<Op>,
  at: u64,
  base_len: u64,
  base_skip: u64,
  run_len: u64,
  run_src: u64,
  at_end: bool,
) {
  match (base_skip, run_len) {
    (0, 0) => {}
    (skip, 0) => {
      // A pure removal: to the base end is a truncate (its `at` is the new length), within it a
      // delete.
      if at_end && at.saturating_add(skip) == base_len {
        ops.push(op(OpKind::Truncate, at, skip, u64::MAX));
      } else {
        ops.push(op(OpKind::Delete, at, skip, u64::MAX));
      }
    }
    (0, add) => {
      // A pure addition: past the base end is an extend, within it an insert.
      if at_end && at == base_len {
        ops.push(op(OpKind::Extend, at, add, run_src));
      } else {
        ops.push(op(OpKind::Insert, at, add, run_src));
      }
    }
    (skip, add) if skip == add => {
      // An in-place replacement of equal length is one overwrite.
      ops.push(op(OpKind::Overwrite, at, add, run_src));
    }
    (skip, add) => {
      // A replacement of unequal length is a delete then an insert (the whole-file-rewrite
      // shape when the delete spans the base).
      ops.push(op(OpKind::Delete, at, skip, u64::MAX));
      ops.push(op(OpKind::Insert, at, add, run_src));
    }
  }
}
