//! The mergeinfo module's tests (A-605, §4.17.4 item 8 and piece 9). Subversion's own vectors from
//! `subversion/tests/libsvn_subr/mergeinfo-test.c` (`1.14.x` at `d434dc14`, sha256 prefix `28381a00f69941fd`), each
//! table cited by the test function it comes from; every refusal of the parser by kind; hostile values; the
//! per-revision model of range lists (the model Subversion's own random tests use, `rl_array_t`) wherever Subversion's
//! algebra is exact; the queries case by case; and the oracle: a naive filesystem keeping every revision's whole tree,
//! which the index-backed queries must equal on generated histories.

use super::*;
use crate::index::{Action, ChangedPath, CopySource, RevisionRecord};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use std::cell::RefCell;

/// A range of Subversion's tables, `{start, end, inheritable}`.
fn range(start: Revision, end: Revision, inheritable: bool) -> MergeRange {
  MergeRange::new(start, end, inheritable).unwrap()
}

/// Ranges from `(start, end, inheritable)` triples.
fn ranges(triples: &[(Revision, Revision, bool)]) -> Vec<MergeRange> {
  triples
    .iter()
    .map(|&(start, end, inheritable)| range(start, end, inheritable))
    .collect()
}

/// A range list written as Subversion's tables write one, canonical; `""` is the empty list.
fn list(text: &str) -> RangeList {
  if text.is_empty() {
    return RangeList::new();
  }
  let mergeinfo = Mergeinfo::parse(format!("/x:{text}").as_bytes()).unwrap();
  mergeinfo.get(&path("/x")).unwrap().clone()
}

/// A source path.
fn path(text: &str) -> SourcePath {
  SourcePath::canonical(text.as_bytes())
}

/// A value that parses.
fn parsed(text: &str) -> Mergeinfo {
  Mergeinfo::parse(text.as_bytes()).unwrap()
}

/// The ranges `mergeinfo` holds for `source`.
fn ranges_of(mergeinfo: &Mergeinfo, source: &str) -> Vec<MergeRange> {
  mergeinfo.get(&path(source)).unwrap().ranges().to_vec()
}

// ---------------------------------------------------------------------------------------------------------------------
// Subversion's vectors: parsing and writing.

/// A-605: `test_parse_single_line_mergeinfo` (`mergeinfo_vals`, `mergeinfo_paths`, `mergeinfo_ranges`): Subversion's 25 valid
/// values each read to its one path and its ranges: unordered and overlapping ranges sorted and combined, colons in
/// paths, relative and non-canonical paths made canonical.
#[test]
fn subversions_valid_values_read_to_its_paths_and_ranges() {
  let cases: [(&str, &str, &[(Revision, Revision, bool)]); 25] = [
    ("/trunk:1", "/trunk", &[(0, 1, true)]),
    ("/trunk/foo:1-6", "/trunk/foo", &[(0, 6, true)]),
    ("/trunk: 5,7-9,10,11,13,14", "/trunk", &[(4, 5, true), (6, 11, true), (12, 14, true)]),
    ("/trunk: 3-10,11*,13,14", "/trunk", &[(2, 10, true), (10, 11, false), (12, 14, true)]),
    ("/branch: 1,2-18*,33*", "/branch", &[(0, 1, true), (1, 18, false), (32, 33, false)]),
    ("patch-common::netasq-bpf.c:25381", "/patch-common::netasq-bpf.c", &[(25380, 25381, true)]),
    ("patch-common_netasq-bpf.c::25381", "/patch-common_netasq-bpf.c:", &[(25380, 25381, true)]),
    (":patch:common:netasq:bpf.c:25381", "/:patch:common:netasq:bpf.c", &[(25380, 25381, true)]),
    (
      "/trunk:3-6,15,18,9,22",
      "/trunk",
      &[(2, 6, true), (8, 9, true), (14, 15, true), (17, 18, true), (21, 22, true)],
    ),
    ("/trunk:5,3", "/trunk", &[(2, 3, true), (4, 5, true)]),
    (
      "/trunk:3-6*,15*,18*,9,22*",
      "/trunk",
      &[(2, 6, false), (8, 9, true), (14, 15, false), (17, 18, false), (21, 22, false)],
    ),
    ("/trunk:5,3*", "/trunk", &[(2, 3, false), (4, 5, true)]),
    ("/trunk:100,3-7,50,99,1-2", "/trunk", &[(0, 7, true), (49, 50, true), (98, 100, true)]),
    ("/gunther_branch:5-10,7-12", "/gunther_branch", &[(4, 12, true)]),
    ("/gunther_branch:5-10*,7-12*", "/gunther_branch", &[(4, 12, false)]),
    (
      "/branches/branch1:43832-45742,49990-53669,43832-49987",
      "/branches/branch1",
      &[(43831, 49987, true), (49989, 53669, true)],
    ),
    ("/gunther_branch:7-12,1,5-10", "/gunther_branch", &[(0, 1, true), (4, 12, true)]),
    ("/gunther_branch:7-12*,1,5-10*", "/gunther_branch", &[(0, 1, true), (4, 12, false)]),
    ("/b5:5-53,1-4,54-90*", "/b5", &[(0, 53, true), (53, 90, false)]),
    ("/c0:1-77,12-44", "/c0", &[(0, 77, true)]),
    ("/A/:7-8", "/A", &[(6, 8, true)]),
    ("/A///:7-8", "/A", &[(6, 8, true)]),
    ("/A/.:7-8", "/A", &[(6, 8, true)]),
    ("/A/./B:7-8", "/A/B", &[(6, 8, true)]),
    (":7-8", "/", &[(6, 8, true)]),
  ];
  for (value, source, expected) in cases {
    let mergeinfo = parsed(value);
    assert_eq!(mergeinfo.len(), 1, "{value}");
    assert_eq!(ranges_of(&mergeinfo, source), ranges(expected), "{value}");
  }
}

/// A-605: `test_parse_combine_rangeinfo`: `/trunk: 5,7-9,10,11,13,14` holds three ranges, 5, 7-11 and 13-14.
#[test]
fn adjacent_revisions_of_one_kind_combine_into_one_range() {
  let mergeinfo = parsed("/trunk: 5,7-9,10,11,13,14");
  assert_eq!(
    ranges_of(&mergeinfo, "/trunk"),
    ranges(&[(4, 5, true), (6, 11, true), (12, 14, true)])
  );
}

/// A-605: `test_parse_broken_mergeinfo` (`broken_mergeinfo_vals`): Subversion's 26 broken values are each refused
/// `SVN_ERR_MERGEINFO_PARSE_ERROR`, each by the message its parser gives it.
#[test]
fn subversions_broken_values_are_refused_each_by_its_kind() {
  use MergeinfoRefusal as R;
  let overlap = |first: (Revision, Revision, bool), second: (Revision, Revision, bool)| R::OverlappingInheritance {
    at: 0,
    overlap: RangeOverlap {
      first: range(first.0, first.1, first.2),
      second: range(second.0, second.1, second.2),
    },
  };
  let trailing = "/trunk: 5,7-9,10,11,13,14,";
  let cases: Vec<(&str, MergeinfoRefusal)> = vec![
    ("/missing-revs", R::PathNotTerminated { at: 0 }),
    (trailing, R::RangeListUnterminated { at: trailing.len() }),
    ("/trunk 5,7-9,10,11,13,14", R::PathNotTerminated { at: 0 }),
    (
      "/trunk:5 7--9 10 11 13 14",
      R::InvalidCharacter {
        at: 8,
        byte: b' ',
        after: InvalidAfter::Revision,
      },
    ),
    ("/trunk:5-9*,9", overlap((4, 9, false), (8, 9, true))),
    ("/trunk:5,5-9*", overlap((4, 5, true), (4, 9, false))),
    ("/trunk:5-9,9*", overlap((4, 9, true), (8, 9, false))),
    ("/trunk:5*,5-9", overlap((4, 5, false), (4, 9, true))),
    ("/trunk:4,4*", overlap((3, 4, true), (3, 4, false))),
    ("/trunk:4*,4", overlap((3, 4, false), (3, 4, true))),
    ("/trunk:3-7*,4-23", overlap((2, 7, false), (3, 23, true))),
    ("/trunk:3-7,4-23*", overlap((2, 7, true), (3, 23, false))),
    ("/trunk:22-20", R::ReversedRange { at: 7, first: 22, last: 20 }),
    ("/trunk:22-20*", R::ReversedRange { at: 7, first: 22, last: 20 }),
    ("/trunk:3,7-12,22-20,25", R::ReversedRange { at: 14, first: 22, last: 20 }),
    ("/trunk:3,7,22-20*,25-30", R::ReversedRange { at: 11, first: 22, last: 20 }),
    ("/trunk:22-22", R::SameStartAndEnd { at: 7, revision: 22 }),
    ("/trunk:22-22*", R::SameStartAndEnd { at: 7, revision: 22 }),
    ("/trunk:3,7-12,20-20,25", R::SameStartAndEnd { at: 14, revision: 20 }),
    ("/trunk:3,7,20-20*,25-30", R::SameStartAndEnd { at: 11, revision: 20 }),
    ("/trunk:", R::NoRanges { at: 0 }),
    ("/trunk:2-9\n/branch:", R::NoRanges { at: 11 }),
    ("::", R::NoRanges { at: 0 }),
    ("trunk:a-3", R::RevisionMissing { at: 6, negative: false }),
    ("branch:3-four", R::RevisionMissing { at: 9, negative: false }),
    ("trunk:yadayadayada", R::RevisionMissing { at: 6, negative: false }),
  ];
  assert_eq!(cases.len(), 26);
  for (value, refusal) in cases {
    let refused = Mergeinfo::parse(value.as_bytes()).unwrap_err();
    assert_eq!(refused, refusal, "{value:?}");
    assert_eq!(refused.subversion_code(), 200_020, "{value:?}");
  }
}

