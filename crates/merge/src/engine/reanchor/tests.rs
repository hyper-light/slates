//! The oracle of [`Green::reanchor`] and its refusals (A-596; §4.17.1 "Works over works"), as the module's doc states.

// proptest's strategy types carry `Arc` (D-8's harness exception).
#![allow(clippy::disallowed_types)]

use std::cell::RefCell;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::{ReanchorRefused, Reanchored};
use crate::engine::{Green, Increment, Outcome};
use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::origin::Origin;
use crate::range::Range;
use crate::review::{OutdatedBy, line_range, lines_of};

/// Shape: the file the comments are written on.
const FILE: &str = "a.txt";
/// Shape: the file renamed onto it.
const OTHER: &str = "b.txt";
/// Shape: where it is renamed away to.
const AWAY: &str = "c.txt";
/// Shape: the most comments a history holds at once (the oldest leaves first), so each step's checks stay small.
const COMMENTS: usize = 4;

/// An increment under construction, laid out as the deriver lays one out.
#[derive(Default)]
struct Work {
  doc: OpsDoc,
  post: Vec<u8>,
}

impl Work {
  fn op(&mut self, kind: OpKind, path: &str, at: u64, len: u64, src: u64) {
    let path = self.doc.paths.intern(path);
    self.doc.ops.push(Op {
      kind,
      flags: 0,
      path,
      at,
      len,
      src,
    });
  }

  fn stash(&mut self, bytes: &[u8]) -> u64 {
    let at = self.post.len() as u64;
    self.post.extend_from_slice(bytes);
    at
  }

  fn name(&mut self, name: &str) -> u64 {
    u64::from(self.doc.paths.intern(name))
  }

  /// Lays `splices` (sorted by offset, disjoint, against a base of `len` bytes) into ops on `path` in the deriver's
  /// canonical forms (`crate::derive`): an equal-length replacement one overwrite, an unequal one a delete then an
  /// insert, an addition at the end an extend and within an insert, a removal to the end a truncate and within a
  /// delete.
  fn splices(&mut self, path: &str, len: usize, splices: &[Splice]) {
    for splice in splices {
      let at = splice.at as u64;
      let removed = splice.removed as u64;
      let added = splice.inserted.len() as u64;
      let to_end = splice.at + splice.removed == len;
      match (removed, added) {
        (0, 0) => {}
        (0, _) => {
          let src = self.stash(&splice.inserted);
          let kind = if to_end {
            OpKind::Extend
          } else {
            OpKind::Insert
          };
          self.op(kind, path, at, added, src);
        }
        (_, 0) => {
          let kind = if to_end {
            OpKind::Truncate
          } else {
            OpKind::Delete
          };
          self.op(kind, path, at, removed, u64::MAX);
        }
        _ if removed == added => {
          let src = self.stash(&splice.inserted);
          self.op(OpKind::Overwrite, path, at, added, src);
        }
        _ => {
          self.op(OpKind::Delete, path, at, removed, u64::MAX);
          let src = self.stash(&splice.inserted);
          self.op(OpKind::Insert, path, at, added, src);
        }
      }
    }
  }
}

/// One edit's change to a text: its `removed` bytes at `at` replaced by `inserted`.
#[derive(Debug)]
struct Splice {
  at: usize,
  removed: usize,
  inserted: Vec<u8>,
}

/// One edit of a text's lines, its line and offset taken modulo what the text holds.
#[derive(Clone, Copy, Debug)]
enum LineEdit {
  /// `count` fresh whole lines inserted before line `at`, or at the end.
  InsertLines { at: u8, count: u8 },
  /// `count` lines from line `at` deleted.
  DeleteLines { at: u8, count: u8 },
  /// `count` lines from line `at` replaced by `with` fresh ones.
  ReplaceLines { at: u8, count: u8, with: u8 },
  /// A fresh token inserted `into` bytes into line `at` (zero: before its first byte).
  InsertInLine { at: u8, into: u8 },
  /// Line `at` split `into` bytes in, by a fresh token, a newline and a fresh token.
  SplitLine { at: u8, into: u8 },
  /// The newline ending line `at` deleted, joining the next line to it.
  JoinNext { at: u8 },
  /// The newline ending line `at` overwritten by a bar, joining the next line to it.
  OverwriteNewline { at: u8 },
  /// A fresh token appended to the text's end, with no newline.
  AppendTail,
}

/// A text's lines, each with its newline (the last perhaps without one).
fn lines(text: &[u8]) -> Vec<&[u8]> {
  text.split_inclusive(|byte| *byte == b'\n').collect()
}

