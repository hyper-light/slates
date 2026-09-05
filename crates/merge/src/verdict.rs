//! The deterministic verdict (§4.16 "The verdict, two pure passes", D-27). Pass one is a sweep
//! line over one path's increment ranges (`mine`) and the intervening deltas' effect ranges
//! (`theirs`, everything accepted since the increment's base touched this path): disjoint ranges
//! accept, an identical span is a candidate for pass two's byte check, and any other overlap is
//! a conflict with its class. The sweep is a linear merge of two sorted lists, so it allocates
//! nothing and reads no clock, draws no randomness, and does no I/O.
//!
//! The classes mirror the design's `MergeConflictClass`. The engine layer supplies the class
//! for the structural conflicts a range sweep cannot see on its own (a rename against a rename,
//! a create against a create, a delete against a modify, a type change, a metadata clash) by
//! calling [`path_verdict`] with the structural signal; the sweep decides the content classes
//! (overlap, containment, same-position differing inserts).

use crate::range::{Range, RangeSet};

/// Why two operations on one path cannot both be accepted (§4.16 `MergeConflictClass`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeConflictClass {
  /// Two ranges overlap or one contains the other.
  Overlap,
  /// An edit anchored inside a range an intervening delta removed.
  AnchoredInDelete,
  /// Two inserts at the same position with differing bytes.
  SamePositionDiffering,
  /// A rename against a rename of the same path.
  RenameRename,
  /// A create against a create with differing content.
  CreateCreate,
  /// A delete on one side, a modify on the other.
  DeleteModify,
  /// A modify on one side, a delete on the other.
  ModifyDelete,
  /// A file became a directory or the reverse.
  TypeChanged,
  /// Differing mode or xattr changes on one path.
  MetaMeta,
}

/// The verdict for one operation or one path (§4.16). `AcceptIdentical` is decided by pass two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
  /// The operation applies; nothing intervening touched its range.
  Accept,
  /// The intervening change and this one produced the same bytes (decided by pass two).
  AcceptIdentical,
  /// The operation cannot apply; the class says why.
  Conflict(MergeConflictClass),
}

/// A path's pass-one outcome: accepted outright, a set of same-span candidates pass two must
/// byte-check, or a conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathVerdict {
  /// Every range is disjoint from the intervening effects; accept without reading bytes.
  Accept,
  /// These spans coincide with an intervening effect; pass two compares their bytes.
  Candidates(Vec<Range>),
  /// A conflict; the class says why.
  Conflict(MergeConflictClass),
}

/// Pass one for one path: sweep `mine` (the increment's ranges) against `theirs` (the
/// intervening effect ranges), both sorted. Disjoint ranges accept; a range that exactly
/// coincides with an intervening one is a candidate for pass two; any other overlap is a
/// conflict. `mine_deletes_all`/`theirs_deletes_all` say a side removed the whole path
/// (delete-vs-modify); a structural signal, when present, is returned directly (rename/create/
/// type/meta conflicts a range sweep cannot see).
pub fn path_verdict(
  mine: &RangeSet,
  theirs: &RangeSet,
  structural: Option<MergeConflictClass>,
) -> PathVerdict {
  if let Some(class) = structural {
    return PathVerdict::Conflict(class);
  }
  // The fast, common case: nothing intervening touched this path, so every range accepts.
  if theirs.is_empty() {
    return PathVerdict::Accept;
  }
  // Likewise a path this increment only reads or that composed to nothing.
  if mine.is_empty() {
    return PathVerdict::Accept;
  }
  sweep(mine.ranges(), theirs.ranges())
}

/// The sweep line: a linear merge of two sorted, within-side non-overlapping range lists. It
/// allocates only the candidate list it must return (the hot comparison itself allocates
/// nothing); a conflict short-circuits.
fn sweep(mine: &[Range], theirs: &[Range]) -> PathVerdict {
  let mut candidates: Vec<Range> = Vec::new();
  let (mut i, mut j) = (0usize, 0usize);
  while i < mine.len() && j < theirs.len() {
    let a = mine[i];
    let b = theirs[j];
    if a.overlaps(b) {
      if a.same_span(b) {
        // Same span, possibly the same edit made twice: pass two decides by bytes.
        candidates.push(a);
        i += 1;
        j += 1;
        continue;
      }
      // A zero-length anchor (an insert) that overlaps a non-empty intervening range is
      // anchored inside a change; two inserts at one point differ in position handling.
      let class = if a.len == 0 || b.len == 0 {
        MergeConflictClass::SamePositionDiffering
      } else {
        MergeConflictClass::Overlap
      };
      return PathVerdict::Conflict(class);
    }
    // Disjoint: advance the one that ends first.
    if a.end() <= b.start {
      i += 1;
    } else {
      j += 1;
    }
  }
  if candidates.is_empty() {
    PathVerdict::Accept
  } else {
    PathVerdict::Candidates(candidates)
  }
}

/// Pass two for one same-span candidate: the increment's bytes for the span and the current
/// version's bytes. Equal bytes are `AcceptIdentical` (the same edit, or a convergent one);
/// unequal are a conflict. This is the only pass that looks at bytes, and it is a memcmp.
pub fn compare_bytes(mine: &[u8], theirs: &[u8]) -> Verdict {
  if mine == theirs {
    Verdict::AcceptIdentical
  } else {
    Verdict::Conflict(MergeConflictClass::Overlap)
  }
}

/// The whole-increment fast path (§4.16): when every path the increment touches was last
/// changed at or before the increment's `base` version, nothing intervening can conflict, so the
/// verdict is `Accept` with no range work. `last_changed` gives each touched path's last
/// version; `base` is the increment's base. Returns true when the fast path applies.
pub fn fast_path(last_changed: &[u64], base: u64) -> bool {
  last_changed.iter().all(|&v| v <= base)
}
