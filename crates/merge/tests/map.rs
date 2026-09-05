//! Oracle and worked-case tests for position mapping (§4.16 "Position mapping"; T-6.x). A range
//! declared against a base version is mapped forward through the intervening deltas to head
//! coordinates. The property: a base range that no intervening change touched maps to the head
//! position where those exact base bytes ended up, and one that any change touched is reported
//! as overlapping (handed to the verdict).
//!
//! The oracle tracks provenance. It applies each intervening delta to a vector that records, for
//! every head byte, which base byte it came from (or that it is new); a base range maps cleanly
//! exactly when its bytes all survived and stayed contiguous, and then to the position they
//! occupy. This is computed independently of the mapper, so a wrong shift or a missed overlap
//! diverges.

use slates_merge::map::{Mapped, map_range};
use slates_merge::ops_doc::{Op, OpKind};
use slates_merge::range::Range;

use proptest::prelude::*;

/// Builds a content op with a path index and flags of zero (position mapping ignores both).
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

/// A raw single-op delta the strategy generates; interpreted against the running length so every
/// op is in bounds.
#[derive(Clone, Copy, Debug)]
struct Raw {
  kind: u8,
  a: u64,
  b: u64,
}

/// Shape: the largest run a generated insert adds, kept small so a case explores many deltas
/// meeting a range rather than a few huge shifts.
const MAX_INSERT: u64 = 12;

/// A base byte's origin marker is its index; a new byte's is this sentinel.
const NEW: i64 = -1;

/// Applies the raw single-op deltas to a provenance vector (head byte -> base index or `NEW`)
/// and returns both the provenance after all deltas and the ops in their version coordinates.
fn simulate(base_len: u64, raw_ops: &[Raw]) -> (Vec<i64>, Vec<Vec<Op>>) {
  let mut provenance: Vec<i64> = (0..base_len)
    .map(|i| i64::try_from(i).unwrap_or(NEW))
    .collect();
  let mut deltas = Vec::new();
  for raw in raw_ops {
    let current = provenance.len() as u64;
    match raw.kind % 3 {
      0 => {
        // Overwrite in place.
        if current == 0 {
          continue;
        }
        let at = raw.a % current;
        let len = 1 + raw.b % (current - at);
        for offset in 0..len {
          let index = usize::try_from(at + offset).unwrap_or(0);
          provenance[index] = NEW;
        }
        deltas.push(vec![op(OpKind::Overwrite, at, len)]);
      }
      1 => {
        // Insert new bytes.
        let at = raw.a % (current + 1);
        let len = 1 + raw.b % MAX_INSERT;
        let start = usize::try_from(at).unwrap_or(0);
        let tail = provenance.split_off(start);
        provenance.extend(std::iter::repeat_n(NEW, usize::try_from(len).unwrap_or(0)));
        provenance.extend(tail);
        deltas.push(vec![op(OpKind::Insert, at, len)]);
      }
      _ => {
        // Delete a range.
        if current == 0 {
          continue;
        }
        let at = raw.a % current;
        let len = 1 + raw.b % (current - at);
        let start = usize::try_from(at).unwrap_or(0);
        let end = usize::try_from(at + len).unwrap_or(start);
        provenance.drain(start..end);
        deltas.push(vec![op(OpKind::Delete, at, len)]);
      }
    }
  }
  (provenance, deltas)
}

/// Classifies a base range from the provenance: `Some(shifted)` when every base byte survived and
/// stayed contiguous (mapping cleanly to where they are), `None` when any is gone or the run was
/// split by an intervening insert.
fn expected(provenance: &[i64], range: Range) -> Option<Range> {
  if range.len == 0 {
    return None;
  }
  let mut head_positions = Vec::new();
  for base_index in range.start..range.end() {
    let wanted = i64::try_from(base_index).unwrap_or(NEW);
    let pos = provenance.iter().position(|&p| p == wanted)?;
    head_positions.push(u64::try_from(pos).unwrap_or(0));
  }
  let first = *head_positions.first()?;
  let contiguous = head_positions
    .iter()
    .enumerate()
    .all(|(offset, &pos)| pos == first + u64::try_from(offset).unwrap_or(0));
  if contiguous {
    Some(Range::new(first, range.len))
  } else {
    None
  }
}

/// Passes the deltas as the slice-of-slices the mapper takes.
fn refs(deltas: &[Vec<Op>]) -> Vec<&[Op]> {
  deltas.iter().map(Vec::as_slice).collect()
}