/// The byte offset line `at` starts at (the text's length past its last line).
fn offset(lines: &[&[u8]], at: usize) -> usize {
  lines.iter().take(at).map(|line| line.len()).sum()
}

/// A token no text has held (`<serial>`), with a newline when `newline`: every line holds a token no other line
/// holds, so lines are unique and a diff of whole texts is unambiguous.
fn fresh(serial: &mut u64, newline: bool) -> Vec<u8> {
  *serial += 1;
  let mut bytes = format!("<{serial}>").into_bytes();
  if newline {
    bytes.push(b'\n');
  }
  bytes
}

impl LineEdit {
  /// The edit's change to `text`, or `None` when the text cannot take it. Every line it makes holds a fresh token.
  fn splice(self, text: &[u8], serial: &mut u64) -> Option<Splice> {
    let lines = lines(text);
    match self {
      LineEdit::InsertLines { at, count } => insert_lines(text, &lines, (at, count), serial),
      LineEdit::DeleteLines { at, count } => replace_lines(text, &lines, (at, count), 0, serial),
      LineEdit::ReplaceLines { at, count, with } => {
        replace_lines(text, &lines, (at, count), with.max(1), serial)
      }
      LineEdit::InsertInLine { at, into } => {
        insert_in_line(&lines, (at, into), fresh(serial, false))
      }
      LineEdit::SplitLine { at, into } => {
        let mut made = fresh(serial, true);
        made.extend(fresh(serial, false));
        insert_in_line(&lines, (at, into), made)
      }
      LineEdit::JoinNext { at } => join_next(&lines, at, Vec::new()),
      LineEdit::OverwriteNewline { at } => join_next(&lines, at, b"|".to_vec()),
      LineEdit::AppendTail => Some(Splice {
        at: text.len(),
        removed: 0,
        inserted: fresh(serial, false),
      }),
    }
  }
}

/// Whether `text`'s last line has no newline.
fn open_tail(text: &[u8]) -> bool {
  text.last().is_some_and(|byte| *byte != b'\n')
}

/// `count` fresh whole lines before line `at` (modulo one past the lines); `None` after a last line with no newline,
/// which the first of them would join.
fn insert_lines(
  text: &[u8],
  lines: &[&[u8]],
  (at, count): (u8, u8),
  serial: &mut u64,
) -> Option<Splice> {
  let at = usize::from(at) % (lines.len() + 1);
  if at == lines.len() && open_tail(text) {
    return None;
  }
  let inserted = (0..count.max(1))
    .flat_map(|_| fresh(serial, true))
    .collect();
  Some(Splice {
    at: offset(lines, at),
    removed: 0,
    inserted,
  })
}

/// `count` lines from line `at` (modulo the lines) replaced by `with` fresh ones, none for a delete: a replaced last
/// line with no newline by lines whose last has none.
fn replace_lines(
  text: &[u8],
  lines: &[&[u8]],
  (at, count): (u8, u8),
  with: u8,
  serial: &mut u64,
) -> Option<Splice> {
  let n = lines.len();
  let at = usize::from(at).checked_rem(n)?;
  let count = usize::from(count.max(1)).min(n - at);
  let start = offset(lines, at);
  let mut inserted: Vec<u8> = (0..with).flat_map(|_| fresh(serial, true)).collect();
  if at + count == n && open_tail(text) {
    inserted.pop();
  }
  Some(Splice {
    at: start,
    removed: offset(lines, at + count) - start,
    inserted,
  })
}

/// `made` inserted `into` bytes into line `at` (both modulo what the text holds: zero is before the line's first
/// byte, its body's length just before its newline).
fn insert_in_line(lines: &[&[u8]], (at, into): (u8, u8), made: Vec<u8>) -> Option<Splice> {
  let at = usize::from(at).checked_rem(lines.len())?;
  let line = lines[at];
  let body = line.len() - usize::from(line.ends_with(b"\n"));
  Some(Splice {
    at: offset(lines, at) + usize::from(into) % (body + 1),
    removed: 0,
    inserted: made,
  })
}

/// The newline ending line `at` (modulo every line but the last) replaced by `with`: deleted when it is empty,
/// overwritten when it is one byte.
fn join_next(lines: &[&[u8]], at: u8, with: Vec<u8>) -> Option<Splice> {
  let at = usize::from(at).checked_rem(lines.len().checked_sub(1)?)?;
  Some(Splice {
    at: offset(lines, at + 1) - 1,
    removed: 1,
    inserted: with,
  })
}

