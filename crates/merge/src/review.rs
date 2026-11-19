//! Review comments moved with the code (§4.17.1 "Works over works", shared piece 6; D-28; A-596): the pure rule that
//! moves a comment's lines through one later version's change, and the line arithmetic a server anchors one by.
//!
//! **A comment's anchor.** A comment is anchored to a path, a run of whole lines and the version it was written on
//! (§4.17.1: "comments anchored to the work, one of its versions, a path and a byte range"). Its lines are held as the
//! byte range they cover in that version's text, from the first line's first byte through the last line's newline, or
//! the text's end for a last line with none ([`line_range`]), so the engine's position mapping ([`crate::map`]) moves
//! it exactly as it moves an increment's ranges (§4.16 "Position mapping"), and a reader is answered its lines again
//! from the version it stands on ([`lines_of`]).
//!
//! **One step** ([`move_through`]): through one later version's canonical delta on the path, a change that meets the
//! range outdates the comment, a change before it shifts it and a change after it leaves it. Bytes are not lines, so
//! three facts are judged besides, each read from the delta or from the version's text, never guessed:
//! - the delta must account for the file's change of length. Every delta the engine records for an edit does (an
//!   accepted op set fits its base: an extend starts at the base's end and a hole is zeros in the post-state,
//!   `fits_base` in `crate::engine`), and one that does not edited nothing of the old bytes: the file was replaced
//!   (a rename onto the path records the new file as one insert at its start). The comment is outdated
//!   ([`OutdatedBy::FileReplaced`]);
//! - the edge rule lets an insert at the range's first byte shift it and one at its end leave it
//!   ([`crate::range::Range::overlaps`]), and a change to the newline just before the range does not meet it. So the
//!   moved range must still cover whole lines of the new text ([`covers_whole_lines`]); when it does not, text was
//!   joined to its first or last line (an insert before the first line's first byte, the newline before it deleted or
//!   overwritten, an append to a last line with no newline), and the comment is outdated
//!   ([`OutdatedBy::LinesJoined`]). Unchanged bytes that cover whole lines are exactly the same lines: the newlines
//!   inside them are unchanged, and nothing else stands on their first or last line;
//! - a change that meets the range itself changed the lines ([`OutdatedBy::LinesChanged`]).
//!
//! A file removed (an unlink, a rename away, a re-creation) outdates the comment too, judged by the walk that feeds
//! these steps. Following a renamed file is owed with the engine's path remapping: the verdict answers a work's edit to
//! a renamed-away file `DeleteModify` today, and a comment is no braver than the verdict. A comment once outdated stays
//! outdated where it last stood: a later version that brings the same text back does not move it there, since that
//! would be a guess.
//!
//! The walk through a green's versions, and the oracle that holds it to a diff of whole texts over generated histories,
//! are [`crate::engine::Green::reanchor`]'s.

use crate::map::{Mapped, effect_of, map_range};
use crate::ops_doc::Op;
use crate::range::Range;

/// Why a comment was outdated at a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutdatedBy {
  /// Its file was removed, or replaced by bytes that are not an edit of the old ones (a rename onto it, a file made
  /// again after a removal).
  FileReplaced,
  /// A change met its lines' bytes: an edit inside them, over them or of a newline among them.
  LinesChanged,
  /// Its bytes are unchanged but are no longer whole lines: text was joined to its first or last line.
  LinesJoined,
}

/// The byte range lines `first` through `last` (counting from zero) cover in `text`, through the last one's newline,
/// or the text's end for a last line with none; `None` when `first` is after `last` or either is past the text's
/// lines. A text's lines are its runs ending in a newline, and the run after its last newline when that is not empty.
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
      .map_or(text.len(), |newline| {
        at.saturating_add(newline).saturating_add(1)
      });
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

/// Whether `range` covers whole lines of a text `len` bytes long, judged from the byte just before it (`None` at the
/// text's start, or where there is none) and its own last byte: it is not empty, lies within the text, starts the text
/// or follows a newline, and ends the text or with a newline. Two bytes and a length decide it, so a caller reads no
/// more of a version's text than those.
pub fn covers_whole_lines(range: Range, len: u64, before: Option<u8>, last: Option<u8>) -> bool {
  let Some(end) = range.start.checked_add(range.len) else {
    return false;
  };
  let starts_a_line = range.start == 0 || before == Some(b'\n');
  let ends_a_line = end == len || last == Some(b'\n');
  range.len > 0 && end <= len && starts_a_line && ends_a_line
}

