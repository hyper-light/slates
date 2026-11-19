//! Review comments moved with the code (§4.17.1 "Works over works": "A comment's range moves with the code: §4.16's
//! position mapping shifts it through every delta in between, so a comment stays on its line across rebases. A range
//! that an edit replaced is marked outdated; it is never moved by a guess"; shared piece 6, A-596).
//!
//! A comment is anchored to a path, a run of whole lines and the version it was written on. Its lines are held as the
//! byte range they cover in that version's text, from the first line's first byte through the last line's newline (or
//! the file's end, for a last line with none), so the engine's position mapping ([`crate::map::map_range`]) moves it
//! exactly as it moves an increment's ranges: through each later version's delta on the path, in order, a change that
//! touches the range hands it over (here: the comment is outdated), and a change before it shifts it.
//!
//! Bytes are not lines, and the mapping alone would move a comment onto text that is no longer its lines: an insert at
//! the range's first byte (the edge rule, [`crate::range::Range::overlaps`]) may add whole lines before it or text
//! before its first line on the same line; a change to the newline before it joins its first line to the one above;
//! an append at the end of a last line with no newline lengthens that line. So after each delta the moved range must
//! still cover whole lines of that version's text, which the caller reads from the version's content
//! ([`reanchor`]'s `whole_lines`); otherwise the comment is outdated. Unchanged bytes covering whole lines are exactly
//! the same lines. A comment once outdated stays outdated: a later version that brings the same text back does not move
//! it there, since that would be a guess. Removing or replacing the file (an unlink, a rename of it or onto it, a
//! re-creation) outdates it too: following a rename is owed with the engine's path remapping.
//!
//! The oracle is the test module's: a model that re-anchors by diffing each version's whole text against the next's,
//! line by line, over generated histories of whole-line, in-line and line-joining edits; the two must agree at every
//! version.

use crate::map::{Mapped, map_range};
use crate::ops_doc::{Op, OpKind};
use crate::range::Range;

/// Where a comment stands at a later version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchored {
  /// Its lines are unchanged, now covering this byte range of the version's text.
  Moved(Range),
  /// A change met its lines (or removed or replaced its file); it is never moved again.
  Outdated,
}

/// The byte range lines `first` through `last` (counting from zero) cover in `text`, through the last one's newline (or
/// the text's end, for a last line with none); `None` when `first` is after `last` or either is past the text's lines.
pub fn line_range(text: &[u8], first: usize, last: usize) -> Option<Range> {
  if first > last {
    return None;
  }
  let mut start = None;
  let mut line = 0usize;
  let mut at = 0usize;
  while at < text.len() {
    let end = text
      .get(at..)?
      .iter()
      .position(|byte| *byte == b'\n')
      .map_or(text.len(), |newline| at.saturating_add(newline).saturating_add(1));
    if line == first {
      start = Some(at);
    }
    if line == last {
      let from = start?;
      let len = end.checked_sub(from)?;
      return Some(Range::new(u64::try_from(from).ok()?, u64::try_from(len).ok()?));
    }
    line = line.saturating_add(1);
    at = end;
  }
  None
}

/// Whether `range` covers whole lines of `text`: it is not empty, lies within the text, starts at the text's start or
/// just after a newline, and ends just after a newline or at the text's end.
pub fn whole_lines(text: &[u8], range: Range) -> bool {
  let (Ok(start), Ok(end)) = (usize::try_from(range.start), usize::try_from(range.end())) else {
    return false;
  };
  if range.len == 0 || end > text.len() {
    return false;
  }
  let starts_a_line = start == 0
    || start
      .checked_sub(1)
      .and_then(|before| text.get(before))
      .is_some_and(|byte| *byte == b'\n');
  let ends_a_line = end == text.len()
    || end
      .checked_sub(1)
      .and_then(|last| text.get(last))
      .is_some_and(|byte| *byte == b'\n');
  starts_a_line && ends_a_line
}

/// The lines (first and last, counting from zero) a whole-lines `range` covers in `text`; `None` when it covers no
/// whole lines there ([`whole_lines`]).
pub fn lines_of(text: &[u8], range: Range) -> Option<(usize, usize)> {
  if !whole_lines(text, range) {
    return None;
  }
  let start = usize::try_from(range.start).ok()?;
  let end = usize::try_from(range.end()).ok()?;
  let first = text.get(..start)?.iter().filter(|byte| **byte == b'\n').count();
  let newlines = text
    .get(start..end)?
    .iter()
    .filter(|byte| **byte == b'\n')
    .count();
  let ends_with_newline = text
    .get(end.checked_sub(1)?)
    .is_some_and(|byte| *byte == b'\n');
  let last = first
    .saturating_add(newlines)
    .saturating_sub(usize::from(ends_with_newline));
  Some((first, last))
}