/// The splices sorted by offset, each kept only when it starts after the last kept one ends: one work's ops on a path
/// are disjoint, as the deriver composes them.
fn disjoint(mut splices: Vec<Splice>) -> Vec<Splice> {
  splices.sort_by_key(|splice| splice.at);
  let mut kept: Vec<Splice> = Vec::new();
  for splice in splices {
    if kept
      .last()
      .is_none_or(|last| splice.at > last.at + last.removed)
    {
      kept.push(splice);
    }
  }
  kept
}

/// A version as an index into the history's per-version records.
fn index(version: u64) -> usize {
  usize::try_from(version).unwrap()
}

/// One generated step of a history.
#[derive(Clone, Debug)]
enum Step {
  /// A comment on line `first` (modulo the text's lines) and `extent` more of FILE, at the version `back` behind the
  /// head.
  Comment { back: u8, first: u8, extent: u8 },
  /// A work based `back` versions behind the head making `edits` to FILE, or to OTHER when `other`.
  Edit {
    back: u8,
    other: bool,
    edits: Vec<LineEdit>,
  },
  /// FILE unlinked.
  Unlink,
  /// FILE made with `lines` fresh lines, or OTHER when `other`, where it is absent.
  Create { other: bool, lines: u8 },
  /// OTHER renamed onto FILE.
  RenameOnto,
  /// FILE renamed away to AWAY.
  RenameAway,
  /// The histories folded below the version `back` behind the head.
  Fold { back: u8 },
}

/// A comment written on FILE: the version it was written on and the byte range its lines cover there.
#[derive(Clone, Copy, Debug)]
struct Comment {
  version: u64,
  range: Range,
}

/// What a diff of whole texts says of a comment at a version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expected {
  /// It stands on these lines, first and last.
  Lines(usize, usize),
  /// The change at `at` outdated it, for this reason.
  Outdated { at: u64, by: OutdatedBy },
}

/// How often each answer was met over every case: the non-vacuity census.
#[derive(Debug, Default)]
struct Census {
  /// Comments followed through at least one later version.
  moved: u64,
  /// Of those, comments whose lines were moved off their first byte.
  shifted: u64,
  /// Outdated as their file was removed or replaced.
  replaced: u64,
  /// Outdated as a change met their lines.
  changed: u64,
  /// Outdated as text was joined to their first or last line.
  joined: u64,
  /// Refused below the fold floor.
  folded: u64,
}

/// A history under test: the green, what it held at each version, and the comments written on it.
struct History {
  green: Green,
  /// FILE's text at each version (`None`: absent), as the head held it when the version committed.
  texts: Vec<Option<Vec<u8>>>,
  /// OTHER's text at each version.
  others: Vec<Option<Vec<u8>>>,
  /// Whether each version removed or replaced FILE.
  replaced: Vec<bool>,
  /// The last version that made, removed or replaced FILE (slot 0) and OTHER (slot 1). A work is never based before
  /// it: the verdict maps a work's edit of a replaced file onto its replacement today (A-596's first finding,
  /// `a_work_based_before_its_file_was_replaced_conflicts`), which this oracle does not judge.
  made: [u64; 2],
  /// The highest floor the histories were folded below.
  floor: u64,
  comments: Vec<Comment>,
  serial: u64,
  works: u64,
}

impl History {
  /// A green of FILE (six fresh lines) and OTHER (three).
  fn new() -> History {
    let mut serial = 0;
    let mut text =
      |count: usize| -> Vec<u8> { (0..count).flat_map(|_| fresh(&mut serial, true)).collect() };
    let (file, other) = (text(6), text(3));
    let mut origin = Origin::default();
    origin.files.push((FILE.to_owned(), file.clone()));
    origin.files.push((OTHER.to_owned(), other.clone()));
    History {
      green: Green::with_origin(origin),
      texts: vec![Some(file)],
      others: vec![Some(other)],
      replaced: vec![false],
      made: [0, 0],
      floor: 0,
      comments: Vec::new(),
      serial,
      works: 0,
    }
  }

  /// `path`'s text at `version`.
  fn text_of(&self, path: &str, version: u64) -> Option<Vec<u8>> {
    let texts = if path == FILE {
      &self.texts
    } else {
      &self.others
    };
    texts.get(index(version)).cloned().flatten()
  }