/// The byte at `at` of `text`, if it has one.
fn byte_at(text: &[u8], at: u64) -> Option<u8> {
  text.get(usize::try_from(at).ok()?).copied()
}

/// Whether `range` covers whole lines of `text` ([`covers_whole_lines`] over the whole text).
pub fn whole_lines(text: &[u8], range: Range) -> bool {
  let len = u64::try_from(text.len()).unwrap_or(u64::MAX);
  let before = range
    .start
    .checked_sub(1)
    .and_then(|at| byte_at(text, at));
  let last = range
    .start
    .checked_add(range.len)
    .and_then(|end| end.checked_sub(1))
    .and_then(|at| byte_at(text, at));
  covers_whole_lines(range, len, before, last)
}

/// The lines, first and last counting from zero, that a whole-lines `range` covers in `text`; `None` when it covers no
/// whole lines there ([`whole_lines`]). What a reader of a comment is answered, from the version it stands on.
pub fn lines_of(text: &[u8], range: Range) -> Option<(usize, usize)> {
  if !whole_lines(text, range) {
    return None;
  }
  let start = usize::try_from(range.start).ok()?;
  let end = usize::try_from(range.end()).ok()?;
  let newlines = |bytes: &[u8]| bytes.iter().filter(|byte| **byte == b'\n').count();
  let first = newlines(text.get(..start)?);
  let inside = newlines(text.get(start..end)?);
  let ends_with_newline = text
    .get(end.checked_sub(1)?)
    .is_some_and(|byte| *byte == b'\n');
  let last = first
    .saturating_add(inside)
    .saturating_sub(usize::from(ends_with_newline));
  Some((first, last))
}

/// The change of length `ops` make to a file: the bytes each adds less the bytes each removes ([`effect_of`]).
fn net_change(ops: &[Op]) -> i128 {
  ops
    .iter()
    .filter_map(effect_of)
    .fold(0i128, |net, (_, removed, added)| {
      net
        .saturating_add(i128::from(added))
        .saturating_sub(i128::from(removed))
    })
}

