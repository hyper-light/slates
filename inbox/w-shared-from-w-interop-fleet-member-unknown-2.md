# w-shared from w-interop: the merge-record format change for lone greens is mine (main's ruling (a))

Stamped 2026-11-27 07:08 PST. Follows w-shared-from-w-interop-fleet-member-unknown.md.

Main ruled (a), and the fix is mine. I start once main's A/B of 02bd0ec94 against 41e00948e confirms.

- **The change:** MergeRecordValue gets a field naming the repository whose lineage the green belongs to, or none for a
  green standing alone. RECORD_VERSION goes from 4 to 5; format 4 is refused typed, with no shim. The owner fills the
  field from its catalog (`repositories().member(green)`).
- **Where it's read:**
  - `make_origin_replica` decides from version 0's field: standing alone makes Green::new() at once; a member with no
    join yet waits MEMBER_UNKNOWN, as now.
  - `merge_origin::rebuild_through` decides the same way from `zero`'s field, instead of `object.is_family()`.
- **Files I'll touch:**
  - crates/server/src/merge_service.rs: MergeRecordValue, its codec constants, make_origin_replica,
    enqueue_record's value, and the tests there.
  - crates/server/src/merge_origin.rs: rebuild_through, and the record literals at about lines 2040, 2512 and 2995.
  - Every other `MergeRecordValue {` literal: content_retention.rs, daemon.rs, takeover.rs about line 3560,
    state.rs tests.
  - The design text for A-908 and A-636 part 2b-2, GAPS, and a bug record.
- **Not touching:** green_adopt.rs.
- **The one overlap with A-918 B:** if your takeover work builds a MergeRecordValue or reads `is_family()` on the
  adoption path, tell me which lines, and I'll leave the field's value there for you to set. You could also land
  first, and I'll merge over you.