  /// Submits `work` based at `base`; when accepted, records what the new head holds, whether it replaced FILE, and
  /// marks the files of `slots` made at it.
  fn submit(
    &mut self,
    work: Work,
    base: u64,
    replaces: bool,
    slots: &[usize],
  ) -> Result<(), TestCaseError> {
    self.works += 1;
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&self.works.to_le_bytes());
    let increment = Increment {
      id,
      base,
      doc: work.doc,
      post_state: work.post,
      evidence: Vec::new(),
    };
    if let Outcome::Accepted { version } = self.green.submit(&increment) {
      prop_assert_eq!(version, self.texts.len() as u64, "versions are dense");
      self
        .texts
        .push(self.green.content(FILE).map(<[u8]>::to_vec));
      self
        .others
        .push(self.green.content(OTHER).map(<[u8]>::to_vec));
      self.replaced.push(replaces);
      for slot in slots {
        self.made[*slot] = version;
      }
    }
    Ok(())
  }

  fn step(&mut self, step: &Step) -> Result<(), TestCaseError> {
    match step {
      Step::Comment {
        back,
        first,
        extent,
      } => {
        self.comment(*back, *first, *extent);
        Ok(())
      }
      Step::Fold { back } => {
        let floor = self.green.head().saturating_sub(u64::from(*back));
        self.green.fold_history_before(floor);
        self.floor = self.floor.max(floor);
        Ok(())
      }
      Step::Edit { back, other, edits } => self.edit(*back, *other, edits),
      Step::Create { other, lines } => self.create(*other, *lines),
      Step::Unlink => self.replace(FILE, &[0], |work| {
        work.op(OpKind::Unlink, FILE, 0, 0, u64::MAX);
      }),
      Step::RenameOnto => self.replace(OTHER, &[0, 1], |work| {
        let src = work.name(OTHER);
        work.op(OpKind::Rename, FILE, 0, 0, src);
      }),
      Step::RenameAway => self.replace(FILE, &[0], |work| {
        let src = work.name(FILE);
        work.op(OpKind::Rename, AWAY, 0, 0, src);
      }),
    }
  }

  /// A work based `back` versions behind the head (never before the floor, nor before its file was last made) making
  /// `edits` to FILE, or to OTHER when `other`, where the file stands at that base.
  fn edit(&mut self, back: u8, other: bool, edits: &[LineEdit]) -> Result<(), TestCaseError> {
    let (path, slot) = if other { (OTHER, 1) } else { (FILE, 0) };
    let base = self
      .green
      .head()
      .saturating_sub(u64::from(back))
      .max(self.floor)
      .max(self.made[slot]);
    let Some(text) = self.text_of(path, base) else {
      return Ok(());
    };
    let splices = edits
      .iter()
      .filter_map(|edit| edit.splice(&text, &mut self.serial))
      .collect();
    let mut work = Work::default();
    work.splices(path, text.len(), &disjoint(splices));
    // A truncate based behind the head drops whatever the green appended at its end after its base, and its delta
    // states only the bytes it saw (A-596's second finding, `a_truncate_based_before_an_append_keeps_the_appended_bytes`):
    // this oracle judges comments, not that, so such a work is not submitted.
    let truncates = work.doc.ops.iter().any(|op| op.kind == OpKind::Truncate);
    if work.doc.ops.is_empty() || (truncates && base < self.green.head()) {
      return Ok(());
    }
    self.submit(work, base, false, &[])
  }

  /// FILE, or OTHER when `other`, made with `lines` fresh lines at the head, where it is absent.
  fn create(&mut self, other: bool, lines: u8) -> Result<(), TestCaseError> {
    let (path, slot) = if other { (OTHER, 1) } else { (FILE, 0) };
    let head = self.green.head();
    if self.text_of(path, head).is_some() {
      return Ok(());
    }
    let bytes: Vec<u8> = (0..lines)
      .flat_map(|_| fresh(&mut self.serial, true))
      .collect();
    let mut work = Work::default();
    work.op(OpKind::Create, path, 0, 0, u64::MAX);
    let src = work.stash(&bytes);
    work.op(OpKind::Insert, path, 0, bytes.len() as u64, src);
    self.submit(work, head, !other, &[slot])
  }

  /// A work at the head that `build` lays out, removing or replacing FILE, where `needs` stands at the head; it makes
  /// the files of `slots`.
  fn replace(
    &mut self,
    needs: &str,
    slots: &[usize],
    build: impl FnOnce(&mut Work),
  ) -> Result<(), TestCaseError> {
    let head = self.green.head();
    if self.text_of(needs, head).is_none() {
      return Ok(());
    }
    let mut work = Work::default();
    build(&mut work);
    self.submit(work, head, true, slots)
  }

  /// Writes a comment on FILE at the version `back` behind the head, on line `first` (modulo its lines) and `extent`
  /// more, when that version is not folded and FILE holds lines there.
  fn comment(&mut self, back: u8, first: u8, extent: u8) {
    let version = self.green.head().saturating_sub(u64::from(back));
    if version < self.floor {
      return;
    }
    let Some(text) = self.text_of(FILE, version) else {
      return;
    };
    let count = lines(&text).len();
    if count == 0 {
      return;
    }
    let first = usize::from(first) % count;
    let last = (first + usize::from(extent)).min(count - 1);
    let range = line_range(&text, first, last).unwrap();
    if self.comments.len() == COMMENTS {
      self.comments.remove(0);
    }
    self.comments.push(Comment { version, range });
  }

  /// What a diff of the whole texts the green held at each commit says of `comment`, at its version and every later
  /// one: lines move while every one of them stands, together and in order, in the next text; otherwise the comment
  /// is outdated, for good, as its file was removed or replaced, or as its bytes stand together in the next text with
  /// other text on its first or last line (joined), or not (changed).
  fn model(&self, comment: &Comment) -> Vec<Expected> {
    let text = self.texts[index(comment.version)].as_deref().unwrap();
    let (mut first, mut last) = lines_of(text, comment.range).unwrap();
    let mut answers = vec![Expected::Lines(first, last)];
    let mut outdated = None;
    for version in comment.version + 1..=self.green.head() {
      if outdated.is_none() {
        let before = self.texts[index(version) - 1].as_deref().unwrap();
        outdated = match self.texts[index(version)].as_deref() {
          Some(after) if !self.replaced[index(version)] => {
            match model_step(before, after, first, last) {
              Some((moved_first, moved_last)) => {
                (first, last) = (moved_first, moved_last);
                None
              }
              None => Some(Expected::Outdated {
                at: version,
                by: gone_because(before, after, first, last),
              }),
            }
          }
          _ => Some(Expected::Outdated {
            at: version,
            by: OutdatedBy::FileReplaced,
          }),
        };
      }
      answers.push(outdated.unwrap_or(Expected::Lines(first, last)));
    }
    answers
  }

  /// Follows every comment to every version from its own to the head, and holds each answer to the model's.
  fn check(&self, census: &mut Census) -> Result<(), TestCaseError> {
    let head = self.green.head();
    for comment in &self.comments {
      let expected = (comment.version >= self.floor).then(|| self.model(comment));
      for to in comment.version..=head {
        let answer = self
          .green
          .reanchor(FILE, comment.range, (comment.version, to));
        let Some(expected) = &expected else {
          prop_assert_eq!(
            answer,
            Err(ReanchorRefused::Folded { floor: self.floor }),
            "{:?} to {}",
            comment,
            to
          );
          census.folded += 1;
          continue;
        };
        match expected[index(to - comment.version)] {
          Expected::Lines(first, last) => {
            let text = self.texts[index(to)].as_deref().unwrap();
            let range = line_range(text, first, last).unwrap();
            prop_assert_eq!(
              answer,
              Ok(Reanchored::Moved(range)),
              "{:?} to {}: {:?}",
              comment,
              to,
              String::from_utf8_lossy(text)
            );
            if to > comment.version {
              census.moved += 1;
              if range.start != comment.range.start {
                census.shifted += 1;
              }
            }
          }
          Expected::Outdated { at, by } => {
            prop_assert_eq!(
              answer,
              Ok(Reanchored::Outdated { at, by }),
              "{:?} to {}",
              comment,
              to
            );
            match by {
              OutdatedBy::FileReplaced => census.replaced += 1,
              OutdatedBy::LinesChanged => census.changed += 1,
              OutdatedBy::LinesJoined => census.joined += 1,
            }
          }
        }
      }
    }
    Ok(())
  }
}