/// A-605: `test_rangelist_to_string` and `test_mergeinfo_to_string`: `mergeinfo1` written back in canonical form, its paths
/// in order, and its relative keys written absolute.
#[test]
fn values_are_written_back_in_canonical_form() {
  let mergeinfo = parsed("/trunk: 3,5,7-9,10,11,13,14\n/fred:8-10");
  assert_eq!(
    mergeinfo.get(&path("/trunk")).unwrap().to_bytes(),
    b"3,5,7-11,13-14"
  );
  assert_eq!(mergeinfo.to_bytes(), b"/fred:8-10\n/trunk:3,5,7-11,13-14");
  let relative = parsed("trunk: 3,5,7-9,10,11,13,14\nfred:8-10");
  assert_eq!(relative.to_bytes(), b"/fred:8-10\n/trunk:3,5,7-11,13-14");
}

/// A-605: `svn_path_compare_paths` orders the written lines: a separator before any other byte, so `/a/b` precedes `/a-b`
/// and `/a!`, which byte order would put first, and a path precedes its children.
#[test]
fn written_paths_follow_subversions_path_order() {
  let mergeinfo = parsed("/a-b:1\n/a!:2\n/a/b:3\n/a:4\n/ab:5\n/:6");
  assert_eq!(
    mergeinfo.to_bytes(),
    b"/:6\n/a:4\n/a/b:3\n/a!:2\n/a-b:1\n/ab:5"
  );
}

// ---------------------------------------------------------------------------------------------------------------------
// Subversion's vectors: the algebra.

/// A-605: `test_diff_mergeinfo`: on `/trunk`, 7, 9, 11 and 33-34 deleted and 2, 5-6, 13-16 and 30 added, inheritance ignored.
#[test]
fn a_mergeinfo_difference_names_what_was_deleted_and_added() {
  let from = parsed("/trunk: 1,3-4,7,9,11-12,31-34");
  let to = parsed("/trunk: 1-6,12-16,30-32");
  let delta = Mergeinfo::diff(Some(&from), Some(&to), Compare::WithoutInheritance);
  assert_eq!(delta.deleted.len(), 1);
  assert_eq!(
    ranges_of(&delta.deleted, "/trunk"),
    ranges(&[(6, 7, true), (8, 9, true), (10, 11, true), (32, 34, true)])
  );
  assert_eq!(delta.added.len(), 1);
  assert_eq!(
    ranges_of(&delta.added, "/trunk"),
    ranges(&[(1, 2, true), (4, 6, true), (12, 16, true), (29, 30, true)])
  );
}

/// A-605: `test_rangelist_intersect`: the intersection with and without inheritance, in both argument orders.
#[test]
fn range_lists_intersect_as_subversion_intersects_them() {
  let first = list("1-6,12-16,30-32*,40-42");
  let second = list("1,3-4*,7,9,11-12,31-34*,38-44");
  let considering = ranges(&[(0, 1, true), (11, 12, true), (30, 32, false), (39, 42, true)]);
  let ignoring = ranges(&[(0, 1, true), (2, 4, true), (11, 12, true), (30, 32, false), (39, 42, true)]);
  for (one, other) in [(&first, &second), (&second, &first)] {
    assert_eq!(one.intersect(other, Compare::WithInheritance).ranges(), considering);
    assert_eq!(one.intersect(other, Compare::WithoutInheritance).ranges(), ignoring);
  }
}

/// A-605: `test_mergeinfo_intersect`: only the source both hold survives, intersected with inheritance considered.
#[test]
fn mergeinfo_intersects_by_source() {
  let first = parsed("/trunk: 1-6,12-16\n/foo: 31");
  let second = parsed("/trunk: 1,3-4,7,9,11-12");
  let common = first.intersect(&second, Compare::WithInheritance);
  assert_eq!(common.len(), 1);
  assert_eq!(
    ranges_of(&common, "/trunk"),
    ranges(&[(0, 1, true), (2, 4, true), (11, 12, true)])
  );
}

