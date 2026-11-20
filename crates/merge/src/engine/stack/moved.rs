//! The pure core of restating an upper (A-659; `super`, its module docs, "Restating an upper"): what changed at one
//! path, and one delta moved over a work's content operations by the walk the verdict maps a work's operations with
//! ([`crate::map`]), so the moved delta and the work's landed operations come from one decision.
//!
//! **The walk.** [`crate::map::map_range`] maps a work's range through a delta's operations in order: one ending at or
//! before the range's start shifts it, one meeting it hands it to the verdict, and the first starting at or after its
//! end ends the walk. A delta's operations are sorted and so are a canonical work's, so the operations a work's
//! operation passes are a prefix of the delta that only grows from one work operation to the next. A delta operation
//! inside a work operation's prefix lies before it; outside it, after it. The moved delta shifts each of its
//! operations by the work operations before it, and the moved work each of its operations by the delta operations
//! before it, so both describe one interleaving: the one the verdict landed the work in.
//!
//! **The verdict's identical case.** A work landed with an operation meeting a delta operation only where the verdict
//! judged the two identical. A same-span overwrite is that case for any frame: the work's post-state holds the delta's
//! change already, so the moved delta leaves it out, and the work's landed operations leave the work's out, as the
//! verdict applied neither twice. Any other meeting (an identical replacement of another shape, which the verdict
//! accepted whole) is a shape no position mapping follows, and moving answers none.

use std::collections::BTreeSet;

use crate::engine::touched_range;
use crate::map::{effect_of, lies_past};
use crate::ops_doc::{Op, OpKind};
use crate::range::Range;

/// What changed at one path between a work's base and the version it is restated over (`super`, its module docs), in
/// the frame of the work's base.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Change {
  /// The content's deltas, oldest first, each in the frame the one before it left: what a range of the base is mapped
  /// through.
  pub(super) edits: Vec<Vec<Op>>,
  /// The node at the path was removed, replaced or made: unlinked, renamed away or onto, copied over, or made where
  /// nothing stood.
  pub(super) node: bool,
  /// The content changed in a shape no position mapping follows (module docs, "The verdict's identical case").
  pub(super) reshaped: bool,
  /// The mode changed.
  pub(super) mode: bool,
  /// A symbolic link's target changed.
  pub(super) symlink: bool,
  /// A hard link's node changed.
  pub(super) hardlink: bool,
  /// The IPC node changed.
  pub(super) special: bool,
  /// The attributes whose values changed.
  pub(super) xattrs: BTreeSet<String>,
}

impl Change {
  /// Whether anything at the path changed, in any dimension.
  pub(super) fn any(&self) -> bool {
    self.content() || self.metadata()
  }

  /// Whether the content's dimension changed: its bytes, or the file itself.
  pub(super) fn content(&self) -> bool {
    !self.edits.is_empty() || self.node || self.reshaped
  }

  /// Whether a dimension other than the content's changed.
  fn metadata(&self) -> bool {
    self.mode || self.symlink || self.hardlink || self.special || !self.xattrs.is_empty()
  }

  /// This change followed by `later`, the change after it: its deltas after these, its dimensions with these.
  pub(super) fn then(mut self, later: Change) -> Change {
    self.edits.extend(later.edits);
    self.node = self.node || later.node;
    self.reshaped = self.reshaped || later.reshaped;
    self.mode = self.mode || later.mode;
    self.symlink = self.symlink || later.symlink;
    self.hardlink = self.hardlink || later.hardlink;
    self.special = self.special || later.special;
    self.xattrs.extend(later.xattrs);
    self
  }

  /// This change's mode and attributes alone: what a path a work made keeps of the changes it was judged against, its
  /// node and content being the work's own.
  pub(super) fn metadata_only(self) -> Change {
    Change {
      mode: self.mode,
      xattrs: self.xattrs,
      ..Change::default()
    }
  }

  /// This change's content with `path`'s other dimensions: a renamed file's bytes come from its source, while its mode
  /// and attributes stay with its path (A-618).
  pub(super) fn with_metadata_of(self, path: Change) -> Change {
    Change {
      edits: self.edits,
      node: self.node || path.node,
      reshaped: self.reshaped,
      ..path
    }
  }
}

/// One delta moved over a work's content operations on its path, the two from one frame ([`moved_pair`]).
struct Pair {
  /// The delta in the frame the work's operations leave, less the operations the work made too.
  delta: Vec<Op>,
  /// The work's operations in the frame the delta leaves.
  work: Vec<Op>,
  /// Which of the work's operations met a same-span overwrite of the delta: the verdict's identical case.
  met: Vec<bool>,
}

/// How a delta's operation stands to a work's operation in the verdict's walk (module docs).
enum Meets {
  /// It ends at or before the work operation's start: it shifts it by its size change.
  Before(i128),
  /// It overwrites the work operation's span, as the work's overwrite does: the verdict's identical case.
  Same,
  /// It starts at or after the work operation's end, as every later one does: the walk ends.
  After,
  /// It starts at the work operation's point and lies after it, and the walk goes on, as an insert at the point may
  /// follow it ([`crate::map::lies_past`], A-668).
  Beside,
  /// It changes no content, and the walk passes it.
  Passed,
}