/// Where lines `first..=last` of `before` stand in `after`: each line unique, so a line is unchanged exactly when
/// `after` holds a line of its bytes; the run moves when every one of its lines does, together and in order.
fn model_step(before: &[u8], after: &[u8], first: usize, last: usize) -> Option<(usize, usize)> {
  let (before, after) = (lines(before), lines(after));
  let start = after.iter().position(|line| *line == before[first])?;
  let together = before[first..=last]
    .iter()
    .enumerate()
    .all(|(offset, line)| after.get(start + offset) == Some(line));
  together.then_some((start, start + (last - first)))
}

/// Why lines `first..=last` of `before` are gone from `after`: their bytes stand together in `after` (text was joined
/// to the first or last of them), or not (a change met them).
fn gone_because(before: &[u8], after: &[u8], first: usize, last: usize) -> OutdatedBy {
  let range = line_range(before, first, last).unwrap();
  let bytes = &before[index(range.start)..index(range.end())];
  if after.windows(bytes.len()).any(|window| window == bytes) {
    OutdatedBy::LinesJoined
  } else {
    OutdatedBy::LinesChanged
  }
}

fn line_edit() -> impl Strategy<Value = LineEdit> {
  prop_oneof![
    (any::<u8>(), 1..3u8).prop_map(|(at, count)| LineEdit::InsertLines { at, count }),
    (any::<u8>(), 1..3u8).prop_map(|(at, count)| LineEdit::DeleteLines { at, count }),
    (any::<u8>(), 1..3u8, 1..3u8).prop_map(|(at, count, with)| LineEdit::ReplaceLines {
      at,
      count,
      with
    }),
    (any::<u8>(), any::<u8>()).prop_map(|(at, into)| LineEdit::InsertInLine { at, into }),
    (any::<u8>(), any::<u8>()).prop_map(|(at, into)| LineEdit::SplitLine { at, into }),
    any::<u8>().prop_map(|at| LineEdit::JoinNext { at }),
    any::<u8>().prop_map(|at| LineEdit::OverwriteNewline { at }),
    Just(LineEdit::AppendTail),
  ]
}

