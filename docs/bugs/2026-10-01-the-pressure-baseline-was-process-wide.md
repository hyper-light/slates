# The memory-pressure baseline was process-wide

**Date:** 2026-10-01. **Area:** `slates-server` (`daemon.rs` `refresh_pressure_hold`, `inject_pressure_hold`;
`state.rs`), the churn test (`tests/fleet.rs`). **Found by:** the AUD-29-43 churn test's CI failures.
**Design:** §4.2, admission.md §5.5.

## Description

`a_holders_replicas_cap_at_its_unpromised_capacity_through_churn_and_retire_to_the_survivors_baseline` failed
on CI (macOS run 36851876814, and Ubuntu earlier) and passed alone locally.

**What the CI run's own numbers show:**
- the holder's peak charge was one seal (32,768 bytes) under a measured cap of three (98,304);
- a put was refused at capacity;
- no waiting seal took the room a destroy freed.

So the holder's admittable capacity shrank after the cap was measured. The test's kept samples were all
from the final baseline phase, so they could not show when.

**Two defects explain how capacity can shrink:**
1. **The baseline was process-wide.** The pressure sampler's baseline was a process-global static, fixed
   by whichever daemon in the process sampled first. Every later daemon measured its shortfall against
   it. In the fleet test binary, many in-process daemons run one after another for about 15 minutes. A
   daemon started after the binary had grown withheld that growth from admission as if it were pressure.
2. **An injected hold did not stay.** `Daemon::inject_pressure_hold` set the hold, and the sampler
   overwrote it at its next one-second cadence. Every test that drives the hold raced the sampler.

**Status of the cause.** The CI failure's own cause, a hold rising between the cap's measurement and the
seals, is consistent with these defects but not yet shown by a log. The churn test now samples the hold
per phase so a recurrence names it.

## Root cause

Process-local state (the baseline) was kept per process instead of per daemon, the owner of the
measurement. The test-support setter had no way to keep the sampler from undoing it.

## Fix

- **A per-daemon baseline.** The baseline is kept by the sampling shard (`ShardState::pressure_baseline`),
  so each daemon measures from its own first sample.
- **An injected hold is pinned.** `inject_pressure_hold` sets `pressure_pinned`, and the sampler leaves
  those shards' holds alone.
- **Test support:**
  - `inject_available_memory` replaces the platform reading for a daemon's sampler;
  - `pressure_baseline` and `pressure_hold` observe the result.
- **The churn test pins the holder's hold at zero** before measuring its cap. It also records, per phase,
  each sample's charge, admittable room, hold, refused puts, stages and manifests.

## Tests

- `recovery::two_daemons_in_one_process_measure_memory_pressure_from_their_own_start`. The first daemon
  reads 8 GiB available; the second, started after it, reads 1 GiB less at its first sample, then a
  further 256 MiB less.
  - It expects no hold for the gap between their starts, and the later drop held across the second
    daemon's shards.
  - Run against the old process-wide baseline (swapped in temporarily), it failed: the second daemon
    withheld 536,870,912 bytes per shard for growth that was not its own.
- The churn test passes with the hold pinned. Phase two reaches the full cap of three seals.
- `recovery` 12 and `nfs_mount` 14, which drive the hold, pass.

## Siblings reported

- **Other tests may depend on admittable capacity.** Any test whose expectations do (a tight quota, a
  takeover's budget on a loaded runner: `BudgetExceeded { available: 0 }`, macOS run 36828863866) can be
  moved by the live hold. They should pin it as the churn test now does, once their own logs show the
  hold moving.