/// How `change`, an operation of a delta, stands to `op`, an operation of a work, both in the delta's frame; `None` when
/// they meet in any way but the verdict's identical case.
fn meets(change: &Op, op: &Op) -> Option<Meets> {
  let Some((at, old, new)) = effect_of(change) else {
    return Some(Meets::Passed);
  };
  let range = touched_range(op);
  if Range::new(at, old).overlaps(range) {
    return same_overwrite(change, op).then_some(Meets::Same);
  }
  if at.checked_add(old)? <= range.start {
    return Some(Meets::Before(size(old, new)?));
  }
  if lies_past(at, range) {
    return Some(Meets::After);
  }
  (range.len == 0 && at == range.start).then_some(Meets::Beside)
}

/// Whether two operations overwrite one span.
fn same_overwrite(a: &Op, b: &Op) -> bool {
  a.kind == OpKind::Overwrite && b.kind == OpKind::Overwrite && a.at == b.at && a.len == b.len
}

/// The size change of an operation that leaves `new` bytes where `old` stood.
fn size(old: u64, new: u64) -> Option<i128> {
  i128::from(new).checked_sub(i128::from(old))
}

/// The size change of a content operation; none for any other.
fn size_change(op: &Op) -> Option<i128> {
  effect_of(op).map_or(Some(0), |(_, old, new)| size(old, new))
}

/// `at` moved by `shift`, while it stays a position.
fn shifted(at: u64, shift: i128) -> Option<u64> {
  u64::try_from(i128::from(at).checked_add(shift)?).ok()
}

/// What one work operation's walk through a delta passed, from the delta operation it started at.
struct Walked {
  /// The first delta operation it did not pass.
  next: usize,
  /// The size change of the delta operations it passed.
  shift: i128,
  /// Whether it met a same-span overwrite.
  met: bool,
}

/// Walks `delta` from operation `from` for the work operation `op`, marking in `absorbed` the delta operations it meets
/// as the verdict's identical case; `None` when it meets one otherwise.
fn walk(delta: &[Op], op: &Op, from: usize, absorbed: &mut [bool]) -> Option<Walked> {
  let mut walked = Walked {
    next: from,
    shift: 0,
    met: false,
  };
  // Past an operation beside a point, nothing more is passed: what lies beside it lies after it.
  let mut beside = false;
  for (index, change) in delta.iter().enumerate().skip(from) {
    match meets(change, op)? {
      Meets::After => break,
      Meets::Beside => beside = true,
      Meets::Before(size) => walked.shift = walked.shift.checked_add(size)?,
      Meets::Same => {
        *absorbed.get_mut(index)? = true;
        walked.met = true;
      }
      Meets::Passed => {}
    }
    if !beside {
      walked.next = index.checked_add(1)?;
    }
  }
  Some(walked)
}

/// Moves `delta` over `work`, a work's content operations on one path, both in one frame (module docs): the delta in the
/// frame the work leaves and the work in the frame the delta leaves, or `None` when an operation of each meets the
/// other but as the verdict's identical case.
fn moved_pair(delta: &[Op], work: &[Op]) -> Option<Pair> {
  let mut absorbed = vec![false; delta.len()];
  let mut passed = Vec::with_capacity(work.len());
  let mut moved = Vec::with_capacity(work.len());
  let mut met = Vec::with_capacity(work.len());
  let (mut next, mut shift) = (0, 0i128);
  for op in work {
    let walked = walk(delta, op, next, &mut absorbed)?;
    next = walked.next;
    shift = shift.checked_add(walked.shift)?;
    passed.push(next);
    met.push(walked.met);
    moved.push(Op {
      at: shifted(op.at, shift)?,
      ..*op
    });
  }
  Some(Pair {
    delta: moved_delta(delta, (work, &passed), &absorbed)?,
    work: moved,
    met,
  })
}

/// The delta's operations moved over the work's: each by the size change of the work operations before it, those whose
/// walk ended at or before it (`passed`: how many delta operations each work operation passed), the ones the work made
/// too left out.
fn moved_delta(
  delta: &[Op],
  (work, passed): (&[Op], &[usize]),
  absorbed: &[bool],
) -> Option<Vec<Op>> {
  let mut moved = Vec::with_capacity(delta.len());
  let (mut before, mut shift) = (0, 0i128);
  for (index, change) in delta.iter().enumerate() {
    while let (Some(walked), Some(op)) = (passed.get(before), work.get(before)) {
      if *walked > index {
        break;
      }
      shift = shift.checked_add(size_change(op)?)?;
      before = before.checked_add(1)?;
    }
    if absorbed.get(index).copied().unwrap_or(false) {
      continue;
    }
    moved.push(Op {
      at: shifted(change.at, shift)?,
      ..*change
    });
  }
  Some(moved)
}

/// A path's deltas moved over a work's operations on it, one after another ([`moved_pair`]): the moved deltas, and the
/// work's operations as the verdict landed them (those it found identical left out); `None` when an operation of each
/// meets the other but as the verdict's identical case.
pub(super) fn move_edits(edits: &[Vec<Op>], work: &[Op]) -> Option<(Vec<Vec<Op>>, Vec<Op>)> {
  let mut current = work.to_vec();
  let mut met = vec![false; work.len()];
  let mut moved = Vec::with_capacity(edits.len());
  for delta in edits {
    let pair = moved_pair(delta, &current)?;
    for (seen, now) in met.iter_mut().zip(pair.met) {
      *seen = *seen || now;
    }
    moved.push(pair.delta);
    current = pair.work;
  }
  let landed = current
    .into_iter()
    .zip(met)
    .filter_map(|(op, met)| (!met).then_some(op))
    .collect();
  Some((moved, landed))
}
