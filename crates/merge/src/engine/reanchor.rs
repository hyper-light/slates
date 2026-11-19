//! A review comment followed through a green's versions (§4.17.1 "Works over works", shared piece 6; §4.16 "Position
//! mapping"; A-596).
//!
//! **What it is.** [`Green::reanchor`] moves a comment's lines, held as the byte range they cover
//! ([`crate::review::line_range`]), from the version it stands on to a later one: through each change the green
//! recorded to its path in between, oldest first, by the one step of [`crate::review::move_through`]. The answer is
//! the range its lines cover at the later version, or the version whose change outdated the comment and why: its file
//! removed (the path's content history records the removal), a delta that is no edit of the old bytes (a rename onto
//! the path), a change that met its bytes, or text joined to its first or last line. It decides nothing durable: the
//! server records each answer on the request's partition (`slates_db::review`), where a comment only moves forward and
//! an outdated one never moves again.
//!
//! **What it costs.** The path's own changes in between, and nothing else: one binary search of the path's content
//! history for the first change after the comment's version, then for each change its delta on the path (the ops the
//! engine recorded when it committed the change: an index into the deltas and one map lookup), the mapping through
//! them, and two bytes of that version's text (the byte before the moved range and its last byte), read from the
//! pieces of the history value the walk already holds. It never reads a whole file, another path or the tree, and
//! never reconstructs a version: a comment on a file changed twice in a hundred thousand versions costs two steps.
//!
//! **What it refuses**, typed ([`ReanchorRefused`]): a later version past the head; a later version before the
//! comment's (a comment moves forward only); a comment's version below the green's fold floor or the checkpoint it was
//! restored at, after which the texts are no longer reconstructible (the server moves its comments forward before the
//! floor passes them, as any live reader holds it); and a range that is not whole lines of the path's text at the
//! comment's version, or a path that held no file then, which no comment was anchored to.
//!
//! **The oracle** (`reanchor/tests.rs`): generated histories of works over a green, each based at the head or behind
//! it and made of line edits (whole lines inserted, deleted or replaced, text inserted inside a line or at its start, a
//! line split or joined to the next, a newline overwritten, text appended to the end) or of file changes (an unlink, a
//! re-creation, a rename onto the file, a rename away), with comments written at versions the green still
//! reconstructs and the histories folded below generated floors. After every step every comment is followed to every
//! later version, and the answer must equal the one a diff of the whole texts the green held at each commit gives,
//! line by line: the same lines while the comment moves, outdated at the same version for the same reason, refused
//! below the floor. A census shows every answer met.

use super::Green;
use crate::history::Value;
use crate::ops_doc::Op;
use crate::range::Range;
use crate::review::{OutdatedBy, covers_whole_lines, move_through, whole_lines};

/// Where a review comment stands at a later version of a green ([`Green::reanchor`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reanchored {
  /// Its lines are unchanged at the later version, where they cover this byte range.
  Moved(Range),
  /// The change committed at `at` outdated it; it stands where it stood before that version.
  Outdated {
    /// The version whose change outdated it.
    at: u64,
    /// Why.
    by: OutdatedBy,
  },
}

/// Why a comment cannot be followed ([`Green::reanchor`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReanchorRefused {
  /// The later version is past the green's head.
  PastHead {
    /// The head.
    head: u64,
  },
  /// The later version is before the comment's: a comment moves forward only.
  Backward,
  /// The comment's version is below the green's fold floor or the checkpoint it was restored at: the texts after it
  /// are no longer reconstructible.
  Folded {
    /// The oldest version a comment is followed from.
    floor: u64,
  },
  /// The range is not whole lines of the path's text at the comment's version, or the path held no file then.
  NotLines,
}

impl Green {
  /// Moves a review comment's `range` on `path`, whole lines of the path's text at version `from`, forward to version
  /// `to` (§4.17.1 "Works over works": "A comment's range moves with the code"; A-596): through each change the green
  /// recorded to the path in `(from, to]`, oldest first ([`crate::review::move_through`]), at the cost of those
  /// changes alone. The range the lines cover at `to`, or the version whose change outdated the comment and why.
  pub fn reanchor(
    &self,
    path: &str,
    range: Range,
    (from, to): (u64, u64),
  ) -> Result<Reanchored, ReanchorRefused> {
    let head = self.head();
    if to > head {
      return Err(ReanchorRefused::PastHead { head });
    }
    if to < from {
      return Err(ReanchorRefused::Backward);
    }
    let floor = self.folded_below.max(self.deltas_from);
    if from < floor {
      return Err(ReanchorRefused::Folded { floor });
    }
    let Some(history) = self.content_history.get(path) else {
      // A path with no history has held the head's bytes since version 0 (the one history rule, A-534): nothing
      // changed it, so the lines stand where they stood.
      let lines = self
        .content
        .get(path)
        .is_some_and(|text| whole_lines(text, range));
      return if lines {
        Ok(Reanchored::Moved(range))
      } else {
        Err(ReanchorRefused::NotLines)
      };
    };
    let after = history.partition_point(|(version, _)| *version <= from);
    // The value in effect at `from` is the last entry at or before it; with none, the path held no file then (a path
    // that held one before its first change records that value first, at version 0).
    let stood = after
      .checked_sub(1)
      .and_then(|index| history.get(index))
      .and_then(|(_, value)| value.as_ref())
      .filter(|value| self.value_covers_whole_lines(value, range));
    let Some(stood) = stood else {
      return Err(ReanchorRefused::NotLines);
    };
    let mut current = range;
    let mut length = value_len(stood);
    for (version, value) in history.get(after..).unwrap_or_default() {
      if *version > to {
        break;
      }
      let outdated = |by| Ok(Reanchored::Outdated { at: *version, by });
      // A removal; or a change whose delta the green does not hold, which none at or above the floor is: never
      // followed by a guess.
      let (Some(value), Some(ops)) = (value, self.delta_on(*version, path)) else {
        return outdated(OutdatedBy::FileReplaced);
      };
      let next = value_len(value);
      match move_through(current, ops, (length, next), |moved| {
        self.value_covers_whole_lines(value, moved)
      }) {
        Ok(moved) => {
          current = moved;
          length = next;
        }
        Err(by) => return outdated(by),
      }
    }
    Ok(Reanchored::Moved(current))
  }

  /// The ops the change committed at `version` made to `path`'s content: the version's delta on the path.
  fn delta_on(&self, version: u64, path: &str) -> Option<&[Op]> {
    let index = version.checked_sub(self.deltas_from)?.checked_sub(1)?;
    self
      .deltas
      .get(usize::try_from(index).ok()?)?
      .get(path)
      .map(Vec::as_slice)
  }

  /// Whether `range` covers whole lines of a content history value's bytes ([`covers_whole_lines`]): the byte before
  /// it and its last byte, read from the value's pieces, never the whole value.
  fn value_covers_whole_lines(&self, value: &Value, range: Range) -> bool {
    let byte = |at: u64| {
      let at = usize::try_from(at).ok()?;
      value
        .read(&self.chunks, at, at.saturating_add(1))
        .first()
        .copied()
    };
    let before = range.start.checked_sub(1).and_then(byte);
    let last = range
      .start
      .checked_add(range.len)
      .and_then(|end| end.checked_sub(1))
      .and_then(byte);
    covers_whole_lines(range, value_len(value), before, last)
  }
}

/// A history value's length in bytes.
fn value_len(value: &Value) -> u64 {
  u64::try_from(value.len()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
