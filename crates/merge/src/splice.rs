//! The splice (§4.16 "Splice"; D-27). Once the verdict accepts a path's net ops, the merge task
//! builds the new version's extent list for that path from the base version's extents, replacing
//! each op's range with a reference into the increment's post-state chunks. No byte is copied: an
//! unchanged run keeps pointing at the base chunk it always did, and an added run points at the
//! post-state chunk that holds it (`src`). The result is one extent list for the new version,
//! computed in `O(base extents + ops)`.
//!
//! The net ops are in base coordinates and non-decreasing by offset (the deriver's shape), so the
//! splice is a single walk: copy the base extents up to the next op, then place the op's
//! replacement. This module is pure: no I/O, no clock, no randomness; it moves references, never
//! bytes.
//!
//! Sources here are `Base` and `PostState` — the two chunk stores an extent can point into. In
//! the running system each is a chunk id and a sub-chunk offset (§4.5); the pure core needs only
//! which store and the offset within it, which is also what the reconstruction oracle checks.

use crate::ops_doc::{Op, OpKind};

/// Which chunk store an extent's bytes come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
  /// The base version's chunks (an unchanged run — the reference is reused, nothing is copied).
  Base,
  /// The increment's post-state chunks (bytes an accepted op added).
  PostState,
}

/// A contiguous run of the file's content, pointing at `len` bytes starting at `at` within
/// `source`'s store. A file's content is the concatenation of its extents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
  /// Which store the bytes are in.
  pub source: Source,
  /// The offset within that store.
  pub at: u64,
  /// The run length.
  pub len: u64,
}

/// The total logical length an extent list covers.
fn total_len(extents: &[Extent]) -> u64 {
  extents.iter().map(|extent| extent.len).sum()
}

/// Appends to `out` the base extents covering the logical range `[from, to)`, splitting the
/// extents that straddle the boundaries so no byte outside the range is included. Preserves each
/// run's source and store offset (the reference is carried, not the bytes).
fn copy_base(base: &[Extent], from: u64, to: u64, out: &mut Vec<Extent>) {
  if from >= to {
    return;
  }
  let mut logical = 0u64;
  for extent in base {
    let start = logical;
    let end = logical + extent.len;
    logical = end;
    if end <= from || start >= to {
      continue;
    }
    let slice_start = from.max(start);
    let slice_end = to.min(end);
    out.push(Extent {
      source: extent.source,
      at: extent.at + (slice_start - start),
      len: slice_end - slice_start,
    });
  }
}

/// Splices a path's accepted content net ops into its base extent list, producing the new
/// version's extent list. Namespace ops (create, unlink, rename) carry no content and are ignored
/// here — they act on the version tree, not on a file's extents.
pub fn splice(base: &[Extent], ops: &[Op]) -> Vec<Extent> {
  let base_len = total_len(base);
  let mut out = Vec::new();
  let mut base_pos = 0u64;
  for op in ops {
    let adds = matches!(op.kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend);
    let removes = matches!(op.kind, OpKind::Delete | OpKind::Truncate);
    if !adds && !removes {
      continue;
    }
    if op.at > base_pos {
      copy_base(base, base_pos, op.at, &mut out);
      base_pos = op.at;
    }
    match op.kind {
      OpKind::Delete => base_pos = op.at.saturating_add(op.len),
      OpKind::Truncate => base_pos = base_len,
      OpKind::Overwrite => {
        out.push(Extent {
          source: Source::PostState,
          at: op.src,
          len: op.len,
        });
        base_pos = op.at.saturating_add(op.len);
      }
      OpKind::Insert | OpKind::Extend => {
        out.push(Extent {
          source: Source::PostState,
          at: op.src,
          len: op.len,
        });
      }
      _ => {}
    }
  }
  if base_pos < base_len {
    copy_base(base, base_pos, base_len, &mut out);
  }
  out
}
