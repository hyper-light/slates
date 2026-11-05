# The increment identity left out its work volume and base

Found 2026-11-04 by the many-client load test at ten clients over Linux v7.4-rc1's `kernel/` directory: 448 submits
were refused a conflict with nothing accepted after their base, and seven of ten clients starved. Design: §4.16 data
model (`id: blake3(work_volume, base, post_state, ops_doc, filter)`) and "Rebase": "A rebased increment has a new
identity; deduplication never blocks it."

## Description

A client whose submit conflicted resolved as §4.16 says (read the green's bytes of the window, write them over the
window, rebase, write its own bytes again) and submitted with its base equal to the head. The submit was answered with
the first submit's conflict, every time.

## Root cause

`build_increment` in `crates/server/src/verbs.rs` hashed only the ops document and the post-state. The resubmit's ops
and post-state equal the first submit's (only the base moved), so it had the same identity, and the engine's
rejected-result cache (AUD-16), keyed by that identity, answered it without judging. Two different works declaring the
same change had one identity too: the second was answered from the first's idempotency record with the first's
version.

## Impact

Every client that resolved a conflict by the design's corrective path and wrote the same bytes again was refused for
as long as the cached result lived; under load that is a livelock. A second work's identical change was reported as
accepted at a version it did not make.

## Edits

- `crates/server/src/verbs.rs`: `increment_identity(work, base, doc, post_state)`, every field fixed-width or
  length-prefixed. The identity is carried in the recorded increment, so recovery and holders replay recorded values
  unchanged.
- `crates/server/tests/merge_load.rs`: `a_conflict_resolved_by_rebase_then_resubmitted_is_accepted` (was
  `Conflict([f, 128, 64])`, now `Accepted(3)`) and `two_works_declaring_the_same_change_are_two_increments` (was
  `Accepted(2)`, now `Accepted(3)`). Both failed against the old identity and pass after.
