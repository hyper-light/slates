//! A work never changes bytes it did not see (A-612; §4.16 "The verdict", "Splice"; D-27's lost designs: last-writer-wins
//! and an inferred merge). Two defects A-596's oracle found, and their siblings:
//! - a work based before its file was replaced (removed and made again, renamed onto, or replaced beneath a directory's
//!   rename) edits a file that is gone: the verdict answers delete against modify (`DeleteModify`), and a rename of it
//!   rename against rename (`RenameRename`), never an edit or a move of the file that took its place;
//! - a truncate is the removal of the bytes it saw, a hunk over its base's bytes (`slates_vfs::algebra`): judged by its
//!   span with the edge rule as every removal is, and applied as exactly that span at the head, so an append the green
//!   made at its end after its base is kept, in a submit, a rebase and a rename's edits alike, and the version's delta
//!   states what left.

use super::{ConflictWindow, Green, Increment, Outcome, Rebased};
use crate::increment::VolumeOp;
use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::origin::Origin;
use crate::range::Range;
use crate::verdict::MergeConflictClass;
use std::collections::BTreeMap;

/// An increment under construction, laid out as the deriver lays one out.
#[derive(Default)]
struct Work {
  doc: OpsDoc,
  post: Vec<u8>,
}

impl Work {
  /// Adds `kind` on `path` at `at` of `len` bytes, its added bytes `bytes` (none for a removal or a namespace op).
  fn op(mut self, kind: OpKind, path: &str, (at, len): (u64, u64), bytes: &[u8]) -> Work {
    let src = if bytes.is_empty() {
      u64::MAX
    } else {
      let src = self.post.len() as u64;
      self.post.extend_from_slice(bytes);
      src
    };
    let path = self.doc.paths.intern(path);
    self.doc.ops.push(Op {
      kind,
      flags: 0,
      path,
      at,
      len,
      src,
    });
    self
  }

  /// Adds a rename of `from` onto `to`.
  fn rename(mut self, from: &str, to: &str) -> Work {
    let src = self.doc.paths.intern(from);
    let path = self.doc.paths.intern(to);
    self.doc.ops.push(Op {
      kind: OpKind::Rename,
      flags: 0,
      path,
      at: 0,
      len: 0,
      src,
    });
    self
  }

  /// The increment `id` of this work based at `base`.
  fn at(self, base: u64, id: u8) -> Increment {
    let mut identity = [0u8; 32];
    identity[0] = id;
    Increment {
      id: identity,
      base,
      doc: self.doc,
      post_state: self.post,
      evidence: Vec::new(),
      named: Default::default(),
    }
  }
}

/// A green of `files`, under `dirs`.
fn green_of(files: &[(&str, &[u8])], dirs: &[&str]) -> Green {
  let mut origin = Origin::default();
  for (path, bytes) in files {
    origin.files.push(((*path).to_owned(), bytes.to_vec()));
  }
  origin.dirs = dirs.iter().map(|dir| (*dir).to_owned()).collect();
  Green::with_origin(origin)
}

/// Submits `increment`, expecting it accepted as `version`.
fn accept(green: &mut Green, increment: &Increment, version: u64) {
  assert_eq!(green.submit(increment), Outcome::Accepted { version });
}

/// The conflict classes of `outcome`, or what it was instead.
fn classes(outcome: &Outcome) -> Result<Vec<MergeConflictClass>, String> {
  match outcome {
    Outcome::Conflict { windows } => Ok(
      windows
        .iter()
        .map(|window: &ConflictWindow| window.class)
        .collect(),
    ),
    other => Err(format!("{other:?}")),
  }
}

/// A file's bytes at the head, as text.
fn text(green: &Green, path: &str) -> Option<String> {
  green
    .content(path)
    .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
}

