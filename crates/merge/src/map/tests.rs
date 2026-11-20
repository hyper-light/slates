//! Two inserts at one point meet in both orders (A-668; D-27; §4.16 "Position mapping" and "The verdict"), worked at
//! the map and at the verdict.

use super::{Mapped, map_range};
use crate::engine::{Green, Increment, Outcome};
use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::origin::Origin;
use crate::range::Range;

/// An operation on the document's first path, its bytes at the post-state's start.
fn op(kind: OpKind, at: u64, len: u64) -> Op {
  Op {
    kind,
    flags: 0,
    path: 0,
    at,
    len,
    src: 0,
  }
}

/// A-668, D-27: a point (an insert's empty range) meets an insert at it though the delta overwrites or truncates from
/// it first, as canonical order puts them. Do: map the point 4 through a delta overwriting `[4, 6)` then inserting at
/// 4, through one truncating from 4 then inserting at 4, and through one inserting at 4 then overwriting `[4, 6)`.
/// Expect each to meet it. Do: map it through an overwrite of `[4, 6)` alone, and through one with an insert at 6 after
/// it. Expect it unmoved: an operation starting at a point lies after it.
#[test]
fn a_point_meets_an_insert_at_it_behind_an_operation_starting_there() {
  let point = Range::new(4, 0);
  let behind_overwrite = [op(OpKind::Overwrite, 4, 2), op(OpKind::Insert, 4, 1)];
  let behind_truncate = [op(OpKind::Truncate, 4, 6), op(OpKind::Insert, 4, 1)];
  let in_front = [op(OpKind::Insert, 4, 1), op(OpKind::Overwrite, 4, 2)];
  for delta in [&behind_overwrite[..], &behind_truncate[..], &in_front[..]] {
    assert_eq!(map_range(&[delta], point), Mapped::Overlaps, "{delta:?}");
  }
  let alone = [op(OpKind::Overwrite, 4, 2)];
  let insert_after = [op(OpKind::Overwrite, 4, 2), op(OpKind::Insert, 6, 1)];
  for delta in [&alone[..], &insert_after[..]] {
    assert_eq!(
      map_range(&[delta], point),
      Mapped::Shifted(point),
      "{delta:?}"
    );
  }
}

/// A work of `ops` on `f`, its added bytes `post`, as increment `id` based at version 0.
fn work(id: u8, ops: &[(OpKind, u64, &[u8], u64)]) -> Increment {
  let mut doc = OpsDoc::new();
  let path = doc.paths.intern("f");
  let mut post = Vec::new();
  for (kind, at, bytes, removed) in ops {
    let src = post.len() as u64;
    post.extend_from_slice(bytes);
    let len = if bytes.is_empty() {
      *removed
    } else {
      bytes.len() as u64
    };
    doc.ops.push(Op {
      kind: *kind,
      flags: 0,
      path,
      at: *at,
      len,
      src,
    });
  }
  let mut identity = [0u8; 32];
  identity[0] = id;
  Increment {
    id: identity,
    base: 0,
    doc,
    post_state: post,
    evidence: Vec::new(),
    named: Default::default(),
  }
}

/// What the verdict answers `second` after `first` landed on f `0123456789`, both based at version 0.
fn second_answer(first: &Increment, second: &Increment) -> Outcome {
  let mut origin = Origin::default();
  origin.files.push(("f".to_owned(), b"0123456789".to_vec()));
  let mut green = Green::with_origin(origin);
  assert_eq!(green.submit(first), Outcome::Accepted { version: 1 });
  green.submit(second)
}

/// A-668, D-27 and §4.16 "The verdict": two inserts at one point conflict in both orders, since accepting one ahead of
/// the other picks an order, an inferred merge. Do: land an insert of `X` at 4 and a work overwriting `[4, 6)` with
/// `OO` and inserting `Y` at 4, each first in turn with the other based before it; and the same with a work truncating
/// from 4 and inserting `Y` at 4. Expect the second a conflict in all four.
#[test]
fn two_inserts_at_one_point_conflict_in_both_orders() {
  let insert = work(1, &[(OpKind::Insert, 4, &b"X"[..], 0)]);
  let overwrite_then_insert = work(
    2,
    &[
      (OpKind::Overwrite, 4, &b"OO"[..], 0),
      (OpKind::Insert, 4, &b"Y"[..], 0),
    ],
  );
  let truncate_then_insert = work(
    3,
    &[
      (OpKind::Truncate, 4, &b""[..], 6),
      (OpKind::Insert, 4, &b"Y"[..], 0),
    ],
  );
  for other in [&overwrite_then_insert, &truncate_then_insert] {
    let after_insert = second_answer(&insert, other);
    let insert_after = second_answer(other, &insert);
    assert!(
      matches!(after_insert, Outcome::Conflict { .. }),
      "{after_insert:?}"
    );
    assert!(
      matches!(insert_after, Outcome::Conflict { .. }),
      "{insert_after:?}"
    );
  }
}