fn step() -> impl Strategy<Value = Step> {
  prop_oneof![
    4 => (0..6u8, any::<u8>(), 0..3u8)
      .prop_map(|(back, first, extent)| Step::Comment { back, first, extent }),
    8 => (0..4u8, prop::bool::weighted(0.2), prop::collection::vec(line_edit(), 1..3))
      .prop_map(|(back, other, edits)| Step::Edit { back, other, edits }),
    1 => Just(Step::Unlink),
    2 => (prop::bool::weighted(0.3), 1..6u8).prop_map(|(other, lines)| Step::Create { other, lines }),
    1 => Just(Step::RenameOnto),
    1 => Just(Step::RenameAway),
    1 => (0..8u8).prop_map(|back| Step::Fold { back }),
  ]
}

/// A-596's oracle (§4.17.1 "Works over works": "A comment's range moves with the code: §4.16's position mapping shifts
/// it through every delta in between, so a comment stays on its line across rebases. A range that an edit replaced is
/// marked outdated; it is never moved by a guess"). Do: run generated histories over a green holding two files: works
/// based at the head or behind it making line edits (whole lines inserted, deleted or replaced, text inserted inside a
/// line or before its first byte, a line split or joined to the next, a newline overwritten, text appended to the end)
/// to the commented file or the other, the file unlinked, made again, renamed onto or renamed away, comments written at
/// versions the green reconstructs, and the histories folded below generated floors; after every step follow every
/// comment by position mapping to every version from its own to the head. Expect each answer to equal a diff of the
/// whole texts the green held at each commit, line by line: the same lines while it moves, outdated at the same
/// version for the same reason (file replaced, lines changed, text joined to them), and refused below the floor. The
/// census shows every answer met.
#[test]
fn a_comment_follows_its_lines_as_a_diff_of_whole_texts_does_over_every_history() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config::default()));
  let result = runner.run(&prop::collection::vec(step(), 1..48), |steps| {
    let mut history = History::new();
    for step in &steps {
      history.step(step)?;
      history.check(&mut census.borrow_mut())?;
    }
    Ok(())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  let met = [
    census.moved,
    census.shifted,
    census.replaced,
    census.changed,
    census.joined,
    census.folded,
  ];
  assert!(
    met.iter().all(|&count| count > 0),
    "an answer never met: {census:?}"
  );
}

/// A green of FILE holding `text` and OTHER holding `other`.
fn green_of(text: &[u8], other: &[u8]) -> Green {
  let mut origin = Origin::default();
  origin.files.push((FILE.to_owned(), text.to_vec()));
  origin.files.push((OTHER.to_owned(), other.to_vec()));
  Green::with_origin(origin)
}

/// Submits `work` to `green` based at `base` as increment `id`, returning what the green answered.
fn submit(green: &mut Green, work: Work, base: u64, id: u8) -> Outcome {
  let mut identity = [0u8; 32];
  identity[0] = id;
  green.submit(&Increment {
    id: identity,
    base,
    doc: work.doc,
    post_state: work.post,
    evidence: Vec::new(),
  })
}

/// A work of one op on `path` adding `bytes` (or none).
fn one(kind: OpKind, path: &str, at: u64, len: u64, bytes: &[u8]) -> Work {
  let mut work = Work::default();
  let src = if bytes.is_empty() {
    u64::MAX
  } else {
    work.stash(bytes)
  };
  work.op(kind, path, at, len, src);
  work
}

/// A-596. Do: comment on lines 1 and 2 of four, then insert a line above them, edit the other file, and insert text
/// inside line 2. Expect the comment moved down a line through the first two versions, its lines read back there, and
/// outdated at the third as a change met them, whether followed from where it was written or from where it was moved.
#[test]
fn a_comment_moves_with_lines_added_above_it_and_is_outdated_by_a_change_inside_them() {
  let mut green = green_of(b"zero\none\ntwo\nthree\n", b"other\n");
  let range = line_range(b"zero\none\ntwo\nthree\n", 1, 2).unwrap();
  let above = one(OpKind::Insert, FILE, 0, 4, b"new\n");
  assert_eq!(
    submit(&mut green, above, 0, 1),
    Outcome::Accepted { version: 1 }
  );
  let elsewhere = one(OpKind::Overwrite, OTHER, 0, 5, b"OTHER");
  assert_eq!(
    submit(&mut green, elsewhere, 1, 2),
    Outcome::Accepted { version: 2 }
  );
  let moved = Range::new(9, 8);
  assert_eq!(
    green.reanchor(FILE, range, (0, 2)),
    Ok(Reanchored::Moved(moved))
  );
  assert_eq!(
    lines_of(&green.content_at(FILE, 2).unwrap(), moved),
    Some((2, 3))
  );
  let inside = one(OpKind::Insert, FILE, 11, 1, b"x");
  assert_eq!(
    submit(&mut green, inside, 2, 3),
    Outcome::Accepted { version: 3 }
  );
  let outdated = Ok(Reanchored::Outdated {
    at: 3,
    by: OutdatedBy::LinesChanged,
  });
  assert_eq!(green.reanchor(FILE, range, (0, 3)), outdated);
  assert_eq!(green.reanchor(FILE, moved, (2, 3)), outdated);
  assert_eq!(
    green.reanchor(FILE, range, (0, 2)),
    Ok(Reanchored::Moved(moved))
  );
}

/// A-596. Do: comment on line 0 of the file, then (each on its own green) rename the other file onto it, unlink it,
/// and rename it away. Expect the comment outdated at that version each time as its file was replaced or removed,
/// never moved onto the bytes that took its place.
#[test]
fn a_comment_is_outdated_when_its_file_is_renamed_onto_unlinked_or_renamed_away() {
  let text = b"zero\none\n";
  let range = line_range(text, 0, 0).unwrap();
  let mut onto = Work::default();
  let src = onto.name(OTHER);
  onto.op(OpKind::Rename, FILE, 0, 0, src);
  let mut away = Work::default();
  let src = away.name(FILE);
  away.op(OpKind::Rename, AWAY, 0, 0, src);
  let unlink = one(OpKind::Unlink, FILE, 0, 0, b"");
  for (work, what) in [
    (onto, "renamed onto"),
    (unlink, "unlinked"),
    (away, "renamed away"),
  ] {
    // The other file repeats the commented line, so a guess could have moved the comment onto it.
    let mut green = green_of(text, b"zero\none\nzero\n");
    assert_eq!(
      submit(&mut green, work, 0, 1),
      Outcome::Accepted { version: 1 },
      "{what}"
    );
    assert_eq!(
      green.reanchor(FILE, range, (0, 1)),
      Ok(Reanchored::Outdated {
        at: 1,
        by: OutdatedBy::FileReplaced
      }),
      "{what}"
    );
  }
}

/// A-596. Do: ask a green to follow a comment past its head, backward, from a version below its fold floor, on a range
/// that is not whole lines, on a file absent at the comment's version, and on a path no change ever touched. Expect
/// each refused by its kind (`PastHead` with the head, `Backward`, `Folded` with the floor, `NotLines`), and a whole-lines
/// range on the untouched path moved where it stood.
#[test]
fn a_comment_is_refused_past_the_head_backward_below_the_floor_or_off_whole_lines() {
  let mut green = green_of(b"zero\none\ntwo\n", b"other\n");
  let range = line_range(b"zero\none\ntwo\n", 1, 1).unwrap();
  for id in 1..=3u8 {
    let work = one(OpKind::Insert, FILE, 0, 2, b"x\n");
    assert_eq!(
      submit(&mut green, work, u64::from(id) - 1, id),
      Outcome::Accepted {
        version: u64::from(id)
      }
    );
  }
  assert_eq!(
    green.reanchor(FILE, range, (0, 4)),
    Err(ReanchorRefused::PastHead { head: 3 })
  );
  assert_eq!(
    green.reanchor(FILE, range, (2, 1)),
    Err(ReanchorRefused::Backward)
  );
  assert_eq!(
    green.reanchor(FILE, Range::new(6, 3), (0, 3)),
    Err(ReanchorRefused::NotLines)
  );
  assert_eq!(
    green.reanchor("absent.txt", range, (0, 3)),
    Err(ReanchorRefused::NotLines)
  );
  assert_eq!(
    green.reanchor(OTHER, Range::new(0, 6), (0, 3)),
    Ok(Reanchored::Moved(Range::new(0, 6)))
  );
  assert_eq!(
    green.reanchor(OTHER, Range::new(0, 3), (0, 3)),
    Err(ReanchorRefused::NotLines)
  );
  assert_eq!(
    green.reanchor(FILE, range, (0, 3)),
    Ok(Reanchored::Moved(Range::new(11, 4)))
  );
  green.fold_history_before(2);
  assert_eq!(
    green.reanchor(FILE, range, (0, 3)),
    Err(ReanchorRefused::Folded { floor: 2 })
  );
  assert_eq!(
    green.reanchor(FILE, Range::new(9, 4), (2, 3)),
    Ok(Reanchored::Moved(Range::new(11, 4)))
  );
}

/// A-596's finding, parked until the verdict is fixed (GAPS, §4.16; A-596). Do: on one green unlink the file and make
/// it again with other bytes, on another rename the other file onto it; then submit a work based before that change
/// that inserts a byte into the file it saw. Expect a conflict both times: the file the work edited is gone. Today the
/// verdict maps the work's insert through the replacement's one-insert delta and accepts it, splicing it into the new
/// file (`merge_content` finds the path's content present and maps through every delta since the base; `fits_base`
/// judged the ops against the base's file only).
#[test]
#[ignore = "A-596's finding: a work based before its file was replaced is spliced into the replacement; un-ignore with the verdict's fix"]
fn a_work_based_before_its_file_was_replaced_conflicts() {
  let text = b"<1>\n<2>\n";
  let mut remade = green_of(text, b"other\n");
  let unlink = one(OpKind::Unlink, FILE, 0, 0, b"");
  assert_eq!(
    submit(&mut remade, unlink, 0, 1),
    Outcome::Accepted { version: 1 }
  );
  let mut create = one(OpKind::Create, FILE, 0, 0, b"");
  let src = create.stash(b"<9>\n");
  create.op(OpKind::Insert, FILE, 0, 4, src);
  assert_eq!(
    submit(&mut remade, create, 1, 2),
    Outcome::Accepted { version: 2 }
  );
  let mut onto = green_of(text, b"other\n");
  let mut rename = Work::default();
  let src = rename.name(OTHER);
  rename.op(OpKind::Rename, FILE, 0, 0, src);
  assert_eq!(
    submit(&mut onto, rename, 0, 1),
    Outcome::Accepted { version: 1 }
  );
  for (green, what) in [(&mut remade, "made again"), (&mut onto, "renamed onto")] {
    let head = green.head();
    let edit = one(OpKind::Insert, FILE, 1, 1, b"x");
    let answer = submit(green, edit, 0, 9);
    assert!(
      matches!(answer, Outcome::Conflict { .. }),
      "{what}: {answer:?}, the file now {:?}",
      green.content(FILE).map(String::from_utf8_lossy)
    );
    assert_eq!(green.head(), head, "{what}");
  }
}

/// A-596's second finding, parked until the verdict is fixed (GAPS, §4.16; A-596; found by this module's oracle, Mac
/// Studio kiln r504). Do: append a line to a two-line file, then submit a work based before the append that deletes
/// the file's last line, its removal to the end composed as the deriver composes it, a truncate. Expect the appended
/// line kept (the work removed only what it saw) or a conflict, never the appended line gone with nothing refused.
/// Today the truncate meets the append only at its edge, so the verdict accepts it, and the splice truncates to the
/// end of the head's file (`apply`, `OpKind::Truncate`), removing the append the work never saw; the version's delta
/// still states the four bytes the work saw.
#[test]
#[ignore = "A-596's second finding: a truncate based before an append removes the appended bytes; un-ignore with the verdict's fix"]
fn a_truncate_based_before_an_append_keeps_the_appended_bytes() {
  let mut green = green_of(b"<1>\n<2>\n", b"other\n");
  let append = one(OpKind::Extend, FILE, 8, 4, b"<3>\n");
  assert_eq!(
    submit(&mut green, append, 0, 1),
    Outcome::Accepted { version: 1 }
  );
  let truncate = one(OpKind::Truncate, FILE, 4, 4, b"");
  let answer = submit(&mut green, truncate, 0, 2);
  let file = green.content(FILE).map(<[u8]>::to_vec);
  assert!(
    matches!(answer, Outcome::Conflict { .. }) || file.as_deref() == Some(&b"<1>\n<3>\n"[..]),
    "{answer:?}, the file now {:?}",
    file.as_deref().map(String::from_utf8_lossy)
  );
}
