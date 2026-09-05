//! The generated operations the model-based and differential suites share: one `Step` type and
//! its proptest strategies, so every harness runs the same histories (Phase 1 task 8).

// proptest's strategy macros expand to `Arc`-carrying unions; the test harness exception of D-8
// covers it, here once for every test binary that includes this module.
#![allow(clippy::disallowed_types)]

use proptest::prelude::*;

/// One generated operation. Paths are component lists from the root; `pick` fields choose a
/// file among those that exist (by index, wrapping), so a step never names an inode directly.
#[derive(Clone, Debug)]
pub(crate) enum Step {
  Create(Vec<String>, String),
  Mkdir(Vec<String>, String),
  Symlink(Vec<String>, String),
  Unlink(Vec<String>, String),
  Rmdir(Vec<String>, String),
  Rename(Vec<String>, String, Vec<String>, String),
  Link(Vec<String>, String, u8),
  Write(u8, u16, Vec<u8>),
  Truncate(u8, u16),
  /// An SDK edit on a picked file: at an offset (clamped to the size by the harness), delete
  /// up to this many bytes and insert these.
  Edit(u8, u16, u8, Vec<u8>),
  Snapshot,
}

/// Six names, two of them case variants of others, so folding policies are exercised.
pub(crate) fn name() -> impl Strategy<Value = String> {
  prop_oneof![
    Just("a"),
    Just("b"),
    Just("C"),
    Just("d"),
    Just("e"),
    Just("A")
  ]
  .prop_map(str::to_owned)
}

/// Up to two components deep.
pub(crate) fn path() -> impl Strategy<Value = Vec<String>> {
  prop::collection::vec(name(), 0..3)
}

/// Shape: writes of up to forty bytes at offsets up to `u16::MAX`, so files cross the inline
/// threshold and a chunk window without growing past what a test can compare byte for byte.
pub(crate) fn step() -> impl Strategy<Value = Step> {
  prop_oneof![
    (path(), name()).prop_map(|(p, n)| Step::Create(p, n)),
    (path(), name()).prop_map(|(p, n)| Step::Mkdir(p, n)),
    (path(), name()).prop_map(|(p, n)| Step::Symlink(p, n)),
    (path(), name()).prop_map(|(p, n)| Step::Unlink(p, n)),
    (path(), name()).prop_map(|(p, n)| Step::Rmdir(p, n)),
    (path(), name(), path(), name()).prop_map(|(a, b, c, d)| Step::Rename(a, b, c, d)),
    (path(), name(), any::<u8>()).prop_map(|(p, n, i)| Step::Link(p, n, i)),
    (
      any::<u8>(),
      any::<u16>(),
      prop::collection::vec(any::<u8>(), 0..40)
    )
      .prop_map(|(i, o, b)| Step::Write(i, o, b)),
    (any::<u8>(), any::<u16>()).prop_map(|(i, l)| Step::Truncate(i, l)),
    (
      any::<u8>(),
      any::<u16>(),
      any::<u8>(),
      prop::collection::vec(any::<u8>(), 0..24)
    )
      .prop_map(|(i, at, del, b)| Step::Edit(i, at, del, b)),
    Just(Step::Snapshot),
  ]
}