/// The greens of the three ways a file is replaced after a work's base, each with the version it was replaced at and
/// the file that replaced it: removed then made again (two versions), another file renamed onto it, and a directory's
/// rename moving another file onto its path (`src/f` renamed out to `old/f`, `new/f` renamed onto `src/f`, in one
/// increment, as the deriver lays out a directory's rename as its children's).
fn replaced() -> Vec<(&'static str, &'static str, Green, &'static str)> {
  let file: &[u8] = b"<1>\n<2>\n";
  let mut remade = green_of(&[("f", file), ("g", b"g\n")], &[]);
  accept(
    &mut remade,
    &Work::default()
      .op(OpKind::Unlink, "f", (0, 0), b"")
      .at(0, 1),
    1,
  );
  let made =
    Work::default()
      .op(OpKind::Create, "f", (0, 0), b"")
      .op(OpKind::Insert, "f", (0, 4), b"<9>\n");
  accept(&mut remade, &made.at(1, 2), 2);
  let mut onto = green_of(&[("f", file), ("g", b"g\n")], &[]);
  accept(&mut onto, &Work::default().rename("g", "f").at(0, 1), 1);
  let mut beneath = green_of(&[("src/f", file), ("new/f", b"new\n")], &["src", "new"]);
  let rotated = Work::default()
    .op(OpKind::Mkdir, "old", (0, 0), b"")
    .rename("src/f", "old/f")
    .rename("new/f", "src/f")
    .op(OpKind::Rmdir, "new", (0, 0), b"");
  accept(&mut beneath, &rotated.at(0, 1), 1);
  vec![
    ("made again", "f", remade, "<9>\n"),
    ("renamed onto", "f", onto, "g\n"),
    (
      "replaced beneath a directory's rename",
      "src/f",
      beneath,
      "new\n",
    ),
  ]
}

/// A-612 (A-596's first finding; §4.16 "The verdict": "delete against modify in either direction"). Do: replace a file
/// after version 0 each of the three ways (made again, renamed onto, replaced beneath a directory's rename), then
/// submit a work based at 0 that inserts a byte into the file it saw. Expect a `DeleteModify` conflict each time, the
/// head unmoved and the file that took its place unchanged; and the same insert based at the head accepted.
#[test]
fn a_work_based_before_its_file_was_replaced_conflicts() {
  for (how, path, mut green, now) in replaced() {
    let head = green.head();
    let edit = Work::default().op(OpKind::Insert, path, (1, 1), b"x");
    let answer = green.submit(&edit.at(0, 9));
    assert_eq!(
      classes(&answer),
      Ok(vec![MergeConflictClass::DeleteModify]),
      "{how}"
    );
    assert_eq!(green.head(), head, "{how}");
    assert_eq!(text(&green, path).as_deref(), Some(now), "{how}");
    let edit = Work::default().op(OpKind::Insert, path, (1, 1), b"x");
    assert_eq!(
      green.submit(&edit.at(head, 10)),
      Outcome::Accepted { version: head + 1 },
      "{how}: a work that saw the new file"
    );
  }
}

/// A-612, a sibling of A-596's first finding (§4.16 "The verdict": "rename against rename"). Do: replace a file after
/// version 0 each of the three ways, then submit a work based at 0 that renames the file it saw to a free path. Expect
/// a `RenameRename` conflict each time, the file that took its place where it stands and the free path still free:
/// the rename never moves a file the work did not see.
#[test]
fn a_rename_based_before_its_source_was_replaced_conflicts() {
  for (how, path, mut green, now) in replaced() {
    let head = green.head();
    let answer = green.submit(&Work::default().rename(path, "moved").at(0, 9));
    assert_eq!(
      classes(&answer),
      Ok(vec![MergeConflictClass::RenameRename]),
      "{how}"
    );
    assert_eq!(green.head(), head, "{how}");
    assert_eq!(text(&green, path).as_deref(), Some(now), "{how}");
    assert_eq!(text(&green, "moved"), None, "{how}");
  }
}

