//! Worked cases of works over works and their refusals (A-659; the module's docs), and the helpers the oracles share.

use super::{Landed, RestateRefused, Restated, Upper};
use crate::engine::{ConflictWindow, Green, Increment, Outcome};
use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::origin::Origin;
use crate::range::Range;
use crate::verdict::MergeConflictClass;

/// A work's ops document under construction, its bytes laid in its post-state as the deriver lays them. Each test adds
/// a path's content operations in their offsets' order, the order a canonical document holds them in.
#[derive(Clone, Debug, Default)]
pub(super) struct Work {
  pub(super) doc: OpsDoc,
  pub(super) post: Vec<u8>,
}

impl Work {
  /// One operation, `(at, len, src)` as the ops document holds them.
  pub(super) fn op(mut self, kind: OpKind, path: &str, (at, len, src): (u64, u64, u64)) -> Work {
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

  /// A content operation adding `bytes` at `at`.
  pub(super) fn adds(mut self, kind: OpKind, path: &str, at: u64, bytes: &[u8]) -> Work {
    let src = self.post.len() as u64;
    self.post.extend_from_slice(bytes);
    self.op(kind, path, (at, bytes.len() as u64, src))
  }

  pub(super) fn overwrite(self, path: &str, at: u64, bytes: &[u8]) -> Work {
    self.adds(OpKind::Overwrite, path, at, bytes)
  }

  pub(super) fn insert(self, path: &str, at: u64, bytes: &[u8]) -> Work {
    self.adds(OpKind::Insert, path, at, bytes)
  }

  pub(super) fn extend(self, path: &str, at: u64, bytes: &[u8]) -> Work {
    self.adds(OpKind::Extend, path, at, bytes)
  }

  pub(super) fn delete(self, path: &str, at: u64, len: u64) -> Work {
    self.op(OpKind::Delete, path, (at, len, u64::MAX))
  }

  pub(super) fn truncate(self, path: &str, at: u64, len: u64) -> Work {
    self.op(OpKind::Truncate, path, (at, len, u64::MAX))
  }

  pub(super) fn create(self, path: &str, bytes: &[u8]) -> Work {
    self
      .op(OpKind::Create, path, (0, 0, u64::MAX))
      .adds(OpKind::Insert, path, 0, bytes)
  }

  pub(super) fn unlink(self, path: &str) -> Work {
    self.op(OpKind::Unlink, path, (0, 0, u64::MAX))
  }

  pub(super) fn rename(mut self, from: &str, to: &str) -> Work {
    let source = self.doc.paths.intern(from);
    self.op(OpKind::Rename, to, (0, 0, source))
  }

  pub(super) fn mode(self, path: &str, mode: u32) -> Work {
    self.op(OpKind::SetMode, path, (0, u64::from(mode), u64::MAX))
  }

  pub(super) fn xattr(mut self, path: &str, name: &str, value: &[u8]) -> Work {
    let name = self.doc.paths.intern(name);
    let src = self.post.len() as u64;
    self.post.extend_from_slice(value);
    self.op(OpKind::SetXattr, path, (name, value.len() as u64, src))
  }

  pub(super) fn copy(mut self, from: &str, to: &str, version: u64) -> Work {
    let source = self.doc.paths.intern(from);
    self.op(OpKind::Copy, to, (version, 0, source))
  }

  /// The work as an increment based on `base`, its identity `id`.
  pub(super) fn increment(self, id: u64, base: u64) -> Increment {
    Increment {
      id: identity(id),
      base,
      doc: self.doc,
      post_state: self.post,
      evidence: Vec::new(),
      named: Default::default(),
    }
  }

  /// The work as an upper, its identity `id`.
  pub(super) fn upper(self, id: u64) -> Upper {
    Upper {
      id: identity(id),
      doc: self.doc,
      post_state: self.post,
      evidence: Vec::new(),
    }
  }
}

/// A work's identity from a number.
pub(super) fn identity(id: u64) -> [u8; 32] {
  let mut identity = [0u8; 32];
  identity[..8].copy_from_slice(&id.to_le_bytes());
  identity
}

/// A green whose origin holds `files`.
pub(super) fn green_of(files: &[(&str, &[u8])]) -> Green {
  let mut origin = Origin::default();
  for (path, bytes) in files {
    origin.files.push(((*path).to_owned(), bytes.to_vec()));
  }
  Green::with_origin(origin)
}

/// `path`'s bytes at the head; none when it holds no file.
pub(super) fn text(green: &Green, path: &str) -> Option<Vec<u8>> {
  green.content(path).map(<[u8]>::to_vec)
}

/// The version each work of a stack landed as; none for one that did not land.
pub(super) fn versions(landed: &[Landed]) -> Vec<Option<u64>> {
  landed
    .iter()
    .map(|outcome| match outcome {
      Landed::Accepted { version, .. } => Some(*version),
      _ => None,
    })
    .collect()
}

/// The work an upper of a stack was committed as.
pub(super) fn restated(landed: &Landed) -> Option<&Increment> {
  match landed {
    Landed::Accepted { restated, .. } => restated.as_ref(),
    _ => None,
  }
}

/// Submits `work` based at `base` as increment `id`; the version it was accepted as.
fn land(green: &mut Green, work: Work, (id, base): (u64, u64)) -> Option<u64> {
  match green.submit(&work.increment(id, base)) {
    Outcome::Accepted { version } => Some(version),
    _ => None,
  }
}

/// A whole-path window, as the verdict names a namespace conflict.
fn window(path: &str, class: MergeConflictClass) -> ConflictWindow {
  ConflictWindow {
    path: path.to_owned(),
    range: Range::new(0, 0),
    class,
  }
}

/// A-659, §4.17.1 "Chains of works": a stack lands in order. Do: land a stack of three works on the head of a green
/// holding `0123456789`: an overwrite, then an insert in its post-state, then a delete in that one's. Expect each landed
/// as the next version, each upper restated over the version below it with its operations where it declared them
/// (nothing landed in between), and the file as the three applied one after another: `AB45xy6789`.
#[test]
fn a_stack_on_the_head_lands_its_works_in_order_as_they_apply_one_after_another() {
  let mut green = green_of(&[("f", b"0123456789")]);
  let bottom = Work::default().overwrite("f", 2, b"AB").increment(1, 0);
  let middle = Work::default().insert("f", 6, b"xy").upper(2);
  let top = Work::default().delete("f", 0, 2).upper(3);
  let landed = green.land_stack(&bottom, &[middle.clone(), top.clone()]);
  assert_eq!(versions(&landed), [Some(1), Some(2), Some(3)]);
  assert_eq!(text(&green, "f").as_deref(), Some(&b"AB45xy6789"[..]));
  let (over_bottom, over_middle) = (restated(&landed[1]), restated(&landed[2]));
  assert_eq!(
    over_bottom.map(|work| (work.base, &work.doc)),
    Some((1, &middle.doc))
  );
  assert_eq!(
    over_middle.map(|work| (work.base, &work.doc)),
    Some((2, &top.doc))
  );
}

/// The stack of the remap cases: on `0123456789`, a bottom based at version 0 overwriting `[6, 8)` with `XY`, and an
/// upper overwriting `[7, 9)` of the bottom's post-state `012345XY89` with `QR`.
fn remapped_stack() -> (Increment, Upper) {
  let bottom = Work::default().overwrite("f", 6, b"XY").increment(1, 0);
  let upper = Work::default().overwrite("f", 7, b"QR").upper(2);
  (bottom, upper)
}

/// A green holding `0123456789` where a work inserted `abc` at 1 as version 1.
fn green_with_an_insert() -> Green {
  let mut green = green_of(&[("f", b"0123456789")]);
  assert_eq!(
    land(&mut green, Work::default().insert("f", 1, b"abc"), (9, 0)),
    Some(1)
  );
  green
}

/// A-659, §4.17.1 "Works over works": "When the lower one was mapped because other works landed first, the upper one is
/// mapped through the green's deltas except the lower work's own". Do: after a work inserted `abc` at 1, land the remap
/// stack, based before it. Expect the bottom as version 2 mapped past the insert, the upper restated over version 2 with
/// its overwrite moved by the insert's 3 bytes to `[10, 12)`, and the file `0abc12345XQR9`.
#[test]
fn an_upper_is_mapped_through_what_its_lower_was_mapped_through() {
  let mut green = green_with_an_insert();
  let (bottom, upper) = remapped_stack();
  let landed = green.land_stack(&bottom, &[upper]);
  assert_eq!(versions(&landed), [Some(2), Some(3)]);
  let ops = restated(&landed[1]).map(|work| (work.base, work.doc.ops.clone()));
  let moved = Op {
    kind: OpKind::Overwrite,
    flags: 0,
    path: 0,
    at: 10,
    len: 2,
    src: 0,
  };
  assert_eq!(ops, Some((2, vec![moved])));
  assert_eq!(text(&green, "f").as_deref(), Some(&b"0abc12345XQR9"[..]));
}

/// A-659: an upper meets a change its lower never saw. Do: after a work overwrote `[2, 4)` of `0123456789` with `ab`,
/// land a stack based before it: a bottom overwriting `[8, 10)` (disjoint), an upper overwriting `[3, 5)` of the bottom's
/// post-state with `pq`, and a third work over that one. Expect the bottom as version 2, the upper an overlap on f at
/// `[3, 5)` with nothing of it applied, the third never judged as the upper below it (1) did not land, and the head 2.
#[test]
fn an_upper_meeting_a_change_its_lower_never_saw_conflicts_and_leaves_the_rest_unjudged() {
  let mut green = green_of(&[("f", b"0123456789")]);
  assert_eq!(
    land(&mut green, Work::default().overwrite("f", 2, b"ab"), (9, 0)),
    Some(1)
  );
  let bottom = Work::default().overwrite("f", 8, b"YZ").increment(1, 0);
  let upper = Work::default().overwrite("f", 3, b"pq").upper(2);
  let above = Work::default().insert("f", 0, b"!").upper(3);
  let landed = green.land_stack(&bottom, &[upper, above]);
  let overlap = ConflictWindow {
    path: "f".to_owned(),
    range: Range::new(3, 2),
    class: MergeConflictClass::Overlap,
  };
  assert_eq!(versions(&landed), [Some(2), None, None]);
  assert_eq!(
    landed[1],
    Landed::Conflict {
      windows: vec![overlap]
    }
  );
  assert_eq!(landed[2], Landed::LowerRefused { lower: 1 });
  assert_eq!(green.head(), 2);
  assert_eq!(text(&green, "f").as_deref(), Some(&b"01ab4567YZ"[..]));
}

/// A-659: "If the lower is refused ... the upper is ... refused, typed". Do: after a work overwrote `[2, 4)`, land a
/// stack whose bottom overwrites `[3, 5)` from before it, with two uppers. Expect the bottom an overlap, both uppers never
/// judged as the bottom (0) did not land, and nothing committed.
#[test]
fn a_refused_bottom_leaves_every_upper_unjudged() {
  let mut green = green_of(&[("f", b"0123456789")]);
  assert_eq!(
    land(&mut green, Work::default().overwrite("f", 2, b"ab"), (9, 0)),
    Some(1)
  );
  let bottom = Work::default().overwrite("f", 3, b"cd").increment(1, 0);
  let first = Work::default().insert("f", 0, b"!").upper(2);
  let second = Work::default().insert("f", 0, b"?").upper(3);
  let landed = green.land_stack(&bottom, &[first, second]);
  assert!(matches!(landed[0], Landed::Conflict { .. }), "{landed:?}");
  assert_eq!(landed[1], Landed::LowerRefused { lower: 0 });
  assert_eq!(landed[2], Landed::LowerRefused { lower: 0 });
  assert_eq!(green.head(), 1);
}

/// A-659: landing is idempotent. Do: land the remap stack twice. Expect the second landing answered the same versions,
/// nothing applied anew, and the head and the file as the first left them.
#[test]
fn a_stack_landed_again_is_answered_each_work_s_version_and_commits_nothing() {
  let mut green = green_with_an_insert();
  let (bottom, upper) = remapped_stack();
  let first = green.land_stack(&bottom, std::slice::from_ref(&upper));
  let again = green.land_stack(&bottom, &[upper]);
  assert_eq!(versions(&first), [Some(2), Some(3)]);
  assert_eq!(versions(&again), [Some(2), Some(3)]);
  assert!(
    again
      .iter()
      .all(|outcome| matches!(outcome, Landed::Accepted { applied: None, .. })),
    "{again:?}"
  );
  assert_eq!(green.head(), 3);
  assert_eq!(text(&green, "f").as_deref(), Some(&b"0abc12345XQR9"[..]));
}

/// A-659: an upper submitted after its lower landed. Do: land the remap stack's bottom alone (version 2, mapped past
/// the insert), then a work based on version 2 overwriting `[0, 1)` with `Z` (version 3), then the stack with its upper.
/// Expect the bottom answered version 2 as accepted before, the upper restated over version 2 (its overwrite at
/// `[10, 12)`) and judged by the verdict past version 3's change: version 4, `Zabc12345XQR9`.
#[test]
fn an_upper_landed_after_its_lower_is_restated_over_the_version_its_lower_landed_as() {
  let mut green = green_with_an_insert();
  let (bottom, upper) = remapped_stack();
  assert_eq!(versions(&green.land_stack(&bottom, &[])), [Some(2)]);
  assert_eq!(
    land(&mut green, Work::default().overwrite("f", 0, b"Z"), (8, 2)),
    Some(3)
  );
  let landed = green.land_stack(&bottom, &[upper]);
  assert!(
    matches!(
      landed[0],
      Landed::Accepted {
        version: 2,
        applied: None,
        ..
      }
    ),
    "{landed:?}"
  );
  assert_eq!(versions(&landed), [Some(2), Some(4)]);
  let base_and_at = restated(&landed[1]).map(|work| (work.base, work.doc.ops[0].at));
  assert_eq!(base_and_at, Some((2, 10)));
  assert_eq!(text(&green, "f").as_deref(), Some(&b"Zabc12345XQR9"[..]));
}

/// A-659 with A-547: an upper follows a file its lower renamed and one it made. Do: after a work inserted `abc` at 1 of
/// f, land a stack based before it whose bottom renames f to g (the rename carries the insert) and makes h, and whose
/// upper overwrites `[7, 9)` of g and appends `!` to h. Expect the upper's overwrite moved by the carried insert to
/// `[10, 12)` of g, h made then appended, and f gone.
#[test]
fn an_upper_follows_a_file_its_lower_renamed_and_one_it_made() {
  let mut green = green_with_an_insert();
  let bottom = Work::default()
    .rename("f", "g")
    .create("h", b"hh")
    .increment(1, 0);
  let upper = Work::default()
    .overwrite("g", 7, b"QR")
    .extend("h", 2, b"!")
    .upper(2);
  let landed = green.land_stack(&bottom, &[upper]);
  assert_eq!(versions(&landed), [Some(2), Some(3)]);
  assert_eq!(text(&green, "g").as_deref(), Some(&b"0abc123456QR9"[..]));
  assert_eq!(text(&green, "h").as_deref(), Some(&b"hh!"[..]));
  assert_eq!(text(&green, "f"), None);
}

/// The upper's answer over a bottom that touches neither f nor g, after a work based at version 0 set f's mode to 0o600
/// and g's attribute `user.a` to `1` (version 1): the bottom, based at 0 too, edits h.
fn upper_over_metadata_changes(upper: Work) -> Landed {
  let mut green = green_of(&[("f", b"0123456789"), ("g", b"ggg"), ("h", b"hhh")]);
  let change = Work::default().mode("f", 0o600).xattr("g", "user.a", b"1");
  assert_eq!(land(&mut green, change, (9, 0)), Some(1));
  let bottom = Work::default().overwrite("h", 0, b"H").increment(1, 0);
  let mut landed = green.land_stack(&bottom, &[upper.upper(2)]);
  assert_eq!(versions(&landed).first(), Some(&Some(2)));
  landed.remove(1)
}

/// A-659, §4.16 "The verdict" (A-643): an upper's removals and metadata are judged against what its lower was mapped
/// through, as the verdict judges a work based before those changes. Do: over the metadata changes, land uppers that
/// unlink f, set its mode to 0o700, set it to 0o600, overwrite its bytes, set g's `user.b`, and set g's `user.a` to `3`.
/// Expect delete against modify, meta against meta, accepted (identical), accepted (another dimension), accepted
/// (another attribute), and meta against meta.
#[test]
fn an_upper_s_removals_and_metadata_are_judged_against_what_its_lower_was_mapped_through() {
  let unlink = upper_over_metadata_changes(Work::default().unlink("f"));
  let other_mode = upper_over_metadata_changes(Work::default().mode("f", 0o700));
  let same_mode = upper_over_metadata_changes(Work::default().mode("f", 0o600));
  let bytes = upper_over_metadata_changes(Work::default().overwrite("f", 0, b"XY"));
  let other_name = upper_over_metadata_changes(Work::default().xattr("g", "user.b", b"2"));
  let same_name = upper_over_metadata_changes(Work::default().xattr("g", "user.a", b"3"));
  let conflict = |path, class| Landed::Conflict {
    windows: vec![window(path, class)],
  };
  assert_eq!(unlink, conflict("f", MergeConflictClass::DeleteModify));
  assert_eq!(other_mode, conflict("f", MergeConflictClass::MetaMeta));
  assert_eq!(same_name, conflict("g", MergeConflictClass::MetaMeta));
  let accepted = [same_mode, bytes, other_name];
  assert!(
    accepted
      .iter()
      .all(|outcome| matches!(outcome, Landed::Accepted { version: 3, .. })),
    "{accepted:?}"
  );
}

/// A-659, "What a lower absorbed": a change the lower made too is in the upper's base. Do: after a work overwrote
/// `[2, 4)` of `0123456789` with `ab`, land a stack based before it whose bottom makes the same overwrite and one at
/// `[8, 10)`, and whose upper overwrites `[2, 4)` with `cd`. Expect both landed: the upper's base held `ab` there as the
/// green did, so it meets no change: `01cd4567YZ`.
#[test]
fn an_upper_over_a_change_its_lower_made_too_meets_nothing_there() {
  let mut green = green_of(&[("f", b"0123456789")]);
  assert_eq!(
    land(&mut green, Work::default().overwrite("f", 2, b"ab"), (9, 0)),
    Some(1)
  );
  let bottom = Work::default()
    .overwrite("f", 2, b"ab")
    .overwrite("f", 8, b"YZ")
    .increment(1, 0);
  let upper = Work::default().overwrite("f", 2, b"cd").upper(2);
  let landed = green.land_stack(&bottom, &[upper]);
  assert_eq!(versions(&landed), [Some(2), Some(3)]);
  assert_eq!(text(&green, "f").as_deref(), Some(&b"01cd4567YZ"[..]));
}

/// A-659, "What a lower absorbed": a change of another shape a lower met moves by no position mapping. Do: after a work
/// inserted `zz` at 3, land a stack based before it whose bottom inserts `zz` at 3 too (the verdict accepts it whole as
/// identical) and whose upper overwrites `[9, 11)` of the bottom's post-state. Expect the upper an overlap on f at
/// `[9, 11)`, never a guess.
#[test]
fn an_upper_over_a_file_its_lower_met_in_another_shape_conflicts() {
  let mut green = green_of(&[("f", b"0123456789")]);
  assert_eq!(
    land(&mut green, Work::default().insert("f", 3, b"zz"), (9, 0)),
    Some(1)
  );
  let bottom = Work::default().insert("f", 3, b"zz").increment(1, 0);
  let upper = Work::default().overwrite("f", 9, b"QR").upper(2);
  let landed = green.land_stack(&bottom, &[upper]);
  let overlap = ConflictWindow {
    path: "f".to_owned(),
    range: Range::new(9, 2),
    class: MergeConflictClass::Overlap,
  };
  assert_eq!(versions(&landed), [Some(2), None]);
  assert_eq!(
    landed[1],
    Landed::Conflict {
      windows: vec![overlap]
    }
  );
}

/// A-659 with A-612: an upper's truncate removes only the bytes it saw. Do: after a work appended `AB` to
/// `0123456789`, land a stack based before it whose bottom overwrites `[0, 1)` and whose upper truncates its base to 7
/// bytes. Expect the upper restated as the delete of `[7, 10)`, the append kept: `Z123456AB`.
#[test]
fn an_upper_s_truncate_keeps_what_was_appended_since_its_lower_s_base() {
  let mut green = green_of(&[("f", b"0123456789")]);
  assert_eq!(
    land(&mut green, Work::default().extend("f", 10, b"AB"), (9, 0)),
    Some(1)
  );
  let bottom = Work::default().overwrite("f", 0, b"Z").increment(1, 0);
  let upper = Work::default().truncate("f", 7, 3).upper(2);
  let landed = green.land_stack(&bottom, &[upper]);
  assert_eq!(versions(&landed), [Some(2), Some(3)]);
  let kind = restated(&landed[1]).map(|work| work.doc.ops[0].kind);
  assert_eq!(kind, Some(OpKind::Delete));
  assert_eq!(text(&green, "f").as_deref(), Some(&b"Z123456AB"[..]));
}

/// A-659, "What it refuses": typed refusals. Do: restate an upper over a bottom that never landed; over a bottom and a
/// middle where only the bottom landed; land an upper holding a copy; and land an upper after the history its bottom was
/// mapped through was folded. Expect not landed (0), not landed (1), a copy at its destination, and folded.
#[test]
fn an_upper_is_refused_typed_where_it_cannot_be_restated() {
  let mut green = green_with_an_insert();
  let (bottom, upper) = remapped_stack();
  let middle = Work::default().insert("f", 0, b"!").upper(3);
  let never = green.restate((&bottom, &[]), &upper);
  assert_eq!(
    never,
    Restated::Refused(RestateRefused::NotLanded { below: 0 })
  );
  assert_eq!(versions(&green.land_stack(&bottom, &[])), [Some(2)]);
  let unlanded = green.restate((&bottom, std::slice::from_ref(&middle)), &upper);
  assert_eq!(
    unlanded,
    Restated::Refused(RestateRefused::NotLanded { below: 1 })
  );
  let copying = Work::default().copy("f", "c", 2).upper(4);
  let copied = green.restate((&bottom, &[]), &copying);
  let copy = RestateRefused::Copy {
    path: "c".to_owned(),
  };
  assert_eq!(copied, Restated::Refused(copy));
  green.fold_history_before(2);
  let folded = green.restate((&bottom, &[]), &upper);
  assert!(
    matches!(folded, Restated::Refused(RestateRefused::Folded { .. })),
    "{folded:?}"
  );
}