/// Whether `op` removes or replaces the file a comment is on, or names it as a rename's other end: such a change takes
/// the comment's lines with it, whatever its bytes.
fn replaces_the_file(op: &Op) -> bool {
  matches!(
    op.kind,
    OpKind::Unlink
      | OpKind::Rename
      | OpKind::Create
      | OpKind::Mkdir
      | OpKind::Rmdir
      | OpKind::Link
      | OpKind::Symlink
      | OpKind::Mknod
  )
}

/// Moves a comment's `range` (whole lines of the text it was written on) forward through `deltas`, oldest first: each
/// later version's canonical ops that touch the comment's path or name it as a rename's other end, sorted by offset
/// and non-overlapping, as the engine maps an increment through them. `whole_lines(i, range)` says whether `range`
/// covers whole lines of the text after delta `i`, read from that version's content ([`whole_lines`] over it). The
/// comment is outdated at the first delta that removes or replaces its file, touches its range, or leaves it not on
/// whole lines; otherwise it moves to where the last delta left it.
pub fn reanchor(
  range: Range,
  deltas: &[&[Op]],
  whole_lines: impl Fn(usize, Range) -> bool,
) -> Anchored {
  let mut current = range;
  for (index, ops) in deltas.iter().enumerate() {
    if ops.iter().any(replaces_the_file) {
      return Anchored::Outdated;
    }
    match map_range(&[*ops], current) {
      Mapped::Overlaps => return Anchored::Outdated,
      Mapped::Shifted(next) => {
        if !whole_lines(index, next) {
          return Anchored::Outdated;
        }
        current = next;
      }
    }
  }
  Anchored::Moved(current)
}

#[cfg(test)]
// proptest's strategy types carry `Arc` (D-8's harness exception).
#[allow(clippy::disallowed_types)]
mod tests {
  use proptest::prelude::*;

  use super::*;

  /// An op of `kind` at `at` of `len` bytes on path 0, adding no content of its own.
  fn op(kind: OpKind, at: usize, len: usize) -> Op {
    Op {
      kind,
      flags: 0,
      path: 0,
      at: u64::try_from(at).unwrap(),
      len: u64::try_from(len).unwrap(),
      src: u64::MAX,
    }
  }

  /// One generated edit of a history, its positions taken modulo what the text holds.
  #[derive(Clone, Debug)]
  enum Edit {
    /// Fresh whole lines inserted before line `at` (or at the end).
    InsertLines { at: usize, count: usize },
    /// Lines `at` onward, `count` of them, deleted.
    DeleteLines { at: usize, count: usize },
    /// Lines `at` onward, `count` of them, replaced by as many fresh ones.
    ReplaceLines { at: usize, count: usize },
    /// Fresh text inserted in line `at`, `into` bytes into it (zero: at its very start, before its first byte).
    InsertInLine { at: usize, into: usize },
    /// The newline ending line `at` deleted, joining the next line to it.
    JoinNext { at: usize },
    /// Fresh text appended at the end of the text, with no newline.
    AppendTail,
  }

  /// A history's text as lines, each unique (a serial no earlier text held), the last one perhaps with no newline.
  #[derive(Clone, Debug)]
  struct Text {
    lines: Vec<Vec<u8>>,
    serial: u64,
  }

  impl Text {
    /// A text of `count` fresh lines, each ending in a newline.
    fn new(count: usize) -> Text {
      let mut text = Text {
        lines: Vec::new(),
        serial: 0,
      };
      for _ in 0..count {
        let line = text.fresh(true);
        text.lines.push(line);
      }
      text
    }

    /// Fresh unique bytes, with a newline when `newline`.
    fn fresh(&mut self, newline: bool) -> Vec<u8> {
      self.serial += 1;
      let mut bytes = format!("<{}>", self.serial).into_bytes();
      if newline {
        bytes.push(b'\n');
      }
      bytes
    }

    /// The whole text.
    fn bytes(&self) -> Vec<u8> {
      self.lines.concat()
    }