/// A-612 (A-596's second finding; §4.16 "The verdict" and "Splice", `slates_vfs::algebra`'s rule that a removal is of
/// the base bytes it covers). Do: append a line to a two-line file, then submit a work based before the append that
/// deletes the file's last line, composed as the deriver composes a removal to the end, a truncate. Expect it accepted
/// (the append begins at the truncate's end: disjoint by the edge rule) with both kept, the file `<1>\n<3>\n`, and the
/// version's delta stating what left: a comment on the appended line follows it up a line, and one on the first line
/// stays.
#[test]
fn a_truncate_based_before_an_append_keeps_the_appended_bytes() {
  let mut green = green_of(&[("f", b"<1>\n<2>\n")], &[]);
  accept(
    &mut green,
    &Work::default()
      .op(OpKind::Extend, "f", (8, 4), b"<3>\n")
      .at(0, 1),
    1,
  );
  accept(
    &mut green,
    &Work::default()
      .op(OpKind::Truncate, "f", (4, 4), b"")
      .at(0, 2),
    2,
  );
  assert_eq!(text(&green, "f").as_deref(), Some("<1>\n<3>\n"));
  assert_eq!(
    green.reanchor("f", Range::new(8, 4), (1, 2)),
    Ok(super::Reanchored::Moved(Range::new(4, 4)))
  );
  assert_eq!(
    green.reanchor("f", Range::new(0, 4), (1, 2)),
    Ok(super::Reanchored::Moved(Range::new(0, 4)))
  );
}

/// A-612 (a sibling: the rebase, §4.16 "Rebase, the only corrective path"). Do: append a line to a two-line file, then
/// rebase a work based before the append that truncates the file's last line. Expect a clean rebase whose file keeps
/// the appended line (`<1>\n<3>\n`) and whose journal removes only the line the work saw, a delete at its offset, never
/// a truncate that would cut the append on the work's next submit.
#[test]
fn a_rebased_truncate_keeps_the_appended_bytes() {
  let mut green = green_of(&[("f", b"<1>\n<2>\n")], &[]);
  accept(
    &mut green,
    &Work::default()
      .op(OpKind::Extend, "f", (8, 4), b"<3>\n")
      .at(0, 1),
    1,
  );
  let rebased = green.rebase(
    &Work::default()
      .op(OpKind::Truncate, "f", (4, 4), b"")
      .at(0, 2),
  );
  let Rebased::Rebased {
    version,
    changed,
    journal,
  } = rebased
  else {
    panic!("not a clean rebase: {rebased:?}");
  };
  assert_eq!(version, 1);
  assert_eq!(
    changed,
    BTreeMap::from([("f".to_owned(), Some(b"<1>\n<3>\n".to_vec()))])
  );
  assert_eq!(
    journal,
    vec![VolumeOp::Delete {
      path: "f".to_owned(),
      at: 4,
      len: 4
    }]
  );
}

/// A-612 (a sibling: a rename's edits, A-547). Do: append a line to a two-line file, then submit a work based before
/// the append that renames the file and truncates the renamed file's last line. Expect it accepted, the renamed file
/// keeping the appended line (`<1>\n<3>\n`) and the source gone: the edit after the rename removes only what the work
/// saw, as an edit in place does.
#[test]
fn a_truncate_after_a_rename_keeps_what_the_green_appended_to_the_source() {
  let mut green = green_of(&[("f", b"<1>\n<2>\n")], &[]);
  accept(
    &mut green,
    &Work::default()
      .op(OpKind::Extend, "f", (8, 4), b"<3>\n")
      .at(0, 1),
    1,
  );
  let moved = Work::default()
    .rename("f", "g")
    .op(OpKind::Truncate, "g", (4, 4), b"");
  accept(&mut green, &moved.at(0, 2), 2);
  assert_eq!(text(&green, "g").as_deref(), Some("<1>\n<3>\n"));
  assert_eq!(text(&green, "f"), None);
}

