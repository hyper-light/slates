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
//! list is a `Vec`, so a split near the front is O(pieces); the journal a `submit` composes is
//! bounded by the work volume's mutations, and if a length benchmark shows the piece count
//! dominating, a gap-buffer or balanced tree is the measured replacement (owed, not guessed).

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
          base_at: base_at + offset,
          len: len - offset,
        },
      ),
      Piece::New { len } => (Piece::New { len: offset }, Piece::New { len: len - offset }),
    }
  }
}

/// The total current length of a piece list.
fn total_len(pieces: &[Piece]) -> u64 {
  pieces.iter().map(|p| p.len()).sum()
}

/// Ensures a piece boundary exists at `offset` bytes from the start, splitting the piece that
/// straddles it, and returns the index of the piece that begins at `offset` (or the length when
/// `offset` is the end).
fn split_at(pieces: &mut Vec<Piece>, offset: u64) -> usize {
  let mut acc = 0u64;
  let mut index = 0;
  while index < pieces.len() {
    if acc == offset {
      return index;
    }
    let piece_len = pieces[index].len();
    if acc + piece_len > offset {
      let (left, right) = pieces[index].split(offset - acc);
      pieces[index] = left;
      pieces.insert(index + 1, right);
      return index + 1;
    }
    acc += piece_len;
    index += 1;
  }
  pieces.len()
}

/// Applies one declared operation to the piece list, in current-file coordinates.
fn apply(pieces: &mut Vec<Piece>, op: &ContentOp) {
  match *op {
    ContentOp::Overwrite { at, len } => {
      if len == 0 {
        return;
      }
      let start = split_at(pieces, at);
      let end = split_at(pieces, at + len);
      pieces.drain(start..end);
      pieces.insert(start, Piece::New { len });
    }
    ContentOp::Extend { at, len } | ContentOp::Insert { at, len } => {
      if len == 0 {
        return;
      }
      let start = split_at(pieces, at);
      pieces.insert(start, Piece::New { len });
    }
    ContentOp::Delete { at, len } => {
      if len == 0 {
        return;
      }
      let start = split_at(pieces, at);
      let end = split_at(pieces, at + len);
      pieces.drain(start..end);
    }
    ContentOp::Truncate { len } => {
      let total = total_len(pieces);
      if len < total {
        let cut = split_at(pieces, len);
        pieces.truncate(cut);
      } else if len > total {
        pieces.push(Piece::New { len: len - total });
      }
    }
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
  let mut pieces = Vec::new();
  if base_len > 0 {
    pieces.push(Piece::Base {
      base_at: 0,
      len: base_len,
    });
  }
  for declared in journal {
    apply(&mut pieces, declared);
  }
  let final_len = total_len(&pieces);
  (read_out(base_len, &pieces), final_len)
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
      run_len += *len;
      index += 1;
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
    base_cursor += base_skip;
    final_offset += run_len;
    if let Some(surviving) = pieces.get(index) {
      let len = surviving.len();
      base_cursor += len;
      final_offset += len;
      index += 1;
    }
  }
  // Any base beyond the last surviving piece was truncated away, with nothing added after it: a
  // removal that runs to the base end, which is a truncate to `base_cursor`.
  if base_cursor < base_len {
    ops.push(op(
      OpKind::Truncate,
      base_cursor,
      base_len - base_cursor,
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
      if at_end && at + skip == base_len {
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
