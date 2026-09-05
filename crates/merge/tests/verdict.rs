//! The verdict's oracle tests (Phase 6; §4.16 "The verdict", D-27; the hecate M-matrix as range
//! cases): each case states the increment's ranges and the intervening deltas' effect ranges on
//! one path and the verdict the design's rules require. The verdict is pure, so these run on
//! every host. "Do X, expect Y" throughout.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_merge::path_verdict;
use slates_merge::range::{Range, RangeSet};
use slates_merge::verdict::{MergeConflictClass, PathVerdict, Verdict, compare_bytes, fast_path};

fn set(ranges: &[(u64, u64)]) -> RangeSet {
  RangeSet::from_ranges(ranges.iter().map(|&(s, l)| Range::new(s, l)).collect())
}

/// Disjoint ranges accept: an edit far from every intervening change applies untouched.
#[test]
fn disjoint_ranges_accept() {
  let mine = set(&[(0, 10), (100, 20)]);
  let theirs = set(&[(40, 10), (200, 5)]);
  assert_eq!(path_verdict(&mine, &theirs, None), PathVerdict::Accept);
}

/// Nothing intervening on the path accepts without any range work (the common case).
#[test]
fn no_intervening_change_accepts() {
  let mine = set(&[(0, 1000)]);
  assert_eq!(
    path_verdict(&mine, &RangeSet::new(), None),
    PathVerdict::Accept
  );
}

/// Overlapping overwrites are a conflict (the byte windows collide).
#[test]
fn overlapping_ranges_conflict() {
  let mine = set(&[(10, 20)]);
  let theirs = set(&[(20, 20)]);
  assert_eq!(
    path_verdict(&mine, &theirs, None),
    PathVerdict::Conflict(MergeConflictClass::Overlap)
  );
}

/// One range containing another is a conflict too.
#[test]
fn containment_conflicts() {
  let mine = set(&[(0, 100)]);
  let theirs = set(&[(40, 10)]);
  assert_eq!(
    path_verdict(&mine, &theirs, None),
    PathVerdict::Conflict(MergeConflictClass::Overlap)
  );
}

/// An identical span is a candidate for pass two (the same edit may have been made on both
/// sides); pass two's memcmp then decides identical or conflict.
#[test]
fn same_span_is_a_candidate_then_pass_two_decides() {
  let mine = set(&[(10, 4)]);
  let theirs = set(&[(10, 4)]);
  let PathVerdict::Candidates(spans) = path_verdict(&mine, &theirs, None) else {
    panic!("expected candidates");
  };
  assert_eq!(spans, vec![Range::new(10, 4)]);
  assert_eq!(compare_bytes(b"data", b"data"), Verdict::AcceptIdentical);
  assert_eq!(
    compare_bytes(b"data", b"diff"),
    Verdict::Conflict(MergeConflictClass::Overlap)
  );
}

/// An insert anchored strictly inside an intervening overwrite is a same-position conflict; an
/// insert at the very edge of a range does not overlap and accepts.
#[test]
fn an_insert_inside_a_change_conflicts_at_the_edge_accepts() {
  let overwrite = set(&[(10, 20)]);
  let inside = set(&[(15, 0)]); // insert at position 15, inside [10,30)
  assert_eq!(
    path_verdict(&inside, &overwrite, None),
    PathVerdict::Conflict(MergeConflictClass::SamePositionDiffering)
  );
  let at_edge = set(&[(30, 0)]); // insert at position 30, the end of [10,30)
  assert_eq!(
    path_verdict(&at_edge, &overwrite, None),
    PathVerdict::Accept
  );
}

/// Two inserts at the same point are a same-position conflict.
#[test]
fn two_inserts_at_one_point_conflict() {
  let mine = set(&[(50, 0)]);
  let theirs = set(&[(50, 0)]);
  // Same zero-length span: a candidate whose bytes pass two compares; differing inserts differ.
  let v = path_verdict(&mine, &theirs, None);
  assert!(matches!(v, PathVerdict::Candidates(_)));
  assert_eq!(
    compare_bytes(b"aaa", b"bbb"),
    Verdict::Conflict(MergeConflictClass::Overlap)
  );
}

/// A structural signal is returned directly: the range sweep cannot see a rename/create/type/
/// meta clash, so the engine passes the class in.
#[test]
fn structural_conflicts_are_returned_directly() {
  for class in [
    MergeConflictClass::RenameRename,
    MergeConflictClass::CreateCreate,
    MergeConflictClass::DeleteModify,
    MergeConflictClass::ModifyDelete,
    MergeConflictClass::TypeChanged,
    MergeConflictClass::MetaMeta,
    MergeConflictClass::AnchoredInDelete,
  ] {
    assert_eq!(
      path_verdict(&set(&[(0, 10)]), &set(&[(0, 10)]), Some(class)),
      PathVerdict::Conflict(class),
      "the structural class wins over the range sweep"
    );
  }
}

/// The whole-increment fast path: every touched path last changed at or before the base means
/// accept with no range work; a path changed after the base falls through to the sweep.
#[test]
fn the_fast_path_accepts_an_untouched_basis() {
  assert!(fast_path(&[3, 5, 5], 5), "all at or before base 5");
  assert!(!fast_path(&[3, 6], 5), "one changed after base 5");
  assert!(fast_path(&[], 0), "an increment touching nothing");
}