/// `answer` as the span test expects of the case `what`: accepted with `f` reading `now`, or a conflict of one `class`.
fn judged_as(
  green: &Green,
  answer: &Outcome,
  expected: Result<&str, MergeConflictClass>,
  what: &str,
) {
  match expected {
    Ok(now) => {
      assert!(
        matches!(answer, Outcome::Accepted { .. }),
        "{what}: {answer:?}"
      );
      assert_eq!(text(green, "f").as_deref(), Some(now), "{what}");
    }
    Err(class) => assert_eq!(classes(answer), Ok(vec![class]), "{what}"),
  }
}

/// A-612's sweep (§4.16 "The verdict": overlap or containment conflicts, an edge does not; `slates_vfs::algebra`'s
/// rule). Do: on a three-line file, submit a work based at 0 that truncates after the first line, against each change
/// the green made at its cut after the base; and the other way round, a work based at 0 that changes bytes at the cut
/// of a truncate the green made. Expect each judged by the spans: an overwrite ending at the cut, an insert at the cut
/// and an append at the end kept beside the truncate; an overwrite or a shorter truncate inside its span a conflict; the
/// same truncate identical; and against the green's truncate, an overwrite ending at the cut kept, one inside the cut
/// span a conflict, an append kept after the cut.
#[test]
fn a_truncate_and_a_change_at_its_cut_are_judged_by_their_spans() {
  let file: &[u8] = b"<1>\n<2>\n<3>\n";
  let truncate = || Work::default().op(OpKind::Truncate, "f", (4, 8), b"");
  let cases: [(&str, Work, Result<&str, MergeConflictClass>); 6] = [
    (
      "an overwrite ending at the cut",
      Work::default().op(OpKind::Overwrite, "f", (0, 4), b"<9>\n"),
      Ok("<9>\n"),
    ),
    (
      "an insert at the cut",
      Work::default().op(OpKind::Insert, "f", (4, 4), b"<x>\n"),
      Ok("<1>\n<x>\n"),
    ),
    (
      "an append at the end",
      Work::default().op(OpKind::Extend, "f", (12, 4), b"<4>\n"),
      Ok("<1>\n<4>\n"),
    ),
    (
      "an overwrite inside the cut span",
      Work::default().op(OpKind::Overwrite, "f", (4, 4), b"<8>\n"),
      Err(MergeConflictClass::Overlap),
    ),
    (
      "a shorter truncate",
      Work::default().op(OpKind::Truncate, "f", (8, 4), b""),
      Err(MergeConflictClass::Overlap),
    ),
    ("the same truncate", truncate(), Ok("<1>\n")),
  ];
  for (what, first, expected) in cases {
    let mut green = green_of(&[("f", file)], &[]);
    accept(&mut green, &first.at(0, 1), 1);
    let answer = green.submit(&truncate().at(0, 2));
    judged_as(&green, &answer, expected, what);
  }
  let against: [(&str, Work, Result<&str, MergeConflictClass>); 3] = [
    (
      "an overwrite ending at the cut",
      Work::default().op(OpKind::Overwrite, "f", (0, 4), b"<9>\n"),
      Ok("<9>\n"),
    ),
    (
      "an overwrite inside the cut span",
      Work::default().op(OpKind::Overwrite, "f", (4, 4), b"<8>\n"),
      Err(MergeConflictClass::Overlap),
    ),
    (
      "an append at the end",
      Work::default().op(OpKind::Extend, "f", (12, 4), b"<4>\n"),
      Ok("<1>\n<4>\n"),
    ),
  ];
  for (what, later, expected) in against {
    let mut green = green_of(&[("f", file)], &[]);
    accept(&mut green, &truncate().at(0, 1), 1);
    let answer = green.submit(&later.at(0, 2));
    judged_as(
      &green,
      &answer,
      expected,
      &format!("against the green's truncate, {what}"),
    );
  }
}
