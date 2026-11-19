use super::{Filter, Refusal, check_served};
use crate::object_name::Kind;

/// A-615 part 2 (`list-objects-filter-options.c` `gently_parse_list_objects_filter`; rev-list-options "--filter"): do
/// parse every spec git reads, units included, and a combination with percent-encoded sub-specs. Expect: each filter, a
/// unit multiplying by 2^10, 2^20 or 2^30, the combination's sub-specs decoded, and `sparse:oid` read but not served.
#[test]
fn every_spec_git_reads_parses_to_its_filter() {
  let cases: [(&[u8], Filter); 13] = [
    (b"blob:none", Filter::BlobNone),
    (b"blob:limit=0", Filter::BlobLimit(0)),
    (b"blob:limit=1024", Filter::BlobLimit(1024)),
    (b"blob:limit=1k", Filter::BlobLimit(1 << 10)),
    (b"blob:limit=2M", Filter::BlobLimit(2 << 20)),
    (b"blob:limit=3g", Filter::BlobLimit(3 << 30)),
    (b"tree:0", Filter::TreeDepth(0)),
    (b"tree:3", Filter::TreeDepth(3)),
    (b"object:type=commit", Filter::ObjectType(Kind::Commit)),
    (b"object:type=blob", Filter::ObjectType(Kind::Blob)),
    (
      b"combine:blob:none+tree:2",
      Filter::Combine(vec![Filter::BlobNone, Filter::TreeDepth(2)]),
    ),
    (
      b"combine:blob%3Alimit%3D10+object%3atype%3dtree",
      Filter::Combine(vec![Filter::BlobLimit(10), Filter::ObjectType(Kind::Tree)]),
    ),
    (
      b"sparse:oid=main:.sparse",
      Filter::SparseOid(b"main:.sparse".to_vec()),
    ),
  ];
  for (spec, filter) in cases {
    assert_eq!(Filter::parse(spec), Ok(filter), "{spec:?}");
  }
  assert_eq!(
    check_served(&Filter::parse(b"sparse:oid=abc").unwrap()),
    Err(Refusal::SparseOid)
  );
  assert_eq!(
    check_served(&Filter::parse(b"combine:blob:none+sparse:oid=abc").unwrap()),
    Err(Refusal::SparseOid)
  );
  assert_eq!(
    check_served(&Filter::parse(b"combine:blob:none+tree:1").unwrap()),
    Ok(())
  );
}

/// A-615 part 2: do parse specs git refuses, and the forms git reads that no client writes. Expect: each refused by
/// kind: an unknown spec, a number not canonical decimal or past 64 bits, a unit git does not know, a type git does not
/// know, an empty combination or sub-spec, a reserved byte in a sub-spec, a nested combination, and `sparse:path`.
#[test]
fn a_spec_out_of_its_grammar_is_refused_by_kind() {
  let cases: [(&[u8], Refusal); 17] = [
    (b"blob:all", Refusal::Unknown),
    (b"", Refusal::Unknown),
    (b"tree", Refusal::Unknown),
    (b"blob:limit=", Refusal::Number),
    (b"blob:limit=-1", Refusal::Number),
    (b"blob:limit=0x10", Refusal::Number),
    (b"blob:limit=010", Refusal::Number),
    (b"blob:limit=1t", Refusal::Number),
    (b"blob:limit=18446744073709551616", Refusal::Number),
    (b"blob:limit=17179869184g", Refusal::Number),
    (b"tree:-1", Refusal::Number),
    (b"object:type=ofs-delta", Refusal::Type),
    (b"combine:", Refusal::Empty),
    (b"combine:blob:none+", Refusal::Empty),
    (b"combine:blob:none+tree:1~", Refusal::Reserved),
    (b"combine:combine%3Ablob%3Anone", Refusal::NestedCombine),
    (b"sparse:path=/x", Refusal::SparsePath),
  ];
  for (spec, refusal) in cases {
    assert_eq!(Filter::parse(spec), Err(refusal), "{spec:?}");
  }
}

/// A-615 part 2 (`list-objects-filter.c`): do ask each filter of each kind, size and depth. Expect: commits and tags
/// always sent but under `object:type`; `blob:limit` sending blobs smaller than its limit; `tree:` sending trees and
/// blobs less deep than its depth; a combination what all send; and only the filters with no depth deciding by row.
#[test]
fn each_filter_admits_as_git_filters() {
  let limit = Filter::BlobLimit(10);
  assert!(limit.admits(Kind::Blob, 9, 7) && !limit.admits(Kind::Blob, 10, 0));
  assert!(limit.admits(Kind::Tree, 1000, 0) && limit.admits(Kind::Commit, 0, 0));
  let none = Filter::BlobNone;
  assert!(!none.admits(Kind::Blob, 0, 0) && none.admits(Kind::Tree, 0, 9));
  let depth = Filter::TreeDepth(2);
  assert!(depth.admits(Kind::Tree, 0, 1) && !depth.admits(Kind::Tree, 0, 2));
  assert!(depth.admits(Kind::Blob, 0, 1) && !depth.admits(Kind::Blob, 0, 2));
  assert!(depth.admits(Kind::Commit, 0, 99) && depth.admits(Kind::Tag, 0, 99));
  assert!(!Filter::TreeDepth(0).admits(Kind::Tree, 0, 0));
  let trees = Filter::ObjectType(Kind::Tree);
  assert!(
    trees.admits(Kind::Tree, 0, 5)
      && !trees.admits(Kind::Commit, 0, 0)
      && !trees.admits(Kind::Blob, 0, 0)
  );
  let both = Filter::Combine(vec![Filter::BlobLimit(5), Filter::TreeDepth(2)]);
  assert!(
    both.admits(Kind::Blob, 4, 1)
      && !both.admits(Kind::Blob, 4, 2)
      && !both.admits(Kind::Blob, 5, 1)
  );
  assert!(none.is_per_object() && limit.is_per_object() && trees.is_per_object());
  assert!(!depth.is_per_object() && !both.is_per_object());
  assert!(depth.reaches(1) && !depth.reaches(2));
  assert!(!Filter::ObjectType(Kind::Commit).reaches(0) && trees.reaches(100));
}