/// Moves a comment's `range` (whole lines of its file's text before the step) through one later version's canonical
/// ops on its path: sorted by offset and non-overlapping, in the coordinates of the version before, as the engine maps
/// an increment's ranges through them. `lengths` is the file's length before the step and after it; `whole_lines` says
/// whether a range covers whole lines of the text after it ([`covers_whole_lines`] over that version's bytes). The
/// range in the text after the step, or why the comment is outdated there.
pub fn move_through(
  range: Range,
  ops: &[Op],
  (before, after): (u64, u64),
  whole_lines: impl FnOnce(Range) -> bool,
) -> Result<Range, OutdatedBy> {
  if i128::from(before).saturating_add(net_change(ops)) != i128::from(after) {
    return Err(OutdatedBy::FileReplaced);
  }
  match map_range(&[ops], range) {
    Mapped::Overlaps => Err(OutdatedBy::LinesChanged),
    Mapped::Shifted(moved) if whole_lines(moved) => Ok(moved),
    Mapped::Shifted(_) => Err(OutdatedBy::LinesJoined),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ops_doc::OpKind;

  /// An op of `kind` at `at` of `len` bytes on path 0, adding no content of its own.
  fn op(kind: OpKind, at: u64, len: u64) -> Op {
    Op {
      kind,
      flags: 0,
      path: 0,
      at,
      len,
      src: u64::MAX,
    }
  }

  /// `text` with its `removed` bytes at `at` replaced by `inserted`: the text after a step, which its whole-lines
  /// judgement reads.
  fn moved_text(text: &[u8], at: usize, removed: usize, inserted: &[u8]) -> Vec<u8> {
    let mut out = text[..at].to_vec();
    out.extend_from_slice(inserted);
    out.extend_from_slice(&text[at + removed..]);
    out
  }

  /// A-596. Do: name lines of a text with a final newline, of one without, and of one with an empty line, then read
  /// each range's lines back. Expect each run of lines covered from its first byte through its last newline (or the
  /// text's end), the same lines read back, and lines past the text, a reversed run, a range starting inside a line,
  /// one ending inside a line, an empty one and one past the text's end each refused.
  #[test]
  fn a_run_of_lines_is_the_bytes_from_its_first_byte_through_its_last_newline() {
    let text = b"zero\none\ntwo\n";
    assert_eq!(line_range(text, 0, 0), Some(Range::new(0, 5)));
    assert_eq!(line_range(text, 1, 2), Some(Range::new(5, 8)));
    assert_eq!(line_range(text, 2, 3), None);
    assert_eq!(line_range(text, 2, 1), None);
    assert_eq!(lines_of(text, Range::new(5, 8)), Some((1, 2)));
    let open = b"zero\none";
    assert_eq!(line_range(open, 1, 1), Some(Range::new(5, 3)));
    assert_eq!(lines_of(open, Range::new(5, 3)), Some((1, 1)));
    assert_eq!(lines_of(open, Range::new(0, 8)), Some((0, 1)));
    let blank = b"a\n\nb\n";
    assert_eq!(line_range(blank, 1, 1), Some(Range::new(2, 1)));
    assert_eq!(lines_of(blank, Range::new(2, 3)), Some((1, 2)));
    for (range, why) in [
      (Range::new(1, 4), "starts inside a line"),
      (Range::new(0, 3), "ends inside a line"),
      (Range::new(5, 0), "is empty"),
      (Range::new(5, 9), "runs past the text"),
      (Range::new(u64::MAX, 2), "has no end"),
    ] {
      assert!(!whole_lines(text, range), "a range that {why}");
      assert_eq!(lines_of(text, range), None, "a range that {why}");
    }
  }

  /// A-596. Do: move a comment on lines 1 and 2 of three through one step each: whole lines inserted above them, an
  /// insert at their first byte that ends in a newline, one that does not, the newline before them deleted, an edit
  /// inside them, lines appended after them, and the file replaced by an insert at its start that does not account for
  /// its length. Expect: moved down by the inserted lines, moved by the insert that ends a line, outdated as joined by
  /// the other insert and by the deleted newline, outdated as changed by the edit inside, unmoved by the lines after,
  /// and outdated as replaced by the insert that is no edit of the old bytes.
  #[test]
  fn one_step_moves_whole_lines_and_outdates_lines_a_change_met_joined_or_replaced() {
    let text = b"zero\none\ntwo\n".to_vec();
    let range = line_range(&text, 1, 2).unwrap();
    let step = |ops: &[Op], after: &[u8]| {
      move_through(
        range,
        ops,
        (text.len() as u64, after.len() as u64),
        |moved| whole_lines(after, moved),
      )
    };
    let above = moved_text(&text, 0, 0, b"new\n");
    assert_eq!(step(&[op(OpKind::Insert, 0, 4)], &above), Ok(Range::new(9, 8)));
    assert_eq!(lines_of(&above, Range::new(9, 8)), Some((2, 3)));
    let ending_a_line = moved_text(&text, 5, 0, b"new\n");
    assert_eq!(
      step(&[op(OpKind::Insert, 5, 4)], &ending_a_line),
      Ok(Range::new(9, 8))
    );
    let prefixed = moved_text(&text, 5, 0, b"xx");
    assert_eq!(
      step(&[op(OpKind::Insert, 5, 2)], &prefixed),
      Err(OutdatedBy::LinesJoined)
    );
    let joined = moved_text(&text, 4, 1, b"");
    assert_eq!(
      step(&[op(OpKind::Delete, 4, 1)], &joined),
      Err(OutdatedBy::LinesJoined)
    );
    let inside = moved_text(&text, 6, 0, b"x");
    assert_eq!(
      step(&[op(OpKind::Insert, 6, 1)], &inside),
      Err(OutdatedBy::LinesChanged)
    );
    let after = moved_text(&text, 13, 0, b"three\n");
    assert_eq!(step(&[op(OpKind::Extend, 13, 6)], &after), Ok(range));
    let replaced = b"another file\n".to_vec();
    assert_eq!(
      step(&[op(OpKind::Insert, 0, 13)], &replaced),
      Err(OutdatedBy::FileReplaced)
    );
  }

  /// A-596. Do: a comment on a last line with no newline, then text appended to that line, and then a newline appended
  /// alone. Expect both outdated as joined: the line holds other text after the comment's bytes either way.
  #[test]
  fn an_append_to_an_open_last_line_outdates_a_comment_on_it() {
    let text = b"zero\none".to_vec();
    let range = line_range(&text, 1, 1).unwrap();
    for added in [&b"more"[..], &b"\n"[..]] {
      let after = moved_text(&text, 8, 0, added);
      assert_eq!(
        move_through(
          range,
          &[op(OpKind::Extend, 8, added.len() as u64)],
          (8, after.len() as u64),
          |moved| whole_lines(&after, moved),
        ),
        Err(OutdatedBy::LinesJoined),
        "{added:?}"
      );
    }
  }
}
