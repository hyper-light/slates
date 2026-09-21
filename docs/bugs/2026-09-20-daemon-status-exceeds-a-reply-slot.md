# A valid daemon status exceeds one reply slot

Date: 2026-09-20. Design: §4.7, §4.14 (status paging), AC-2.6.

## Reproduction and cause

[macOS job 106138990793](https://github.com/hyper-light/slates/actions/runs/35533762695/job/106138990793)
fails `a_fleet_node_under_a_containers_memory_bound_still_admits_a_client` on `2fb843f`:
the real client receives `BadRequest: reply too large for the bulk area` for `DaemonStatus`.
The admission assertion is valid; this is not a scheduler-dependent performance assertion.

The fixture fixed memory at 1 GiB but derived its budgets from the host's core count before
forcing one daemon shard. Consequently a larger host generated fewer peers per shard and
could hide the failure. Pinning the profile to the one core the fixture actually runs makes
the native macOS reproduction fail in 2.08 s. The typed packing diagnostic reports
`PayloadTooLarge { offered: 5888, capacity: 4096 }`.

Command: `cargo test --offline -p slates-server --test fleet
a_fleet_node_under_a_containers_memory_bound_still_admits_a_client -- --exact --nocapture`.
Log: `/private/tmp/slates-status-one-core-red.log`.

The report's member list grows with the admitted fleet, but the wire tried to pack the entire
report into the same single bulk chunk used by ordinary lifecycle replies. §4.14 already
records paging beyond one chunk as owed. A bigger constant or a smaller test fleet would
preserve the defect.

## Fix

Status uses pages of one immutable, schema-checked report. The retained bytes are charged
to the shard's metadata ledger. One client can hold one report, bounded by that client's
admitted reply-bulk credit; a report beyond that credit refuses before retention. Every page
fits its actual ring slot. The snapshot is owned by that client and request identity, and
ends on completion, another operation, the derived liveness deadline, or client removal.
Clients validate identity, offsets, total length and strict progress before assembly, then
return the existing complete `DaemonReport`. No observation is truncated or silently sampled.

## Validation

On 2026-09-20 the exact one-core fleet regression passes in **1.58 s**. The new
`status_pages_preserve_a_capture_and_refuse_foreign_or_cancelled_cursors` passes on
native macOS in **0.77 s**: a report crosses real reply slots, another client mutates the
daemon between pages, the assembled report retains its earlier view, cross-channel and
cancelled cursors refuse, and the public typed client returns the complete current report.

Three receiver tests cover every fragmentation width, over-credit/hostile lengths,
changed identity/total, gaps, overlap, no progress, wrong schema/verb and typed refusals.
Three retention tests cover exact allocation charging, completion, replacement,
cancellation, expiry, invalid cursors and admission refusal without leaking credit.
Commands: `cargo test --offline -p slates-ipc --test status` and
`cargo test --offline -p slates-server status_pages -- --nocapture`.

The approved four-CPU/4-GiB aarch64 Linux container with `SLATES_TEST_DRIVER=io_uring`
passes strict workspace Clippy, `cargo xtask check`, and `cargo test --offline --workspace`:
**1,547 passed, zero failed, 14 ignored**; all **49 fleet tests pass in 261.98 s**.
Log: `/private/tmp/slates-status-linux-workspace.log`. Native strict Clippy and xtask pass
as well. The complete native macOS workspace subsequently passes **1,538 tests, zero
failures, 14 ignored**, including all **48 fleet tests in 316.67 s**; log
`/private/tmp/slates-status-macos-workspace.log`. Remaining CI steps are tracked separately;
these workspace results do not claim every CI job is green.

## Sibling findings and limits

Other growing replies (`List`, grants, audit and some content reads) still have one-slot
packing limits; this repair only supplies the daemon-status paging already required by
§4.14. The status scatter still has no completion deadline if a return task is lost. Its
capture expires, but expiry does not complete that missing gather. Captures are transient
observations; a daemon restart invalidates the cursor and requires a new report. These
limits remain visible rather than being hidden by a larger slot or truncation.
