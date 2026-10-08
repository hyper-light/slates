# A memory-pressure hold kept a successor from serving a taken-over volume

**Found:** 2026-10-07, testing the leading hypothesis for an intermittent takeover failure (three fleet tests, about one
full suite in three: a successor adopted a head and never served the volume). **Status: fixed.** Whether this was
that flake's cause is not shown; the flake has not recurred since its counters landed.

## Description

The pressure hold (admission.md §5.5) withholds capacity from new admission while the host is short of memory. A
takeover's materialization charged the same budget as a new volume: the fetch staged the replica (`charge_replicated`),
the restore was capped at `admittable`, and the restored volume's writes grew it (`grow`). All of these are refused
under the hold.

So under host pressure a successor adopted the dead owner's head and never served the volume. It stayed unavailable
until the pressure lifted. With the refusal fix of the same day, its own clients were refused a retryable
`Overloaded` the whole time.

## Root cause

The admission rules already say a committed claim is outside the hold:
- §5.5: the hold "shrinks admittable only, never revoking a committed claim";
- §4e: "an admitted claim survives a restart ahead of any new claim". Boot recovery gets this for free, because its
  hold starts at zero (the baseline is the first sample).

A takeover is the fleet's recovery of a claim the fleet admitted and committed: its successor serves the dead owner's
volumes (§4.8). The materialization path never distinguished it from new admission.

## Impact

Under host memory pressure, every volume whose owner died was unavailable on its successor until the pressure
receded. The refusals were counted only as bare `fleet.materialize`.

## Fix

- `past_the_pressure_hold` (`crates/server/src/fleet.rs`) runs a takeover's synchronous recovery work with the hold
  lifted for exactly its duration, then sets it back:
  - the fetch's stage, each fetched chunk and the stage's completion;
  - the volume's materialization (restore, reservations, populating writes);
  - a green's materialization.
- This is exact because a shard runs one task at a time and each of these steps is synchronous. No other admission
  can run while the hold is lifted, and the pressure refresh is itself a message to that shard.
- Capacity and the operation headroom still bound the recovery.
- A refused stage is now counted as `fleet.fetch.stage_refused`.

## Tests

`a_pressure_hold_does_not_keep_a_successor_from_serving_a_taken_over_volume` (`crates/server/tests/fleet.rs`): seal a
volume, raise a hold of all admittable capacity on both survivors, kill the owner.
- Expected: the successor serves the volume, and the hold still refuses a new bounded volume there.
- Before the fix it failed after 488 s with 3,913 materialization refusals; after it, it passes in 16 s.

## Owed

- The successor answers its own client an immediate retryable refusal while it materializes. A client that retries
  without pause (the test's poll loop) was refused 2.8 million times in about 400 s. The better answer holds the reply
  until the volume is served, within a bounded wait, as a forwarded read already does (`defers_reply`).