    /// The byte offset line `at` starts at (the text's length past the last line).
    fn offset(&self, at: usize) -> usize {
      self.lines.iter().take(at).map(Vec::len).sum()
    }

    /// Whether the last line has no newline.
    fn open_tail(&self) -> bool {
      self.lines.last().is_some_and(|last| last.last() != Some(&b'\n'))
    }

    /// Applies `edit` and returns the canonical delta it is on the path (sorted, non-overlapping, in the text's
    /// coordinates before it), or `None`, changing nothing, when the text cannot take it.
    fn apply(&mut self, edit: &Edit) -> Option<Vec<Op>> {
      let lines = self.lines.len();
      match *edit {
        Edit::InsertLines { at, count } => {
          let at = at % (lines + 1);
          // After a last line with no newline the first inserted line would join it.
          if at == lines && self.open_tail() {
            return None;
          }
          let made: Vec<Vec<u8>> = (0..count.max(1)).map(|_| self.fresh(true)).collect();
          let len = made.iter().map(Vec::len).sum();
          let offset = self.offset(at);
          self.lines.splice(at..at, made);
          Some(vec![op(OpKind::Insert, offset, len)])
        }
        Edit::DeleteLines { at, count } => {
          if lines == 0 {
            return None;
          }
          let at = at % lines;
          // Whole lines end in a newline but the last, so deleting whole lines never joins two.
          let count = count.max(1).min(lines - at);
          let offset = self.offset(at);
          let len = self.offset(at + count) - offset;
          self.lines.drain(at..at + count);
          Some(vec![op(OpKind::Delete, offset, len)])
        }
        Edit::ReplaceLines { at, count } => {
          if lines == 0 {
            return None;
          }
          let at = at % lines;
          let count = count.max(1).min(lines - at);
          let offset = self.offset(at);
          let old = self.offset(at + count) - offset;
          let open = self.lines[at + count - 1].last() != Some(&b'\n');
          let mut made: Vec<Vec<u8>> = (0..count).map(|_| self.fresh(true)).collect();
          if open && let Some(last) = made.last_mut() {
            last.pop();
          }
          let new: usize = made.iter().map(Vec::len).sum();
          self.lines.splice(at..at + count, made);
          // A replacement's canonical form: the shorter length overwritten in place, the rest inserted or deleted just
          // after it (an insert at an overwrite's end edge does not overlap it).
          let common = old.min(new);
          let mut ops = vec![op(OpKind::Overwrite, offset, common)];
          if new > old {
            ops.push(op(OpKind::Insert, offset + common, new - old));
          } else if old > new {
            ops.push(op(OpKind::Delete, offset + common, old - new));
          }
          Some(ops)
        }
        Edit::InsertInLine { at, into } => {
          if lines == 0 {
            return None;
          }
          let at = at % lines;
          let body = self.lines[at].len() - usize::from(self.lines[at].last() == Some(&b'\n'));
          let into = into % (body + 1);
          let made = self.fresh(false);
          let offset = self.offset(at) + into;
          let len = made.len();
          self.lines[at].splice(into..into, made);
          Some(vec![op(OpKind::Insert, offset, len)])
        }
        Edit::JoinNext { at } => {
          if lines < 2 {
            return None;
          }
          let at = at % (lines - 1);
          let offset = self.offset(at + 1) - 1;
          self.lines[at].pop();
          let next = self.lines.remove(at + 1);
          self.lines[at].extend(next);
          Some(vec![op(OpKind::Delete, offset, 1)])
        }
        Edit::AppendTail => {
          let made = self.fresh(false);
          let offset = self.offset(lines);
          let len = made.len();
          match self.lines.last_mut() {
            Some(last) if last.last() != Some(&b'\n') => last.extend(made),
            _ => self.lines.push(made),
          }
          Some(vec![op(OpKind::Extend, offset, len)])
        }
      }
    }
  }

  /// The oracle: where a comment on lines `first..=last` of `before` stands in `after`, by diffing the two whole texts
  /// line by line. Every line is unique, so the diff is unambiguous: a line is unchanged exactly when `after` holds a
  /// line of the same bytes. The comment moves when each of its lines is unchanged and they stand together, in order, in
  /// `after`; otherwise it is outdated.
  fn model_step(before: &Text, after: &Text, first: usize, last: usize) -> Option<(usize, usize)> {
    let position = |line: &Vec<u8>| after.lines.iter().position(|other| other == line);
    let start = position(before.lines.get(first)?)?;
    for (offset, line) in before.lines.get(first..=last)?.iter().enumerate() {
      if position(line)? != start + offset {
        return None;
      }
    }
    Some((start, start + (last - first)))
  }