/// A-605: `test_merge_mergeinfo`: Subversion's twelve merges of two values, each source's ranges as its table gives them, and
/// the changes merged in left as they were.
#[test]
fn mergeinfo_merges_as_subversion_merges_it() {
  type Expected<'a> = &'a [(&'a str, &'a [(Revision, Revision, bool)])];
  let cases: [(&str, &str, Expected<'_>); 12] = [
    ("/trunk: 5-10", "/trunk: 6", &[("/trunk", &[(4, 10, true)])]),
    ("/trunk: 5-10*", "/trunk: 6*", &[("/trunk", &[(4, 10, false)])]),
    ("/trunk: 5-10", "/trunk: 6*", &[("/trunk", &[(4, 10, true)])]),
    ("/trunk: 5-10*", "/trunk: 6", &[("/trunk", &[(4, 5, false), (5, 6, true), (6, 10, false)])]),
    ("/trunk: 1,3,5-11,13", "/trunk: 2,4,12,14-22", &[("/trunk", &[(0, 22, true)])]),
    ("/trunk: 1*,3*,5-11*,13*", "/trunk: 2*,4*,12*,14-22*", &[("/trunk", &[(0, 22, false)])]),
    (
      "/trunk: 1*,3*,5-11*,13*",
      "/trunk: 2,4,12,14-22",
      &[(
        "/trunk",
        &[
          (0, 1, false),
          (1, 2, true),
          (2, 3, false),
          (3, 4, true),
          (4, 11, false),
          (11, 12, true),
          (12, 13, false),
          (13, 22, true),
        ],
      )],
    ),
    (
      "/trunk: 1,3,5-11,13",
      "/trunk: 2*,4*,12*,14-22*",
      &[(
        "/trunk",
        &[
          (0, 1, true),
          (1, 2, false),
          (2, 3, true),
          (3, 4, false),
          (4, 11, true),
          (11, 12, false),
          (12, 13, true),
          (13, 22, false),
        ],
      )],
    ),
    (
      "/trunk::1: 3,5,7-9,10,11,13,14\n/fred:8-10",
      "/trunk::1: 1-4,6\n/fred:9-12",
      &[("/trunk::1", &[(0, 11, true), (12, 14, true)]), ("/fred", &[(7, 12, true)])],
    ),
    (
      "/trunk: 3*,5*,7-9*,10*,11*,13*,14*\n/fred:8-10*",
      "/trunk: 1-4*,6*\n/fred:9-12*",
      &[("/trunk", &[(0, 11, false), (12, 14, false)]), ("/fred", &[(7, 12, false)])],
    ),
    (
      "/trunk: 3,5*,7-9,10,11*,13,14\n/fred:8-10",
      "/trunk: 1-4,6\n/fred:9-12*",
      &[
        (
          "/trunk",
          &[(0, 4, true), (4, 5, false), (5, 10, true), (10, 11, false), (12, 14, true)],
        ),
        ("/fred", &[(7, 10, true), (10, 12, false)]),
      ],
    ),
    (
      "/trunk: 3,5-9*\n/betty: 2-4",
      "/fred: 1-18\n/:barney: 1,3-43",
      &[
        ("/trunk", &[(2, 3, true), (4, 9, false)]),
        ("/betty", &[(1, 4, true)]),
        ("/:barney", &[(0, 1, true), (2, 43, true)]),
        ("/fred", &[(0, 18, true)]),
      ],
    ),
  ];
  for (first, second, expected) in cases {
    let changes = parsed(second);
    let before = changes.to_bytes();
    let merged = parsed(first).merge(&changes);
    assert_eq!(merged.len(), expected.len(), "{first:?} + {second:?}");
    for (source, triples) in expected {
      assert_eq!(ranges_of(&merged, source), ranges(triples), "{first:?} + {second:?}");
    }
    assert_eq!(changes.to_bytes(), before);
  }
}

/// A-605: `test_remove_rangelist`: Subversion's fifteen removals, each with and without inheritance considered.
#[test]
fn range_lists_remove_as_subversion_removes_them() {
  type Triples<'a> = &'a [(Revision, Revision, bool)];
  let cases: [(&str, &str, Triples<'_>, Triples<'_>); 15] = [
    ("1-44", "5", &[(0, 4, true), (5, 44, true)], &[(0, 4, true), (5, 44, true)]),
    ("1-44*", "5", &[(0, 44, false)], &[(0, 4, false), (5, 44, false)]),
    ("1-44", "5*", &[(0, 44, true)], &[(0, 4, true), (5, 44, true)]),
    ("1-44*", "5*", &[(0, 4, false), (5, 44, false)], &[(0, 4, false), (5, 44, false)]),
    ("2-9,14-19", "12", &[(1, 9, true), (13, 19, true)], &[(1, 9, true), (13, 19, true)]),
    ("2-9*,14-19*", "12", &[(1, 9, false), (13, 19, false)], &[(1, 9, false), (13, 19, false)]),
    ("2-9,14-19", "12*", &[(1, 9, true), (13, 19, true)], &[(1, 9, true), (13, 19, true)]),
    ("2-9*,14-19*", "12*", &[(1, 9, false), (13, 19, false)], &[(1, 9, false), (13, 19, false)]),
    ("1,9-17", "12-20", &[(0, 1, true), (8, 11, true)], &[(0, 1, true), (8, 11, true)]),
    ("1,9-17*", "12-20", &[(0, 1, true), (8, 17, false)], &[(0, 1, true), (8, 11, false)]),
    ("1,9-17", "12-20*", &[(0, 1, true), (8, 17, true)], &[(0, 1, true), (8, 11, true)]),
    ("1,9-17*", "12-20*", &[(0, 1, true), (8, 11, false)], &[(0, 1, true), (8, 11, false)]),
    ("", "", &[], &[]),
    ("", "5-8,10-100", &[], &[]),
    ("5-8,10-100", "", &[(4, 8, true), (9, 100, true)], &[(4, 8, true), (9, 100, true)]),
  ];
  for (whiteboard, eraser, considering, ignoring) in cases {
    let (whiteboard, eraser) = (list(whiteboard), list(eraser));
    let (white_before, eraser_before) = (whiteboard.clone(), eraser.clone());
    assert_eq!(
      whiteboard.remove(&eraser, Compare::WithInheritance).ranges(),
      ranges(considering),
      "{whiteboard:?} - {eraser:?}"
    );
    assert_eq!(
      whiteboard.remove(&eraser, Compare::WithoutInheritance).ranges(),
      ranges(ignoring),
      "{whiteboard:?} - {eraser:?}"
    );
    assert_eq!((whiteboard, eraser), (white_before, eraser_before));
  }
}

/// A-605: `test_remove_mergeinfo`: removing `/trunk: 1-6,12-16,30-32` leaves 7, 9, 11 and 33-34.
#[test]
fn mergeinfo_removes_by_source() {
  let whiteboard = parsed("/trunk: 1,3-4,7,9,11-12,31-34");
  let eraser = parsed("/trunk: 1-6,12-16,30-32");
  let left = whiteboard.remove(&eraser, Compare::WithInheritance);
  assert_eq!(left.len(), 1);
  assert_eq!(
    ranges_of(&left, "/trunk"),
    ranges(&[(6, 7, true), (8, 9, true), (10, 11, true), (32, 34, true)])
  );
}

/// A-605: `test_rangelist_merge`: Subversion's 68 merges of two range lists.
#[test]
fn range_lists_merge_as_subversion_merges_them() {
  type Triples<'a> = &'a [(Revision, Revision, bool)];
  let cases: [(&str, &str, Triples<'_>); 68] = [
    ("1-44", "70-101", &[(0, 44, true), (69, 101, true)]),
    ("1-44*", "70-101", &[(0, 44, false), (69, 101, true)]),
    ("1-44", "70-101*", &[(0, 44, true), (69, 101, false)]),
    ("1-44*", "70-101*", &[(0, 44, false), (69, 101, false)]),
    ("70-101", "1-44", &[(0, 44, true), (69, 101, true)]),
    ("70-101*", "1-44", &[(0, 44, true), (69, 101, false)]),
    ("70-101", "1-44*", &[(0, 44, false), (69, 101, true)]),
    ("70-101*", "1-44*", &[(0, 44, false), (69, 101, false)]),
    ("4-20", "4-20", &[(3, 20, true)]),
    ("4-20*", "4-20", &[(3, 20, true)]),
    ("4-20", "4-20*", &[(3, 20, true)]),
    ("4-20*", "4-20*", &[(3, 20, false)]),
    ("6-17", "6-12", &[(5, 17, true)]),
    ("6-17*", "6-12", &[(5, 12, true), (12, 17, false)]),
    ("6-17", "6-12*", &[(5, 17, true)]),
    ("6-17*", "6-12*", &[(5, 17, false)]),
    ("6-12", "6-17", &[(5, 17, true)]),
    ("6-12*", "6-17", &[(5, 17, true)]),
    ("6-12", "6-17*", &[(5, 12, true), (12, 17, false)]),
    ("6-12*", "6-17*", &[(5, 17, false)]),
    ("5-77", "44-77", &[(4, 77, true)]),
    ("5-77*", "44-77", &[(4, 43, false), (43, 77, true)]),
    ("5-77", "44-77*", &[(4, 77, true)]),
    ("5-77*", "44-77*", &[(4, 77, false)]),
    ("44-77", "5-77", &[(4, 77, true)]),
    ("44-77*", "5-77", &[(4, 77, true)]),
    ("44-77", "5-77*", &[(4, 43, false), (43, 77, true)]),
    ("44-77*", "5-77*", &[(4, 77, false)]),
    ("12-24", "20-23", &[(11, 24, true)]),
    ("12-24*", "20-23", &[(11, 19, false), (19, 23, true), (23, 24, false)]),
    ("12-24", "20-23*", &[(11, 24, true)]),
    ("12-24*", "20-23*", &[(11, 24, false)]),
    ("20-23", "12-24", &[(11, 24, true)]),
    ("20-23*", "12-24", &[(11, 24, true)]),
    ("20-23", "12-24*", &[(11, 19, false), (19, 23, true), (23, 24, false)]),
    ("20-23*", "12-24*", &[(11, 24, false)]),
    ("50-73", "60-99", &[(49, 99, true)]),
    ("50-73*", "60-99", &[(49, 59, false), (59, 99, true)]),
    ("50-73", "60-99*", &[(49, 73, true), (73, 99, false)]),
    ("50-73*", "60-99*", &[(49, 99, false)]),
    ("60-99", "50-73", &[(49, 99, true)]),
    ("60-99*", "50-73", &[(49, 73, true), (73, 99, false)]),
    ("60-99", "50-73*", &[(49, 59, false), (59, 99, true)]),
    ("60-99*", "50-73*", &[(49, 99, false)]),
    ("1-5,7,12-13", "2-17", &[(0, 17, true)]),
    ("1-5*,7*,12-13*", "2-17*", &[(0, 17, false)]),
    (
      "1-5,7,12-13",
      "2-17*",
      &[(0, 5, true), (5, 6, false), (6, 7, true), (7, 11, false), (11, 13, true), (13, 17, false)],
    ),
    ("1-5*,7*,12-13*", "2-17", &[(0, 1, false), (1, 17, true)]),
    ("2-17", "1-5,7,12-13", &[(0, 17, true)]),
    ("2-17*", "1-5*,7*,12-13*", &[(0, 17, false)]),
    (
      "2-17*",
      "1-5,7,12-13",
      &[(0, 5, true), (5, 6, false), (6, 7, true), (7, 11, false), (11, 13, true), (13, 17, false)],
    ),
    ("2-17", "1-5*,7*,12-13*", &[(0, 1, false), (1, 17, true)]),
    (
      "3-4*,10-15,20",
      "5-60*",
      &[(2, 9, false), (9, 15, true), (15, 19, false), (19, 20, true), (20, 60, false)],
    ),
    (
      "5-60*",
      "3-4*,10-15,20",
      &[(2, 9, false), (9, 15, true), (15, 19, false), (19, 20, true), (20, 60, false)],
    ),
    ("3-4*,50-100*", "5-60*", &[(2, 100, false)]),
    ("5-60*", "3-4*,50-100*", &[(2, 100, false)]),
    ("3-4*,50-100", "5-60*", &[(2, 49, false), (49, 100, true)]),
    ("5-60*", "3-4*,50-100", &[(2, 49, false), (49, 100, true)]),
    ("3-4,50-100*", "5-60", &[(2, 60, true), (60, 100, false)]),
    ("5-60", "3-4,50-100*", &[(2, 60, true), (60, 100, false)]),
    (
      "5,9,11-15,17,200-300,999",
      "7-50",
      &[(4, 5, true), (6, 50, true), (199, 300, true), (998, 999, true)],
    ),
    ("1-44,45,46,47-50", "", &[(0, 50, true)]),
    ("1,2,3,4,5,6,7,8", "", &[(0, 8, true)]),
    ("6-10,12-13,14,15,16-22", "", &[(5, 10, true), (11, 22, true)]),
    ("", "1-44,45,46,47-50", &[(0, 50, true)]),
    ("", "1,2,3,4,5,6,7,8", &[(0, 8, true)]),
    ("", "6-10,12-13,14,15,16-22", &[(5, 10, true), (11, 22, true)]),
    ("", "", &[]),
  ];
  for (first, second, expected) in cases {
    let changes = list(second);
    let before = changes.clone();
    let merged = list(first).merge(&changes);
    assert_eq!(merged.ranges(), ranges(expected), "{first:?} + {second:?}");
    assert!(merged.is_canonical(), "{first:?} + {second:?}");
    assert_eq!(changes, before);
  }
}

/// A-605: `test_rangelist_diff`: Subversion's sixteen differences, each with and without inheritance considered.
#[test]
fn range_lists_differ_as_subversion_differs_them() {
  type Triples<'a> = &'a [(Revision, Revision, bool)];
  let cases: [(&str, &str, Triples<'_>, Triples<'_>, Triples<'_>, Triples<'_>); 16] = [
    ("1", "3", &[(2, 3, true)], &[(0, 1, true)], &[(2, 3, true)], &[(0, 1, true)]),
    ("1", "1,3", &[(2, 3, true)], &[], &[(2, 3, true)], &[]),
    ("1,3", "1", &[], &[(2, 3, true)], &[], &[(2, 3, true)]),
    ("1,3", "1,3", &[], &[], &[], &[]),
    ("1,3*", "1,3*", &[], &[], &[], &[]),
    (
      "1,5-8",
      "1,6,10-12",
      &[(9, 12, true)],
      &[(4, 5, true), (6, 8, true)],
      &[(9, 12, true)],
      &[(4, 5, true), (6, 8, true)],
    ),
    ("6*", "6", &[(5, 6, true)], &[(5, 6, false)], &[], &[]),
    ("6", "6*", &[(5, 6, false)], &[(5, 6, true)], &[], &[]),
    ("6*", "6", &[(5, 6, true)], &[(5, 6, false)], &[], &[]),
    (
      "1,5-8",
      "1,6*,10-12",
      &[(5, 6, false), (9, 12, true)],
      &[(4, 8, true)],
      &[(9, 12, true)],
      &[(4, 5, true), (6, 8, true)],
    ),
    (
      "1,5-8*",
      "1,6,10-12",
      &[(5, 6, true), (9, 12, true)],
      &[(4, 8, false)],
      &[(9, 12, true)],
      &[(4, 5, false), (6, 8, false)],
    ),
    ("3-9", "", &[], &[(2, 9, true)], &[], &[(2, 9, true)]),
    ("3-9*", "", &[], &[(2, 9, false)], &[], &[(2, 9, false)]),
    ("", "3-9", &[(2, 9, true)], &[], &[(2, 9, true)], &[]),
    ("", "3-9*", &[(2, 9, false)], &[], &[(2, 9, false)], &[]),
    ("", "", &[], &[], &[], &[]),
  ];
  for (from, to, added, deleted, added_ignoring, deleted_ignoring) in cases {
    let (from, to) = (list(from), list(to));
    let considering = from.diff(&to, Compare::WithInheritance);
    assert_eq!(considering.added.ranges(), ranges(added), "{from:?} -> {to:?}");
    assert_eq!(considering.deleted.ranges(), ranges(deleted), "{from:?} -> {to:?}");
    let ignoring = from.diff(&to, Compare::WithoutInheritance);
    assert_eq!(ignoring.added.ranges(), ranges(added_ignoring), "{from:?} -> {to:?}");
    assert_eq!(ignoring.deleted.ranges(), ranges(deleted_ignoring), "{from:?} -> {to:?}");
  }
}

/// A-605: `test_rangelist_merge_overlap` (issue 4686), `test_rangelist_merge_canonical_result` and
/// `test_rangelist_merge_array_insert_failure` (issue 4840): merges whose results Subversion once got wrong, or aborted
/// on, give the canonical results it gives now.
#[test]
fn the_merges_subversion_once_got_wrong_give_its_fixed_results() {
  let cases = [
    ("19473-19612*,19615-19630*,19631-19634", "15014-20515*", "15014-19630*,19631-19634,19635-20515*"),
    ("8-10", "5-10*,11-24", "5-7*,8-24"),
    ("2*,4*,6*,8", "1-9*,11", "1-7*,8,9*,11"),
  ];
  for (first, second, expected) in cases {
    let merged = list(first).merge(&list(second));
    assert_eq!(merged.to_bytes(), expected.as_bytes(), "{first} + {second}");
    assert!(merged.is_canonical());
  }
  let reported = list(
    "997347-997597*,997884-1000223*,1000542-1000551*,1001389-1001516,1002139-1002268*,1002896-1003064*,\
     1003320-1003468,1005939-1006089*,1006443-1006630*,1006631-1006857,1007028-1007116*,1009467-1009629,\
     1009630-1010007*,1010774-1010860,1011036-1011502,1011672-1014004*,1014023-1014197,1014484-1014542*,\
     1015077-1015568,1016219-1016365,1016698-1016845,1017331-1018616,1027032-1027180,1027855-1028051,\
     1028261-1028395,1028553-1028663,1028674-1028708,1028773-1028891*,1029223-1030557,1032239-1032284*,\
     1032801-1032959,1032960-1033074*,1033745-1033810,1034990-1035104,1035435-1036108*,1036109-1036395,\
     1036396-1036865*,1036866-1036951,1036952-1037647*,1037648-1037750,1037751-1038548*,1038549-1038700,\
     1038701-1042103*,1042104-1042305,1042306-1046626*,1046627-1046910,1046911-1047676*,1047677-1047818,\
     1047819-1047914*,1047915-1048025,1048026-1048616*,1048617-1048993,1048994-1050066*,1054605-1054739,\
     1054854-1055021",
  );
  let merged = reported.merge(&list("1035435-1050066*,1052459-1054617"));
  assert!(merged.is_canonical());
  assert_eq!(covers(&merged, 1_060_000), merged_covers(&reported, &list("1035435-1050066*,1052459-1054617"), 1_060_000));
}

/// A-605: `test_rangelist_loop`: every single range over revisions 0 to 62, of either kind, merged with Subversion's base list
/// in both orders gives a canonical list; and, which Subversion's test leaves to do, the per-revision union.
#[test]
fn every_single_range_merges_with_a_mixed_list_canonically() {
  let base = list("2,4,7-9,12-15,18-20,22*,25*,28-30*,33-35*,38-40,43-45*,48-50,52-54,56-59*");
  for start in 0..62 {
    for end in start + 1..63 {
      for inheritable in [false, true] {
        let change = RangeList::canonical(vec![range(start, end, inheritable)]).unwrap();
        for merged in [base.merge(&change), change.merge(&base)] {
          assert!(merged.is_canonical(), "{start}-{end} {inheritable}");
          assert_eq!(covers(&merged, 63), merged_covers(&base, &change, 63));
        }
      }
    }
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// The parser beyond Subversion's tables.

/// A-605: A revision is decimal digits alone, at most ten, at most 2^31 - 1, never 0, as `svn_revnum_parse` reads it: no
/// sign and no space, so `5, 7` is refused where the line's first space is skipped.
#[test]
fn revisions_are_read_as_svn_revnum_parse_reads_them() {
  use MergeinfoRefusal as R;
  assert_eq!(ranges_of(&parsed("/a:2147483647"), "/a"), ranges(&[(2_147_483_646, 2_147_483_647, true)]));
  assert_eq!(ranges_of(&parsed("/a:0000000007"), "/a"), ranges(&[(6, 7, true)]));
  let refused = [
    ("/a:2147483648", R::RevisionTooLarge { at: 3 }),
    ("/a:3000000000", R::RevisionTooLarge { at: 3 }),
    ("/a:12345678901", R::RevisionTooLong { at: 3 }),
    ("/a:00000000007", R::RevisionTooLong { at: 3 }),
    ("/a:0", R::RevisionZero { at: 3 }),
    ("/a:0-5", R::RevisionZero { at: 3 }),
    ("/a:-5", R::RevisionMissing { at: 3, negative: true }),
    ("/a:+5", R::RevisionMissing { at: 3, negative: false }),
    ("/a:5, 7", R::RevisionMissing { at: 5, negative: false }),
    ("/a:5- 7", R::RevisionMissing { at: 5, negative: false }),
    ("/a:5-\n7", R::RevisionMissing { at: 5, negative: false }),
    ("/a:1-2147483648", R::RevisionTooLarge { at: 5 }),
  ];
  for (value, refusal) in refused {
    assert_eq!(Mergeinfo::parse(value.as_bytes()), Err(refusal), "{value:?}");
  }
}

/// A-605: What may end a range: a comma, a `*`, the line's newline or the value's end; anything else is refused with the
/// message for where it stands, after a revision or after a range.
#[test]
fn a_byte_where_a_range_must_end_is_refused_by_where_it_stands() {
  use MergeinfoRefusal as R;
  let invalid = |at, byte, after| R::InvalidCharacter { at, byte, after };
  let cases = [
    ("/a:1x", invalid(4, b'x', InvalidAfter::Revision)),
    ("/a:1-3x", invalid(6, b'x', InvalidAfter::Range)),
    ("/a:1*x", invalid(5, b'x', InvalidAfter::Range)),
    ("/a:1-3*-", invalid(7, b'-', InvalidAfter::Range)),
    ("/a:1-3-5", invalid(6, b'-', InvalidAfter::Range)),
    ("/a:1**", invalid(5, b'*', InvalidAfter::Range)),
    ("/a:1 ", invalid(4, b' ', InvalidAfter::Revision)),
    ("/a:1\r", invalid(4, b'\r', InvalidAfter::Revision)),
  ];
  for (value, refusal) in cases {
    assert_eq!(Mergeinfo::parse(value.as_bytes()), Err(refusal), "{value:?}");
  }
}

/// A-605: The grammar's edges as `parse_rangelist` and `parse_top` draw them: a comma before a newline is accepted, at the
/// value's end refused; one trailing newline is accepted, an empty line refused; the C locale's spaces after a colon
/// skipped; the empty value is empty mergeinfo; a path named twice has its ranges merged, inheritable winning where
/// they overlap (issue #3547's `trunk` and `/trunk`).
#[test]
fn the_grammar_edges_fall_where_subversions_parser_draws_them() {
  use MergeinfoRefusal as R;
  assert_eq!(parsed("/a:1,\n/b:2").to_bytes(), b"/a:1\n/b:2");
  assert_eq!(Mergeinfo::parse(b"/a:1,"), Err(R::RangeListUnterminated { at: 5 }));
  assert_eq!(parsed("/a:1\n").to_bytes(), b"/a:1");
  assert_eq!(Mergeinfo::parse(b"/a:1\n\n"), Err(R::PathNotTerminated { at: 5 }));
  assert_eq!(Mergeinfo::parse(b"\n"), Err(R::PathNotTerminated { at: 0 }));
  assert_eq!(Mergeinfo::parse(b"/a: \n/b:1"), Err(R::NoRanges { at: 0 }));
  assert_eq!(parsed("/a:\t\x0b\x0c\r 5").to_bytes(), b"/a:5");
  assert_eq!(parsed(""), Mergeinfo::default());
  assert!(parsed("").is_empty());
  assert_eq!(parsed("trunk:4033\n/trunk:4039-4995").to_bytes(), b"/trunk:4033,4039-4995");
  assert_eq!(parsed("/a:1-5\n/a:3*").to_bytes(), b"/a:1-5");
  assert_eq!(parsed("/a:3*\n/a:1-5*").to_bytes(), b"/a:1-5*");
  assert_eq!(parsed("/a/../b/./:1").to_bytes(), b"/a/../b:1");
}

/// A-605: The value is a C string: a parse reads to its first NUL, as `svn_mergeinfo_parse` reads a `const char *`; a commit's
/// check refuses the NUL first (`verify_mergeinfo`, "unexpected string terminator").
#[test]
fn a_nul_ends_a_read_and_is_refused_at_commit() {
  let value = b"/a:1\0/b:garbage";
  assert_eq!(Mergeinfo::parse(value).unwrap().to_bytes(), b"/a:1");
  assert_eq!(
    Mergeinfo::verify(value),
    Err(MergeinfoRefusal::Terminator { at: 4 })
  );
  assert_eq!(Mergeinfo::verify(b"/a:1-3*").unwrap().to_bytes(), b"/a:1-3*");
  assert_eq!(
    Mergeinfo::verify(b"/a:0"),
    Err(MergeinfoRefusal::RevisionZero { at: 3 })
  );
}

// ---------------------------------------------------------------------------------------------------------------------
// The per-revision model (Subversion's `rl_array_t`): a range list as each revision's cover.

/// The revisions the generated lists span: Subversion's own random tests' `RANGELIST_TESTS_MAX_REV`.
const MODEL_REVISIONS: usize = 15;

/// A revision's cover in the model: none, non-inheritable, inheritable.
const NONE: u8 = 0;
/// A non-inheritable cover.
const PARTIAL: u8 = 1;
/// An inheritable cover.
const WHOLE: u8 = 2;

/// Each revision's cover, revisions 1 to `span`, as index 0 to `span - 1`.
fn covers(list: &RangeList, span: u64) -> Vec<u8> {
  let mut covers = vec![NONE; usize::try_from(span).unwrap()];
  for range in list.ranges() {
    for revision in range.start() + 1..=range.end() {
      covers[usize::try_from(revision - 1).unwrap()] = if range.inheritable() { WHOLE } else { PARTIAL };
    }
  }
  covers
}

/// The union's covers by the model: each revision's larger cover.
fn merged_covers(first: &RangeList, second: &RangeList, span: u64) -> Vec<u8> {
  covers(first, span)
    .into_iter()
    .zip(covers(second, span))
    .map(|(one, other)| one.max(other))
    .collect()
}

/// The canonical list of the given covers: one range a revision, combined.
fn from_covers(covers: &[u8]) -> RangeList {
  let ranges = covers
    .iter()
    .enumerate()
    .filter(|&(_, &cover)| cover != NONE)
    .map(|(position, &cover)| {
      let revision = u64::try_from(position).unwrap() + 1;
      range(revision - 1, revision, cover == WHOLE)
    })
    .collect();
  RangeList::canonical(ranges).unwrap()
}

/// Covers of `MODEL_REVISIONS` revisions.
fn cover_strategy() -> impl Strategy<Value = Vec<u8>> {
  prop::collection::vec(0..=WHOLE, MODEL_REVISIONS)
}

/// Covers of `MODEL_REVISIONS` revisions, none of them non-inheritable.
fn whole_strategy() -> impl Strategy<Value = Vec<u8>> {
  prop::collection::vec(prop_oneof![Just(NONE), Just(WHOLE)], MODEL_REVISIONS)
}

/// Which revisions a list holds, whatever their kind.
fn held(covers: &[u8]) -> Vec<bool> {
  covers.iter().map(|&cover| cover != NONE).collect()
}

/// Whether each range of `list` is inheritable exactly when some revision of it is inheritable in `sources`: the rule
/// `combine_with_lastrange` gives when inheritance is ignored, joining touching pieces.
fn kinds_join_touching_pieces(list: &RangeList, sources: &[&[u8]]) -> bool {
  list.ranges().iter().all(|range| {
    let any_whole = (range.start() + 1..=range.end()).any(|revision| {
      let position = usize::try_from(revision - 1).unwrap();
      sources.iter().any(|covers| covers[position] == WHOLE)
    });
    range.inheritable() == any_whole
  })
}

/// The span of every model list.
fn span() -> u64 {
  u64::try_from(MODEL_REVISIONS).unwrap()
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-605: The union is each revision's larger cover, canonical, whichever list merges into which (Subversion's
  /// `test_rangelist_merge_random_canonical_inputs`).
  #[test]
  fn a_union_is_each_revisions_larger_cover(first in cover_strategy(), second in cover_strategy()) {
    let (one, other) = (from_covers(&first), from_covers(&second));
    for merged in [one.merge(&other), other.merge(&one)] {
      prop_assert!(merged.is_canonical());
      prop_assert_eq!(covers(&merged, span()), merged_covers(&one, &other, span()));
    }
  }

  /// A-605: Of inheritable ranges alone, intersection, removal and difference are the sets' (Subversion's
  /// `test_rangelist_intersect_randomly`, `test_rangelist_remove_randomly`), with inheritance compared or not.
  #[test]
  fn inheritable_lists_intersect_and_remove_as_sets(first in whole_strategy(), second in whole_strategy()) {
    let (one, other) = (from_covers(&first), from_covers(&second));
    let both: Vec<u8> = first.iter().zip(&second).map(|(&a, &b)| if a == WHOLE && b == WHOLE { WHOLE } else { NONE }).collect();
    let less: Vec<u8> = first.iter().zip(&second).map(|(&a, &b)| if a == WHOLE && b == NONE { WHOLE } else { NONE }).collect();
    let more: Vec<u8> = first.iter().zip(&second).map(|(&a, &b)| if b == WHOLE && a == NONE { WHOLE } else { NONE }).collect();
    for compare in [Compare::WithInheritance, Compare::WithoutInheritance] {
      prop_assert_eq!(one.intersect(&other, compare), from_covers(&both));
      prop_assert_eq!(one.remove(&other, compare), from_covers(&less));
      let delta = one.diff(&other, compare);
      prop_assert_eq!(delta.deleted, from_covers(&less));
      prop_assert_eq!(delta.added, from_covers(&more));
    }
  }

  /// A-605: Without inheritance the walk is positional: an intersection holds the revisions both hold and a removal those the
  /// eraser lacks, canonical, each range inheritable exactly when a revision of it is in either list (touching pieces
  /// joined by `combine_with_lastrange`); and two lists are equal exactly when they hold the same revisions.
  #[test]
  fn without_inheritance_lists_meet_by_revision(first in cover_strategy(), second in cover_strategy()) {
    let (one, other) = (from_covers(&first), from_covers(&second));
    let common = one.intersect(&other, Compare::WithoutInheritance);
    prop_assert!(common.is_canonical());
    let expected: Vec<bool> = held(&first).iter().zip(held(&second)).map(|(&a, b)| a && b).collect();
    prop_assert_eq!(held(&covers(&common, span())), expected);
    prop_assert!(kinds_join_touching_pieces(&common, &[&first, &second]));
    let left = one.remove(&other, Compare::WithoutInheritance);
    prop_assert!(left.is_canonical());
    let expected: Vec<bool> = held(&first).iter().zip(held(&second)).map(|(&a, b)| a && !b).collect();
    prop_assert_eq!(held(&covers(&left, span())), expected);
    prop_assert!(kinds_join_touching_pieces(&left, &[&first]));
    let single = |ranges: &RangeList| {
      let mut mergeinfo = Mergeinfo::default();
      mergeinfo.insert(path("/s"), ranges.clone());
      mergeinfo
    };
    let equal = Mergeinfo::equals(Some(&single(&one)), Some(&single(&other)), Compare::WithoutInheritance);
    prop_assert_eq!(equal, held(&first) == held(&second));
  }

  /// A-605: With inheritance compared, Subversion's walk is not exact per revision (it may pass a range at one of another kind,
  /// module documentation), but it is bounded: an intersection holds only revisions both hold with one kind, of that
  /// kind; a removal keeps every revision the eraser holds with another kind or not at all, and adds none.
  #[test]
  fn with_inheritance_lists_meet_within_the_per_revision_bounds(first in cover_strategy(), second in cover_strategy()) {
    let (one, other) = (from_covers(&first), from_covers(&second));
    let common = one.intersect(&other, Compare::WithInheritance);
    prop_assert!(common.is_canonical());
    for (position, cover) in covers(&common, span()).into_iter().enumerate() {
      prop_assert!(cover == NONE || (cover == first[position] && cover == second[position]));
    }
    let left = one.remove(&other, Compare::WithInheritance);
    prop_assert!(left.is_canonical());
    for (position, cover) in covers(&left, span()).into_iter().enumerate() {
      let kept_by_rule = first[position] != NONE && first[position] != second[position];
      prop_assert!(cover == NONE || cover == first[position]);
      prop_assert!(!kept_by_rule || cover == first[position]);
    }
  }

  /// A-605: A canonical value written and read again is itself, and writing is stable.
  #[test]
  fn a_written_value_reads_back_as_itself(
    sources in prop::collection::btree_map(prop_oneof![Just("/trunk"), Just("/a-b"), Just("/a/b"), Just("/"), Just("/x:y")], cover_strategy(), 0..4)
  ) {
    let mut mergeinfo = Mergeinfo::default();
    for (source, cover) in &sources {
      mergeinfo.insert(path(source), from_covers(cover));
    }
    let written = mergeinfo.to_bytes();
    let read = Mergeinfo::parse(&written).unwrap();
    prop_assert_eq!(read.to_bytes(), written);
    prop_assert_eq!(read, mergeinfo);
  }

  /// A-605: Hostile values, from the grammar's alphabet and any byte: a read never panics, and what it accepts writes back to
  /// a value that reads as itself; a commit's check accepts exactly what a read accepts of a value with no NUL.
  #[test]
  fn hostile_values_are_refused_or_read_canonically(
    value in prop::collection::vec(prop_oneof![
      4 => prop::sample::select(b"0123456789-,*:/\n .a".to_vec()),
      1 => any::<u8>(),
    ], 0..48)
  ) {
    match Mergeinfo::parse(&value) {
      Ok(mergeinfo) => {
        let written = mergeinfo.to_bytes();
        prop_assert_eq!(Mergeinfo::parse(&written), Ok(mergeinfo));
      }
      Err(refusal) => prop_assert_eq!(refusal.subversion_code(), 200_020),
    }
    if !value.contains(&0) {
      prop_assert_eq!(Mergeinfo::verify(&value), Mergeinfo::parse(&value));
    } else {
      let refused = matches!(Mergeinfo::verify(&value), Err(MergeinfoRefusal::Terminator { .. }));
      prop_assert!(refused);
    }
  }

  /// A-605: Every cut and every bit flip of a valid value is refused, typed, or read canonically; none panics.
  #[test]
  fn cut_and_flipped_values_never_panic(cut in 0_usize..64, flip in 0_usize..512) {
    let value = b"/trunk:1-5,7*,9-12\n/branches/b:3,100-2147483647*".to_vec();
    let cut_value = value.get(..cut.min(value.len())).unwrap().to_vec();
    let mut flipped = value.clone();
    let position = flip / 8 % flipped.len();
    flipped[position] ^= 1 << (flip % 8);
    for candidate in [cut_value, flipped] {
      if let Ok(mergeinfo) = Mergeinfo::parse(&candidate) {
        prop_assert_eq!(Mergeinfo::parse(&mergeinfo.to_bytes()), Ok(mergeinfo));
      }
    }
  }
}

/// A-605: A value of a hundred thousand ranges, and one of a hundred thousand lines, read and written whole: the work is a
/// pass over the bytes and the result holds what the value names.
#[test]
fn long_values_read_in_proportion_to_their_bytes() {
  let many_ranges: Vec<String> = (1..=100_000_u64).map(|revision| (revision * 2).to_string()).collect();
  let value = format!("/a:{}", many_ranges.join(","));
  let mergeinfo = parsed(&value);
  assert_eq!(mergeinfo.get(&path("/a")).unwrap().ranges().len(), 100_000);
  assert_eq!(mergeinfo.to_bytes(), value.as_bytes());
  let many_lines: Vec<String> = (1..=100_000_u64).map(|line| format!("/p{line}:{line}")).collect();
  let mergeinfo = parsed(&many_lines.join("\n"));
  assert_eq!(mergeinfo.len(), 100_000);
}

// ---------------------------------------------------------------------------------------------------------------------
// Elision and the protocol's words.

/// A-605: A range is forward or nothing, and holds the changes after its start through its end
/// (`svn_merge_range_contains_rev`).
#[test]
fn a_range_is_forward_and_holds_the_revisions_after_its_start() {
  assert_eq!(MergeRange::new(5, 5, true), None);
  assert_eq!(MergeRange::new(6, 5, true), None);
  let five_to_seven = range(4, 7, false);
  assert!(!five_to_seven.contains(4));
  assert!(five_to_seven.contains(5) && five_to_seven.contains(7));
  assert!(!five_to_seven.contains(8));
  assert_eq!(RangeList::canonical(vec![range(4, 7, false), range(5, 9, true)]).unwrap_err(), RangeOverlap { first: range(4, 7, false), second: range(5, 9, true) });
}

/// A-605: `should_elide_mergeinfo`: no mergeinfo never elides; empty mergeinfo elides to none or empty; any other never to none
/// or empty, and otherwise when equal with inheritance compared.
#[test]
fn mergeinfo_elides_by_subversions_rules() {
  let empty = Mergeinfo::default();
  let some = parsed("/trunk:1-5");
  let partial = parsed("/trunk:1-5*");
  assert!(!elides(None, None));
  assert!(!elides(None, Some(&some)));
  assert!(elides(Some(&empty), None));
  assert!(elides(Some(&empty), Some(&empty)));
  assert!(!elides(Some(&empty), Some(&some)));
  assert!(!elides(Some(&some), None));
  assert!(!elides(Some(&some), Some(&empty)));
  assert!(elides(Some(&some), Some(&parsed("/trunk:1-3,4-5"))));
  assert!(!elides(Some(&partial), Some(&some)));
  assert!(!elides(Some(&some), Some(&parsed("/trunk:1-6"))));
}

/// A-605: The protocol's words: each mode's own, and any other word explicit (`svn_inheritance_from_word`).
#[test]
fn inheritance_words_read_as_subversion_reads_them() {
  for mode in [
    MergeinfoInheritance::Explicit,
    MergeinfoInheritance::Inherited,
    MergeinfoInheritance::NearestAncestor,
  ] {
    assert_eq!(MergeinfoInheritance::from_word(mode.word().as_bytes()), mode);
  }
  assert_eq!(MergeinfoInheritance::from_word(b"Inherited"), MergeinfoInheritance::Explicit);
  assert_eq!(MergeinfoInheritance::from_word(b""), MergeinfoInheritance::Explicit);
}

// ---------------------------------------------------------------------------------------------------------------------
// The naive filesystem: every revision's whole tree, and the index built from the same rows.

/// A transaction name of the form §4.17.4 item 3.1 gives.
const TRANSACTION: &str = "0-1z";

/// One node of the naive filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NaiveNode {
  /// Its kind.
  kind: NodeKind,
  /// Its property list.
  properties: PropertyList,
}

/// A property list a row sets.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Props {
  /// No properties.
  Empty,
  /// A list without `svn:mergeinfo`.
  Without,
  /// A list holding this `svn:mergeinfo` value.
  With(Vec<u8>),
}

/// One row a test commits.
#[derive(Clone, Debug)]
enum Op {
  /// `add-dir` without history.
  AddDir(String, Option<Props>),
  /// `add-file` without history.
  AddFile(String, Option<Props>),
  /// A copy of a path of an earlier revision.
  Copy(String, String, Revision, Option<Props>),
  /// A deletion.
  Delete(String),
  /// A property change.
  SetProps(String, Props),
}

/// The naive filesystem and the index built from the same rows; the green's side of [`Contents`].
struct Naive {
  /// Every revision's whole tree.
  trees: Vec<BTreeMap<String, NaiveNode>>,
  /// Every property list's `svn:mergeinfo`, `None` for a list without one.
  values: BTreeMap<ContentIdentity, Option<Vec<u8>>>,
  /// The index.
  index: Index,
  /// The lists named so far.
  lists: u64,
  /// The directories listed, in order.
  listed: RefCell<Vec<(String, Revision)>>,
}

/// Whether `place` is `path` or beneath it.
fn at_or_beneath(place: &str, path: &str) -> bool {
  place == path || path == "/" || place.strip_prefix(path).is_some_and(|rest| rest.starts_with('/'))
}

/// `place` moved from beneath `from` to beneath `to`.
fn moved(place: &str, from: &str, to: &str) -> String {
  let rest = &place[from.len()..];
  if from == "/" { format!("{to}/{rest}") } else { format!("{to}{rest}") }
}

/// The parent of a path other than the root.
fn parent_of(path: &str) -> &str {
  match path.rfind('/') {
    Some(0) | None => "/",
    Some(cut) => &path[..cut],
  }
}

impl Naive {
  /// A repository at revision 0.
  fn new() -> Self {
    let root = NaiveNode {
      kind: NodeKind::Directory,
      properties: PropertyList::Empty,
    };
    Self {
      trees: vec![BTreeMap::from([("/".to_owned(), root)])],
      values: BTreeMap::new(),
      index: Index::new(),
      lists: 0,
      listed: RefCell::new(Vec::new()),
    }
  }

  /// The youngest tree.
  fn head(&self) -> &BTreeMap<String, NaiveNode> {
    self.trees.last().unwrap()
  }

  /// The tree of `revision`.
  fn tree(&self, revision: Revision) -> &BTreeMap<String, NaiveNode> {
    &self.trees[usize::try_from(revision).unwrap()]
  }

  /// A property list for `props`, named afresh.
  fn list_for(&mut self, props: &Props) -> PropertyList {
    let value = match props {
      Props::Empty => return PropertyList::Empty,
      Props::Without => None,
      Props::With(value) => Some(value.clone()),
    };
    self.lists += 1;
    let mut identity = [0_u8; 32];
    identity[..8].copy_from_slice(&self.lists.to_le_bytes());
    let has_mergeinfo = value.is_some();
    self.values.insert(identity, value);
    PropertyList::Held { identity, has_mergeinfo }
  }

  /// Commits one revision of rows that touch disjoint subtrees, to the naive trees and to the index; answers its
  /// number.
  fn commit(&mut self, ops: &[Op]) -> Revision {
    let mut tree = self.head().clone();
    let mut rows = Vec::new();
    for op in ops {
      rows.push(self.apply(op, &mut tree));
    }
    let revision = self
      .index
      .append(
        RevisionRecord {
          transaction: TRANSACTION.to_owned(),
          changes: rows,
        },
        usize::MAX,
      )
      .unwrap();
    self.trees.push(tree);
    revision
  }

  /// Applies one row to `tree` and answers it.
  fn apply(&mut self, op: &Op, tree: &mut BTreeMap<String, NaiveNode>) -> ChangedPath {
    let row = |path: &str, action, kind| ChangedPath {
      path: path.to_owned(),
      action,
      kind,
      copy_from: None,
      text: None,
      properties: None,
      mergeinfo_modified: false,
    };
    match op {
      Op::AddDir(path, props) | Op::AddFile(path, props) => {
        let kind = if matches!(op, Op::AddDir(..)) { NodeKind::Directory } else { NodeKind::File };
        let properties = props.as_ref().map(|props| self.list_for(props));
        let node = NaiveNode { kind, properties: properties.unwrap_or(PropertyList::Empty) };
        tree.insert(path.clone(), node);
        ChangedPath {
          properties,
          mergeinfo_modified: node.properties.has_mergeinfo(),
          ..row(path, Action::Add, kind)
        }
      }
      Op::Copy(path, from, revision, props) => {
        let source = self.tree(*revision).clone();
        let mut copied = source[from];
        let properties = props.as_ref().map(|props| self.list_for(props));
        let had = copied.properties.has_mergeinfo();
        if let Some(properties) = properties {
          copied.properties = properties;
        }
        for (place, node) in source.iter().filter(|(place, _)| at_or_beneath(place, from) && *place != from) {
          tree.insert(moved(place, from, path), *node);
        }
        tree.insert(path.clone(), copied);
        ChangedPath {
          copy_from: Some(CopySource { path: from.clone(), revision: *revision }),
          properties,
          mergeinfo_modified: properties.is_some() && (had || copied.properties.has_mergeinfo()),
          ..row(path, Action::Add, copied.kind)
        }
      }
      Op::Delete(path) => {
        let kind = tree[path].kind;
        tree.retain(|place, _| !at_or_beneath(place, path));
        row(path, Action::Delete, kind)
      }
      Op::SetProps(path, props) => {
        let properties = self.list_for(props);
        let node = tree.get_mut(path).unwrap();
        let had = node.properties.has_mergeinfo();
        node.properties = properties;
        ChangedPath {
          properties: Some(properties),
          mergeinfo_modified: had || properties.has_mergeinfo(),
          ..row(path, Action::Modify, node.kind)
        }
      }
    }
  }

  /// The model's mergeinfo of `path` in `revision`, by the rule stated on whole trees: absent paths refused; the
  /// nearest node holding `svn:mergeinfo` from the path (or its parent) up, the path alone when explicit; an ancestor's
  /// inheritable part extended by the path beneath it; a value that does not parse is none.
  fn mergeinfo(&self, path: &str, revision: Revision, inheritance: MergeinfoInheritance) -> Option<Option<Mergeinfo>> {
    let tree = self.tree(revision);
    tree.get(path)?;
    let mut chain = vec![(path.to_owned(), String::new())];
    let mut place = path.to_owned();
    while place != "/" {
      let parent = parent_of(&place).to_owned();
      let relative = if parent == "/" { path[1..].to_owned() } else { path[parent.len() + 1..].to_owned() };
      chain.push((parent.clone(), relative));
      place = parent;
    }
    let skip = usize::from(inheritance == MergeinfoInheritance::NearestAncestor);
    let take = if inheritance == MergeinfoInheritance::Explicit { 1 } else { chain.len() };
    for (place, relative) in chain.into_iter().skip(skip).take(take) {
      if let PropertyList::Held { identity, has_mergeinfo: true } = tree[&place].properties {
        let value = self.values[&identity].as_ref().unwrap();
        let read = Mergeinfo::parse(value).ok();
        return Some(read.map(|mergeinfo| if relative.is_empty() { mergeinfo } else { mergeinfo.inheritable_part().with_suffix(&relative) }));
      }
    }
    Some(None)
  }

  /// The model's mergeinfo beneath `path` in `revision`: every node strictly beneath holding a value that parses, in
  /// `svn_path_compare_paths` order.
  fn beneath(&self, path: &str, revision: Revision) -> Vec<(String, Mergeinfo)> {
    let mut found: Vec<(String, Mergeinfo)> = self
      .tree(revision)
      .iter()
      .filter(|(place, _)| *place != path && at_or_beneath(place, path))
      .filter_map(|(place, node)| match node.properties {
        PropertyList::Held { identity, has_mergeinfo: true } => {
          Mergeinfo::parse(self.values[&identity].as_ref().unwrap()).ok().map(|mergeinfo| (place.clone(), mergeinfo))
        }
        _ => None,
      })
      .collect();
    found.sort_by(|left, right| path_order(&left.0, &right.0));
    found
  }

  /// Whether a node strictly beneath `directory` in `revision` holds `svn:mergeinfo`.
  fn holds_beneath(&self, directory: &str, revision: Revision) -> bool {
    self
      .tree(revision)
      .iter()
      .any(|(place, node)| place != directory && at_or_beneath(place, directory) && node.properties.has_mergeinfo())
  }
}

/// `svn_path_compare_paths` over two paths.
fn path_order(left: &str, right: &str) -> Ordering {
  SourcePath(left.as_bytes().into()).cmp(&SourcePath(right.as_bytes().into()))
}

impl Contents for Naive {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    self.listed.borrow_mut().push((directory.to_owned(), revision));
    let tree = self.trees.get(usize::try_from(revision).ok()?)?;
    if tree.get(directory)?.kind != NodeKind::Directory {
      return None;
    }
    Some(
      tree
        .keys()
        .filter(|place| *place != directory && parent_of(place) == directory)
        .map(|place| place.rsplit('/').next().unwrap().to_owned())
        .collect(),
    )
  }

  fn mergeinfo_value(&self, properties: &ContentIdentity) -> Option<&[u8]> {
    self.values.get(properties)?.as_deref()
  }
}

/// A `svn:mergeinfo` value.
fn with(value: &str) -> Option<Props> {
  Some(Props::With(value.as_bytes().to_vec()))
}

/// A small repository: `/trunk` with mergeinfo holding a non-inheritable range, a branch copied from it, a file whose
/// value does not parse, and a directory whose only mergeinfo is beneath it.
fn small_repository() -> Naive {
  let mut naive = Naive::new();
  naive.commit(&[
    Op::AddDir("/trunk".into(), None),
    Op::AddDir("/branches".into(), None),
    Op::AddDir("/quiet".into(), None),
  ]);
  naive.commit(&[Op::AddDir("/trunk/a".into(), None), Op::AddFile("/quiet/f".into(), None)]);
  naive.commit(&[Op::AddFile("/trunk/a/f".into(), None), Op::SetProps("/trunk".into(), Props::With(b"/src:1-5,7*".to_vec()))]);
  naive.commit(&[Op::Copy("/branches/b".into(), "/trunk".into(), 3, None)]);
  naive.commit(&[Op::SetProps("/trunk/a/f".into(), Props::With(b"garbage".to_vec()))]);
  naive.commit(&[Op::AddDir("/branches/c".into(), None)]);
  naive.commit(&[Op::AddFile("/branches/c/g".into(), with("/x:9"))]);
  naive
}

/// The mergeinfo the query answers, panicking on a refusal.
fn answered(naive: &Naive, path: &str, revision: Revision, inheritance: MergeinfoInheritance) -> Option<Vec<u8>> {
  mergeinfo_at(&naive.index, naive, path, revision, inheritance).unwrap().map(|mergeinfo| mergeinfo.to_bytes())
}

/// A-605: A path's own mergeinfo, an ancestor's as the path inherits it (its `*` ranges dropped, its sources extended), and a
/// nearest ancestor's: each as FSFS answers, through a copied branch as through its source.
#[test]
fn a_path_inherits_its_nearest_ancestors_mergeinfo_extended_without_its_partial_ranges() {
  use MergeinfoInheritance as I;
  let naive = small_repository();
  assert_eq!(answered(&naive, "/trunk", 3, I::Explicit), Some(b"/src:1-5,7*".to_vec()));
  assert_eq!(answered(&naive, "/trunk/a/f", 3, I::Explicit), None);
  assert_eq!(answered(&naive, "/trunk/a/f", 3, I::Inherited), Some(b"/src/a/f:1-5".to_vec()));
  assert_eq!(answered(&naive, "/trunk", 3, I::NearestAncestor), None);
  assert_eq!(answered(&naive, "/trunk/a", 3, I::NearestAncestor), Some(b"/src/a:1-5".to_vec()));
  assert_eq!(answered(&naive, "/branches/b", 4, I::Explicit), Some(b"/src:1-5,7*".to_vec()));
  assert_eq!(answered(&naive, "/branches/b/a/f", 4, I::Inherited), Some(b"/src/a/f:1-5".to_vec()));
  assert_eq!(answered(&naive, "/", 4, I::NearestAncestor), None);
  assert_eq!(answered(&naive, "/trunk/a", 2, I::Inherited), None);
}

/// A-605: Issue #3896: a node whose value does not parse answers no mergeinfo, and an inherited search stops at it rather than
/// going on to `/trunk`'s; its nearest ancestor's is still found.
#[test]
fn a_value_that_does_not_parse_is_no_mergeinfo_and_ends_the_search() {
  use MergeinfoInheritance as I;
  let naive = small_repository();
  assert_eq!(answered(&naive, "/trunk/a/f", 5, I::Explicit), None);
  assert_eq!(answered(&naive, "/trunk/a/f", 5, I::Inherited), None);
  assert_eq!(answered(&naive, "/trunk/a/f", 5, I::NearestAncestor), Some(b"/src/a/f:1-5".to_vec()));
  assert_eq!(answered(&naive, "/branches/b/a/f", 5, I::Inherited), Some(b"/src/a/f:1-5".to_vec()));
}

/// A-605: The crawl finds the nodes beneath with mergeinfo, a value that does not parse skipped, in path order; it lists only
/// directories with mergeinfo beneath them, so `/quiet` and `/trunk/a` before revision 5 are never listed.
#[test]
fn descendants_are_found_by_the_counts_without_entering_quiet_directories() {
  let naive = small_repository();
  let found: Vec<(String, Vec<u8>)> = descendants(&naive.index, &naive, "/", 7)
    .unwrap()
    .map(|found| found.map(|(path, mergeinfo)| (path, mergeinfo.to_bytes())))
    .collect::<Result<_, _>>()
    .unwrap();
  assert_eq!(
    found,
    vec![
      ("/branches/b".to_owned(), b"/src:1-5,7*".to_vec()),
      ("/branches/c/g".to_owned(), b"/x:9".to_vec()),
      ("/trunk".to_owned(), b"/src:1-5,7*".to_vec()),
    ]
  );
  let listed: Vec<String> = naive.listed.borrow().iter().map(|(path, _)| path.clone()).collect();
  assert_eq!(listed, vec!["/", "/branches", "/branches/c", "/trunk", "/trunk/a"]);
  naive.listed.borrow_mut().clear();
  let at_four: Vec<_> = descendants(&naive.index, &naive, "/", 4).unwrap().collect();
  assert_eq!(at_four.len(), 2);
  let listed: Vec<String> = naive.listed.borrow().iter().map(|(path, _)| path.clone()).collect();
  assert_eq!(listed, vec!["/", "/branches"]);
}

/// A-605: `get-mergeinfo`'s answer for two paths: each path's own or inherited mergeinfo, then what lies beneath it.
#[test]
fn a_catalog_answers_each_path_then_its_descendants() {
  let naive = small_repository();
  let answer = catalog(&naive.index, &naive, &["/branches", "/trunk/a"], 7, MergeinfoInheritance::Inherited, true).unwrap();
  let answer: Vec<(String, Vec<u8>)> = answer.into_iter().map(|(path, mergeinfo)| (path, mergeinfo.to_bytes())).collect();
  assert_eq!(
    answer,
    vec![
      ("/branches/b".to_owned(), b"/src:1-5,7*".to_vec()),
      ("/branches/c/g".to_owned(), b"/x:9".to_vec()),
      ("/trunk/a".to_owned(), b"/src/a:1-5".to_vec()),
    ]
  );
}

/// A-605: The queries' refusals, each with its Subversion error: an absent path (`E160013`), a path out of canonical form
/// (`E160005`), a revision past the youngest (`E160006`), a row claiming mergeinfo its list lacks (`E160004`), and a
/// green that cannot list a directory the index says holds mergeinfo beneath (`E160004`).
#[test]
fn the_queries_refuse_by_kind() {
  use MergeinfoInheritance as I;
  let naive = small_repository();
  let refused = |path, revision| mergeinfo_at(&naive.index, &naive, path, revision, I::Inherited).unwrap_err();
  assert_eq!(refused("/nothing", 3).subversion_code(), 160_013);
  assert_eq!(refused("trunk", 3).subversion_code(), 160_005);
  assert_eq!(refused("/trunk", 99).subversion_code(), 160_006);
  assert!(descendants(&naive.index, &naive, "/nothing", 3).is_err());

  /// A green that lost every list and every directory.
  #[derive(Debug)]
  struct Lost;
  impl Contents for Lost {
    fn entries(&self, _: &str, _: Revision) -> Option<Vec<String>> {
      None
    }
    fn mergeinfo_value(&self, _: &ContentIdentity) -> Option<&[u8]> {
      None
    }
  }
  let claims = mergeinfo_at(&naive.index, &Lost, "/trunk", 3, I::Explicit).unwrap_err();
  assert_eq!(claims, QueryRefusal::ClaimsMergeinfo { path: "/trunk".into(), revision: 3 });
  assert_eq!(claims.subversion_code(), 160_004);
  let lost = descendants(&naive.index, &Lost, "/", 3).unwrap_err();
  assert_eq!(lost, QueryRefusal::ContentsDisagree { path: "/".into(), revision: 3 });
  assert_eq!(lost.subversion_code(), 160_004);
}

// ---------------------------------------------------------------------------------------------------------------------
// The oracle: generated histories.

/// Names the generator places, `a` and `a-b` among them so path order and byte order differ.
const NAMES: [&str; 4] = ["a", "b", "a-b", "c"];

/// Values the generator sets: valid, empty, all non-inheritable, several sources, and three that do not parse.
const VALUES: [&[u8]; 8] = [
  b"/trunk:1-5",
  b"/trunk:3,7*\n/branch:2-4",
  b"",
  b"/trunk:5*",
  b"/a/b:1,3-9\n/c:2*",
  b"garbage",
  b"/trunk:0",
  b"/x:4-2",
];

/// The deepest a generated addition goes.
const DEPTH: usize = 4;

/// The most paths a generated copy takes, so trees stay small enough to compare whole after every revision.
const COPY_CAP: usize = 12;

/// One generated wish; the history turns it into a row when the tree allows one.
#[derive(Clone, Copy, Debug)]
struct Wish {
  /// Which operation.
  op: u8,
  /// Which place.
  place: u16,
  /// Which source.
  source: u16,
  /// Which earlier revision.
  from: u16,
  /// Which name.
  name: u8,
  /// Which property list.
  props: u8,
}

/// Any wish.
fn wish() -> impl Strategy<Value = Wish> {
  (any::<u8>(), any::<u16>(), any::<u16>(), any::<u16>(), any::<u8>(), any::<u8>())
    .prop_map(|(op, place, source, from, name, props)| Wish { op, place, source, from, name, props })
}

/// The property list a wish sets: none (a quarter), empty, without mergeinfo, or one of the values.
fn wished_props(wish: &Wish) -> Option<Props> {
  match wish.props % 13 {
    0..=2 => None,
    3 => Some(Props::Empty),
    4 => Some(Props::Without),
    pick => Some(Props::With(VALUES[usize::from(pick - 5) % VALUES.len()].to_vec())),
  }
}

/// The `pick`-th of `choices`.
fn pick<T: Clone>(choices: &[T], pick: u16) -> Option<T> {
  (!choices.is_empty()).then(|| choices[usize::from(pick) % choices.len()].clone())
}

/// Whether `path` overlaps a path some row of this revision names.
fn overlaps(path: &str, taken: &[String]) -> bool {
  taken.iter().any(|other| at_or_beneath(path, other) || at_or_beneath(other, path))
}

/// The row a wish makes against the youngest tree, untouched by the other rows of its revision, or none.
fn op_for(naive: &Naive, wish: &Wish, taken: &[String]) -> Option<Op> {
  let head = naive.head();
  let free: Vec<&String> = head.keys().filter(|path| *path != "/" && !overlaps(path, taken)).collect();
  match wish.op % 6 {
    0 | 1 | 2 => {
      let directories: Vec<&String> = head
        .iter()
        .filter(|(path, node)| node.kind == NodeKind::Directory && path.matches('/').count() < DEPTH && !taken.iter().any(|other| at_or_beneath(path, other)))
        .map(|(path, _)| path)
        .collect();
      let parent = pick(&directories, wish.place)?;
      let name = NAMES[usize::from(wish.name) % NAMES.len()];
      let path = if parent == "/" { format!("/{name}") } else { format!("{parent}/{name}") };
      if head.contains_key(&path) || overlaps(&path, taken) {
        return None;
      }
      match wish.op % 6 {
        0 => Some(Op::AddDir(path, wished_props(wish))),
        1 => Some(Op::AddFile(path, wished_props(wish))),
        _ => {
          let revision = u64::from(wish.from) % u64::try_from(naive.trees.len()).unwrap();
          let tree = naive.tree(revision);
          let sources: Vec<String> = tree
            .keys()
            .filter(|source| tree.keys().filter(|place| at_or_beneath(place, source)).count() <= COPY_CAP)
            .cloned()
            .collect();
          let source = pick(&sources, wish.source)?;
          Some(Op::Copy(path, source, revision, wished_props(wish)))
        }
      }
    }
    3 => pick(&free, wish.place).map(|path| Op::Delete(path.clone())),
    _ => {
      let props = wished_props(wish).unwrap_or(Props::Without);
      let places: Vec<&String> = head.keys().filter(|path| !overlaps(path, taken)).collect();
      pick(&places, wish.place).map(|path| Op::SetProps(path.clone(), props))
    }
  }
}

/// The path an op names.
fn op_path(op: &Op) -> &str {
  match op {
    Op::AddDir(path, _) | Op::AddFile(path, _) | Op::Copy(path, ..) | Op::Delete(path) | Op::SetProps(path, _) => path,
  }
}

/// How often the generated histories met each case, so the oracle cannot pass on histories that never reach one.
#[derive(Debug, Default)]
struct Census {
  /// Answers inherited from an ancestor.
  inherited: usize,
  /// Answers from a holder whose value does not parse.
  unparsed: usize,
  /// Descendant answers that found something.
  found_beneath: usize,
  /// Directories the crawl listed.
  listings: usize,
  /// Directories the crawl skipped by the counts.
  pruned: usize,
}

/// The queries against the naive filesystem in one revision, every path and mode, and absent paths refused.
fn same_queries(naive: &Naive, revision: Revision, census: &mut Census) -> Result<(), TestCaseError> {
  use MergeinfoInheritance as I;
  for path in naive.tree(revision).keys() {
    for inheritance in [I::Explicit, I::Inherited, I::NearestAncestor] {
      let expected = naive.mergeinfo(path, revision, inheritance).unwrap();
      let actual = mergeinfo_at(&naive.index, naive, path, revision, inheritance)
        .map_err(|refusal| TestCaseError::fail(format!("{path}@{revision}: {refusal:?}")))?;
      prop_assert_eq!(&actual, &expected, "{}@{} {:?}", path, revision, inheritance);
      let holder_unparsed = naive.mergeinfo(path, revision, I::Inherited) == Some(None)
        && naive.tree(revision)[path].properties.has_mergeinfo();
      census.unparsed += usize::from(holder_unparsed && inheritance == I::Inherited);
      census.inherited += usize::from(inheritance == I::NearestAncestor && actual.is_some());
    }
    naive.listed.borrow_mut().clear();
    let actual: Vec<(String, Mergeinfo)> = descendants(&naive.index, naive, path, revision)
      .and_then(|crawl| crawl.collect::<Result<_, _>>())
      .map_err(|refusal| TestCaseError::fail(format!("{path}@{revision}: {refusal:?}")))?;
    let expected = naive.beneath(path, revision);
    census.found_beneath += usize::from(!expected.is_empty());
    prop_assert_eq!(actual, expected, "beneath {}@{}", path, revision);
    for (listed, at) in naive.listed.borrow().iter() {
      prop_assert!(naive.holds_beneath(listed, *at), "listed {} at {} with nothing beneath", listed, at);
      census.listings += 1;
    }
    let directory = naive.tree(revision)[path].kind == NodeKind::Directory;
    census.pruned += usize::from(directory && naive.listed.borrow().is_empty());
  }
  let absent = mergeinfo_at(&naive.index, naive, "/absent", revision, I::Inherited);
  prop_assert_eq!(absent.map_err(|refusal| refusal.subversion_code()), Err(160_013));
  Ok(())
}

/// A-605: The oracle: on generated histories of additions, copies (of directories holding mergeinfo and beneath it),
/// deletions and property changes, every path's explicit, inherited and nearest-ancestor mergeinfo and every
/// descendants crawl, answered from the index and the green, equal what whole trees say; the crawl lists only
/// directories with mergeinfo beneath. Non-vacuity: the histories reach inherited answers, unparsed values, found
/// descendants, listings and pruned directories.
#[test]
fn the_queries_equal_a_naive_filesystem_on_generated_histories() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config::default()));
  let strategy = prop::collection::vec(prop::collection::vec(wish(), 1..4), 1..12);
  runner
    .run(&strategy, |history| {
      let mut naive = Naive::new();
      for wishes in &history {
        let mut ops: Vec<Op> = Vec::new();
        for wish in wishes {
          let taken: Vec<String> = ops.iter().map(|op| op_path(op).to_owned()).collect();
          if let Some(op) = op_for(&naive, wish, &taken) {
            ops.push(op);
          }
        }
        if ops.is_empty() {
          continue;
        }
        let revision = naive.commit(&ops);
        same_queries(&naive, revision, &mut census.borrow_mut())?;
      }
      Ok(())
    })
    .unwrap();
  let census = census.into_inner();
  assert!(census.inherited > 0, "{census:?}");
  assert!(census.unparsed > 0, "{census:?}");
  assert!(census.found_beneath > 0, "{census:?}");
  assert!(census.listings > 0, "{census:?}");
  assert!(census.pruned > 0, "{census:?}");
}
