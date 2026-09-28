# Stream reassembly scanned every buffered segment per arrival (quadratic)

Date: 2026-09-28. Design: §4.10a (the session plane's ordered streams, `crates/transport/src/stream.rs`).
Found by the focal cross-check, a parallel study of the same transport questions on quinn.

## Root cause

`StreamAssembler::offer` found the buffered segments an arriving one overlaps by iterating every buffered
segment. A long, lossy, high-bandwidth path keeps a large window with many holes, so up to thousands of
segments can be buffered at once. Each arrival then cost time in proportion to all of them, and a window's
worth of arrivals cost quadratic time.

## Fix

Buffered segments are disjoint and keyed by their start, so the only candidates are:
- the last segment that starts before the arrival;
- the segments that start inside the arrival's range.

Two `BTreeMap::range` lookups give O(log n + overlaps). A `segments_examined` counter witnesses it. The offset
arithmetic there is now saturating.

## Tests

- `reassembly_with_many_holes_examines_only_neighbouring_segments`: 4,000 segments arrive with a hole before
  each, so up to 2,000 are buffered. The stream reassembles, and at most 16,000 segments are examined. With
  the old full scan restored the test fails: 4,000,000 examined.
- `overlapping_slices_reassemble_exactly` (proptest): arbitrary partially overlapping, duplicated, reordered
  slices reassemble to exactly the covered prefix, keeping the first copy of every byte.