  /// A generated history: a first text's line count, a comment's first line and extent on it, and the edits after it.
  fn history() -> impl Strategy<Value = (usize, usize, usize, Vec<Edit>)> {
    let edit = prop_oneof![
      (0..12usize, 1..3usize).prop_map(|(at, count)| Edit::InsertLines { at, count }),
      (0..12usize, 1..3usize).prop_map(|(at, count)| Edit::DeleteLines { at, count }),
      (0..12usize, 1..3usize).prop_map(|(at, count)| Edit::ReplaceLines { at, count }),
      (0..12usize, 0..8usize).prop_map(|(at, into)| Edit::InsertInLine { at, into }),
      (0..12usize).prop_map(|at| Edit::JoinNext { at }),
      Just(Edit::AppendTail),
    ];
    (
      1..10usize,
      0..10usize,
      0..4usize,
      prop::collection::vec(edit, 0..12),
    )
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// A-596's oracle (§4.17.1 "Works over works"). Do: generate a text of unique lines, a comment on a run of its
    /// lines, and a history of edits (whole lines inserted, deleted or replaced, text inserted inside a line or at its
    /// very start, a line joined to the next, text appended to the end), one canonical delta a version; move the comment
    /// through the deltas by position mapping, and re-anchor it by diffing each version's whole text against the next.
    /// Expect the two to agree at every version: the same lines while it moves, outdated at the same version, and
    /// outdated for good after.
    #[test]
    fn a_comment_moved_by_position_mapping_is_where_diffing_whole_texts_puts_it(
      (lines, first, extra, edits) in history()
    ) {
      let mut text = Text::new(lines);
      let first = first % lines;
      let last = (first + extra).min(lines - 1);
      let range = line_range(&text.bytes(), first, last).unwrap();
      let mut texts = vec![text.clone()];
      let mut deltas: Vec<Vec<Op>> = Vec::new();
      for edit in &edits {
        if let Some(ops) = text.apply(edit) {
          deltas.push(ops);
          texts.push(text.clone());
        }
      }
      let mut model = Some((first, last));
      for upto in 0..=deltas.len() {
        if upto > 0 {
          model = model.and_then(|(a, b)| model_step(&texts[upto - 1], &texts[upto], a, b));
        }
        let held: Vec<&[Op]> = deltas[..upto].iter().map(Vec::as_slice).collect();
        let mapped = reanchor(range, &held, |index, moved| {
          whole_lines(&texts[index + 1].bytes(), moved)
        });
        let mapped_lines = match mapped {
          Anchored::Moved(moved) => lines_of(&texts[upto].bytes(), moved),
          Anchored::Outdated => None,
        };
        prop_assert_eq!(mapped_lines, model, "at version {}: {:?}", upto, texts[upto]);
      }
    }
  }

  /// A-596. Do: anchor a comment on lines 1 and 2 of three, then (each alone) insert a whole line before them, insert
  /// text before line 1's first byte on line 1, and unlink the file. Expect the comment moved one line down, outdated
  /// (its first line now holds other text), and outdated (its file gone).
  #[test]
  fn a_comment_moves_on_whole_lines_and_is_outdated_by_a_change_to_its_lines_or_file() {
    let text = b"zero\none\ntwo\n".to_vec();
    let range = line_range(&text, 1, 2).unwrap();
    assert_eq!(range, Range::new(5, 8));
    let after = b"new\nzero\none\ntwo\n".to_vec();
    let inserted = [op(OpKind::Insert, 5, 4)];
    assert_eq!(
      reanchor(range, &[&inserted], |_, moved| whole_lines(&after, moved)),
      Anchored::Moved(Range::new(9, 8))
    );
    assert_eq!(lines_of(&after, Range::new(9, 8)), Some((2, 3)));
    let prefixed = b"zero\nxxone\ntwo\n".to_vec();
    let prefix = [op(OpKind::Insert, 5, 2)];
    assert_eq!(
      reanchor(range, &[&prefix], |_, moved| whole_lines(&prefixed, moved)),
      Anchored::Outdated
    );
    let unlinked = [op(OpKind::Unlink, 0, 0)];
    assert_eq!(
      reanchor(range, &[&unlinked], |_, _| true),
      Anchored::Outdated
    );
  }
}
