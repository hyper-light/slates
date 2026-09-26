# A shard step cost its whole ready set

Date: 2026-09-26. Contracts: §4.3 (a step's work is bounded by the batch; "bounded work everywhere",
CLAUDE.md §3). Found 2026-09-25 while measuring A-31 (recorded in
`2026-09-25-wake-estimate-frozen-at-boot-and-preemptions-counted-as-long-steps.md`, found 2).

## Symptom

`crates/server/tests/observe.rs`, whose full-arena fill admits 82,245 yielding tasks on this Mac, ran
95–97 s (measured 2026-09-25, on the A-31 commit and on the HEAD before it).

## Root cause

`LocalQueue::take_ready` swapped out the whole ready list. `Shard::step` polled the first `batch` of
it and re-queued every other slot (clearing and re-setting each pending flag). So a step cost
O(ready), and draining N ready tasks cost O(N² / batch). The re-queued remainder also landed *behind*
tasks woken during the same step, so the order was not FIFO either.

## Fix

- `take_ready(limit)` takes at most `limit` slots from the front of a `VecDeque`. The rest stay queued
  in order with their pending flags set, so a repeated wake for one of them still collapses.
- `Shard::step` polls what it was given.
- A step now costs its batch, and the order is strict FIFO: tasks left behind keep their places ahead
  of wakes that arrive while the batch runs.

## Tests and measurements

- `a_drain_takes_at_most_its_batch_and_the_rest_keep_their_places` (the queue's contract: at most the
  batch, oldest first, the left-behind ahead of new wakes, a repeated wake collapsed).
- The observe suite on this Mac (Apple silicon, 18 CPUs, 2026-09-26): 12.64 s and 12.67 s, where
  95–97 s was measured before. This is recorded as a measurement, not asserted by a test.
- On macOS the runtime suite passes. So do the server lib (105), daemon (14), recovery (6), NFS mount
  (9), client and fleet (49 of 49, 233 s) suites.

## Edits

- `crates/rt/src/{queue,shard}.rs`, `docs/wip/TBD_FIXES.md`.
