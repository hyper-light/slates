//! The byte splice's buffer (`super::apply`; goal 93, P7 finding (2): main 01a20182's routing of 2026-11-22 07:38, the
//! engine's `push_slice` the import's largest churner after A-862): what it makes, sized once.

use crate::ops_doc::{Op, OpKind};

/// Shape: the base's length and the inserted run's: the run first, longer than the base, so a buffer grown a piece at a
/// time doubles past the whole.
const BASE: usize = 1_000;
/// Shape: see [`BASE`].
const RUN: usize = 3_000;

/// An op of `kind` at `at`, `len` bytes, its added bytes at `src` in the post-state.
fn op(kind: OpKind, at: u64, len: u64, src: u64) -> Op {
  Op {
    kind,
    flags: 0,
    path: 0,
    at,
    len,
    src,
  }
}

/// Goal 93, P7 finding (2). Do: splice a base of [`BASE`] bytes under an insert of [`RUN`] bytes at its start, then a
/// base whose ops run past it and past the post-state (an overwrite reaching beyond the post-state's end, a delete past
/// the base's end: an ops document is not checked against its base when decoded). Expect: each made as before, its
/// buffer sized once at exactly what it holds. Failing first: the buffer grew a piece at a time, 6,000 bytes held for
/// 4,000, about twice every file an import's increments make.
#[test]
fn a_splice_makes_its_bytes_in_one_buffer_of_their_length() {
  let base = vec![b'b'; BASE];
  let post = vec![b'p'; RUN];
  let inserted = super::apply_for_tests(&base, &[op(OpKind::Insert, 0, RUN as u64, 0)], &post);
  assert_eq!(inserted.len(), RUN + BASE);
  assert!(inserted[..RUN].iter().all(|byte| *byte == b'p'));
  assert!(inserted[RUN..].iter().all(|byte| *byte == b'b'));
  assert_eq!(inserted.capacity(), inserted.len(), "the insert's buffer");
  let clamped = super::apply_for_tests(
    &base,
    &[
      op(OpKind::Overwrite, 10, 50, RUN as u64 - 20),
      op(OpKind::Delete, 900, 500, 0),
    ],
    &post,
  );
  assert_eq!(clamped.len(), 10 + 20 + (900 - 60));
  assert_eq!(
    clamped.capacity(),
    clamped.len(),
    "the clamped splice's buffer"
  );
}
