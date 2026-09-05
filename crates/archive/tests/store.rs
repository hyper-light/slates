//! Tests for the content-addressed store with deduplication (Phase 7 task 1; T-7.1). Identical
//! chunks fold to one stored copy referenced many times; the accounting reports the deduplicated
//! `unique_bytes` and the undeduplicated `referenced_bytes`; references evict the chunk on the
//! last release. A non-vacuity check asserts that deduplication actually happened, so a store that
//! silently kept duplicates could not pass.

use std::collections::BTreeMap;

use slates_archive::archive::Archive;
use slates_archive::store::ContentStore;

use proptest::prelude::*;

/// Inserting the same chunk twice stores it once, referenced twice; the accounting reflects the
/// deduplication.
#[test]
fn identical_chunks_deduplicate() {
  let chunk = Archive::raw_chunk(b"a repeated chunk of bytes".to_vec());
  let raw_len = chunk.raw_len;
  let mut store = ContentStore::new();
  let first = store.insert(chunk.clone());
  let second = store.insert(chunk);
  assert_eq!(first, second, "identical bytes share one identity");
  assert_eq!(store.unique_count(), 1, "stored once");
  assert_eq!(store.references(&first), 2, "referenced twice");
  assert_eq!(
    store.unique_bytes(),
    raw_len,
    "deduplicated bytes counted once"
  );
  assert_eq!(
    store.referenced_bytes(),
    raw_len * 2,
    "references counted twice"
  );
  assert_eq!(store.total_references(), 2);
}

/// The worked example: two clones each with the same `target/` output dedup that output to one
/// copy; `unique_bytes` counts the distinct chunks, `referenced_bytes` counts every reference.
#[test]
fn two_clones_share_an_identical_output() {
  let source_a = Archive::raw_chunk(b"clone A source file".to_vec());
  let source_b = Archive::raw_chunk(b"clone B source file (different)".to_vec());
  let target = Archive::raw_chunk(b"the identical build output".to_vec());
  let mut store = ContentStore::new();
  // Clone A: its source and the output.
  store.insert(source_a.clone());
  store.insert(target.clone());
  // Clone B: a different source and the same output.
  store.insert(source_b.clone());
  store.insert(target.clone());
  assert_eq!(store.unique_count(), 3, "two sources and one shared output");
  assert_eq!(
    store.references(&target.identity),
    2,
    "the output is shared"
  );
  let expected_unique = source_a.stored_len + source_b.stored_len + target.stored_len;
  assert_eq!(store.unique_bytes(), expected_unique);
  let expected_referenced = source_a.raw_len + source_b.raw_len + target.raw_len + target.raw_len;
  assert_eq!(store.referenced_bytes(), expected_referenced);
}

/// A chunk is evicted only when its last reference is released.
#[test]
fn a_chunk_is_evicted_on_the_last_release() {
  let chunk = Archive::raw_chunk(b"held twice".to_vec());
  let mut store = ContentStore::new();
  let id = store.insert(chunk.clone());
  store.insert(chunk);
  assert!(!store.release(&id), "still one reference");
  assert!(store.contains(&id));
  assert!(store.release(&id), "the last reference evicts it");
  assert!(!store.contains(&id));
  assert!(
    !store.release(&id),
    "releasing an absent chunk does nothing"
  );
}

proptest! {
  /// T-7.1 (property): for any chunks with duplicates, the store reads back byte-exact and its
  /// accounting is exact — distinct chunks stored once, references summed. The non-vacuity check:
  /// when duplicates were inserted, the unique count is below the insert count (dedup happened).
  #[test]
  fn insertions_deduplicate_with_exact_accounting(
    payloads in proptest::collection::vec(
      proptest::collection::vec(any::<u8>(), 0..24),
      1..40,
    ),
    // Which earlier payload each insertion duplicates (or itself).
    dups in proptest::collection::vec(0usize..40, 1..40),
  ) {
    let mut store = ContentStore::new();
    // A byte-level model: identity -> (raw_len, references).
    let mut model: BTreeMap<[u8; 32], (u64, u64)> = BTreeMap::new();
    let mut inserted = 0u64;
    for (index, payload) in payloads.iter().enumerate() {
      // Insert the payload, and if `dups` says so, insert an identical duplicate.
      let repeats = 1 + usize::from(dups.get(index).is_some_and(|d| d % 2 == 0));
      for _ in 0..repeats {
        let chunk = Archive::raw_chunk(payload.clone());
        let id = store.insert(chunk.clone());
        let entry = model.entry(id).or_insert((chunk.raw_len, 0));
        entry.1 += 1;
        inserted += 1;
      }
    }
    // Byte-exact reads.
    for (id, (raw_len, references)) in &model {
      let got = store.get(id).expect("a stored chunk reads back");
      prop_assert_eq!(got.raw_len, *raw_len);
      prop_assert_eq!(store.references(id), *references);
    }
    // Exact accounting.
    prop_assert_eq!(store.unique_count(), model.len());
    prop_assert_eq!(store.total_references(), inserted);
    let referenced: u64 = model.values().map(|(raw, refs)| raw * refs).sum();
    prop_assert_eq!(store.referenced_bytes(), referenced);
    // Non-vacuity: if any identity was inserted more than once, dedup shrank the count.
    if inserted > store.unique_count() as u64 {
      prop_assert!((store.unique_count() as u64) < inserted, "deduplication happened");
    }
  }
}
