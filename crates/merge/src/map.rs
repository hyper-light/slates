//! Position mapping (§4.16 "Position mapping"; D-27's canonical rebase). An increment is
//! declared against a base version; by the time it is merged, the green volume's head may be
//! several versions ahead. Before the verdict and the splice, the increment's ranges are mapped
//! from the base version forward through the canonical deltas of every version in `(base, head]`,
//! one direction, per path.
//!
//! The rule for one delta on one path: an intervening operation that touches base bytes at
//! `[at, at + old_len)` and leaves `new_len` bytes there shifts every later position by
//! `new_len - old_len`; a range that lies entirely before it is unaffected (beyond the shift a
//! still-earlier operation applied); a range that overlaps a touched span is handed to the
//! verdict as a possible conflict (this module does not classify it — that is the verdict's
//! job). An insert at the very edge of a range does not overlap it (the edge rule of
//! [`crate::range::Range::overlaps`]), so a range whose neighbourhood only grew or shrank around
//! it maps cleanly. Maps compose: mapping through `(base, head]` is mapping through each delta in
//! turn, feeding the shifted range into the next.
//!
//! This module is pure: no I/O, no clock, no randomness. It maps through the raw deltas in
//! order; folding old deltas into checkpoint deltas so a distant base maps in `O(log)` lookups
//! is the same composition applied ahead of time (the design's checkpoints) and is the measured
//! optimization when a base-lag benchmark shows the raw walk dominating (owed, not guessed).

use crate::ops_doc::{Op, OpKind};
use crate::range::Range;

/// The result of mapping one range forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapped {
  /// The range maps cleanly to this shifted range in head coordinates (no intervening change
  /// touched it); the verdict accepts it and the splice places it here.
  Shifted(Range),
  /// The range overlaps an intervening change; the verdict decides whether that is an
  /// accept-identical or a byte-exact conflict.
  Overlaps,
}

/// The content effect of one op on this path, as `(at, old_len, new_len)` in the op's version
/// coordinates: `old_len` base bytes at `at` become `new_len` bytes. A namespace op (create,
/// rename, mode, xattr) has no byte-coordinate effect and returns `None`.
fn effect_of(op: &Op) -> Option<(u64, u64, u64)> {
  match op.kind {
    OpKind::Overwrite => Some((op.at, op.len, op.len)),
    OpKind::Insert | OpKind::Extend => Some((op.at, 0, op.len)),
    OpKind::Delete | OpKind::Truncate => Some((op.at, op.len, 0)),
    OpKind::Create
    | OpKind::Unlink
    | OpKind::Mkdir
    | OpKind::Rmdir
    | OpKind::Rename
    | OpKind::Link
    | OpKind::Symlink
    | OpKind::SetMode
    | OpKind::SetXattr
    | OpKind::RemoveXattr => None,
  }
}

/// Maps `range` forward through one delta's ops on this path. The ops are sorted by `at` and
/// non-overlapping (a canonical delta's shape), so a single pass accumulates the shift from
/// effects before the range and stops at the first effect past it.
fn map_through_one(ops: &[Op], range: Range) -> Mapped {
  // The accumulated size change from effects that lie entirely before the range; `i128` so a
  // large delete before a large range cannot underflow during accumulation.
  let mut shift: i128 = 0;
  for op in ops {
    let Some((at, old_len, new_len)) = effect_of(op) else {
      continue;
    };
    let touched = Range::new(at, old_len);
    if touched.overlaps(range) {
      return Mapped::Overlaps;
    }
    if at.saturating_add(old_len) <= range.start {
      // Entirely before the range (or ending exactly at its start): it shifts the range.
      shift += i128::from(new_len) - i128::from(old_len);
    } else if at >= range.end() {
      // Entirely after the range; every later op is further still (sorted), so stop.
      break;
    }
    // Otherwise it touches a boundary without overlapping (the edge rule): no shift, no stop.
  }
  let shifted = i128::from(range.start) + shift;
  let start = u64::try_from(shifted.max(0)).unwrap_or(0);
  Mapped::Shifted(Range::new(start, range.len))
}

/// Maps `range` from the increment's base version forward through the canonical deltas in
/// `(base, head]`, in order (oldest first). Each delta is that version's ops on this path.
/// Returns the head-coordinate range when nothing touched it, or [`Mapped::Overlaps`] as soon as
/// any delta's change meets it.
pub fn map_range(deltas: &[&[Op]], range: Range) -> Mapped {
  let mut current = range;
  for ops in deltas {
    match map_through_one(ops, current) {
      Mapped::Shifted(next) => current = next,
      Mapped::Overlaps => return Mapped::Overlaps,
    }
  }
  Mapped::Shifted(current)
}