/// A range before an intervening insert is shifted right by the insert's length.
#[test]
fn an_insert_before_a_range_shifts_it() {
  let deltas = vec![vec![op(OpKind::Insert, 0, 5)]];
  let got = map_range(&refs(&deltas), Range::new(10, 4));
  assert_eq!(got, Mapped::Shifted(Range::new(15, 4)));
}

/// A range after an intervening insert is unaffected.
#[test]
fn an_insert_after_a_range_leaves_it() {
  let deltas = vec![vec![op(OpKind::Insert, 20, 5)]];
  let got = map_range(&refs(&deltas), Range::new(10, 4));
  assert_eq!(got, Mapped::Shifted(Range::new(10, 4)));
}

/// A delete before a range shifts it left by the deleted length.
#[test]
fn a_delete_before_a_range_shifts_it_left() {
  let deltas = vec![vec![op(OpKind::Delete, 0, 3)]];
  let got = map_range(&refs(&deltas), Range::new(10, 4));
  assert_eq!(got, Mapped::Shifted(Range::new(7, 4)));
}

/// An intervening overwrite that hits the range overlaps it (the verdict then decides).
#[test]
fn an_overwrite_hitting_the_range_overlaps() {
  let deltas = vec![vec![op(OpKind::Overwrite, 11, 2)]];
  let got = map_range(&refs(&deltas), Range::new(10, 4));
  assert_eq!(got, Mapped::Overlaps);
}

/// An insert exactly at the range's start is an edge, not an overlap: the range shifts right and
/// stays clean (the edge rule).
#[test]
fn an_insert_at_the_range_edge_is_clean() {
  let deltas = vec![vec![op(OpKind::Insert, 10, 5)]];
  let got = map_range(&refs(&deltas), Range::new(10, 4));
  assert_eq!(got, Mapped::Shifted(Range::new(15, 4)));
}

/// Maps compose: an insert before, then a delete before, shift the range by the sum.
#[test]
fn maps_compose_across_deltas() {
  let deltas = vec![
    vec![op(OpKind::Insert, 0, 6)], // range 10..14 -> 16..20
    vec![op(OpKind::Delete, 0, 4)], // 16..20 -> 12..16
  ];
  let got = map_range(&refs(&deltas), Range::new(10, 4));
  assert_eq!(got, Mapped::Shifted(Range::new(12, 4)));
}

/// A change in a later delta that meets the already-shifted range overlaps.
#[test]
fn a_later_delta_meeting_the_shifted_range_overlaps() {
  let deltas = vec![
    vec![op(OpKind::Insert, 0, 6)],     // range 10..14 -> 16..20
    vec![op(OpKind::Overwrite, 17, 2)], // hits 16..20
  ];
  let got = map_range(&refs(&deltas), Range::new(10, 4));
  assert_eq!(got, Mapped::Overlaps);
}

proptest! {
  /// T-6.4 (the position-mapping oracle): for any base length, any sequence of intervening
  /// single-op deltas, and any base range, the mapper agrees with the provenance: a range whose
  /// bytes all survived contiguously maps to their head position, and any other range overlaps.
  #[test]
  fn the_map_agrees_with_provenance(
    base_len in 1u64..48,
    raw in proptest::collection::vec(
      (any::<u8>(), 0u64..64, 0u64..64).prop_map(|(kind, a, b)| Raw { kind, a, b }),
      0..16,
    ),
    start in 0u64..48,
    len in 1u64..12,
  ) {
    prop_assume!(start + len <= base_len);
    let (provenance, deltas) = simulate(base_len, &raw);
    let range = Range::new(start, len);
    let got = map_range(&refs(&deltas), range);
    match expected(&provenance, range) {
      Some(shifted) => prop_assert_eq!(got, Mapped::Shifted(shifted)),
      None => prop_assert_eq!(got, Mapped::Overlaps),
    }
  }

  /// T-6.5 (determinism): the same deltas and range map the same way every time.
  #[test]
  fn mapping_is_deterministic(
    base_len in 1u64..48,
    raw in proptest::collection::vec(
      (any::<u8>(), 0u64..64, 0u64..64).prop_map(|(kind, a, b)| Raw { kind, a, b }),
      0..16,
    ),
    start in 0u64..48,
    len in 1u64..12,
  ) {
    prop_assume!(start + len <= base_len);
    let (_prov, deltas) = simulate(base_len, &raw);
    let range = Range::new(start, len);
    let first = map_range(&refs(&deltas), range);
    let second = map_range(&refs(&deltas), range);
    prop_assert_eq!(first, second);
  }
}
