# The pressure hold counted other processes' memory swings on a roomy host as pressure

**Found:** 2026-10-09, while running the recovery suite for A-128. `an_extent_claimed_just_before_a_crash_is_returned_at_the_first_publication`
was flaky: at the base commit `caaeba85`, 3 of 6 runs failed. **Status: fixed** (same day).

## Description

After the test's restart, shard 0 refused a 1 MiB create with `BudgetExceeded { available: 0 }`. The daemon's
refuse-all breakdown, logged on the first failing run:

```
bytes budget refuse-all (first occurrence): requested=1048576 config[reserve_per_shard=33554432 shards=2 …]
bytes[capacity=67108864 committed=4194304 retained=0 headroom=10485760 replicated=0] …
```

`admittable = capacity − committed − headroom − hold` was 0 with capacity 64 MiB, committed 4 MiB and headroom 10 MiB,
so the pressure hold (not in the breakdown) was at least 50 MiB a shard. At the same moment `memory_pressure` reported
92% of the host's memory free.

## Root cause

`refresh_pressure_hold` (`crates/server/src/daemon.rs`) held back the host's drop in free memory below the daemon's first
sample, less the daemon's own growth since (admission.md §5.5, as amended 2026-10-05). On a machine shared with other
work (load 70–85 from other sessions) the host's free memory swings by hundreds of megabytes. A swing of 100 MiB after
the daemon's first sample held 50 MiB on each of its two shards, the whole admittable capacity of a daemon whose
content capacity was 64 MiB, on a host with gigabytes free.

The hold exists to keep the invariant of §4.2 "Atomic admission": the daemon must never promise more than the host can
back. A host with more memory free than the daemon could ever fill backs every promise even after other processes take
part of that slack. The formula held back any drop, slack or not.

## Impact

- **Spurious refusals on roomy, busy hosts:** a developer laptop or a shared node running a small slates daemon refused
  volume creates and dynamic growth whenever another process allocated after the daemon started, with the host nowhere
  near pressure.
- **Takeover:** a takeover's materialization lifts the hold (admission.md §5.5, amended 2026-10-07), so it was not
  affected; every other admission was.
- **A flaky test:** the recovery suite's pool-crash test, which hid this as noise.

## Exact edits

- `crates/server/src/daemon.rs`:
  - `pressure_hold(baseline, available, own_growth, capacity)`, a pure function: the smaller of the shortfall (as
    before) and the daemon's unfilled room, `capacity − own_growth`, less the host's free memory;
  - `refresh_pressure_hold` uses it, with the daemon's whole content capacity (`reserve_per_shard × shards`);
  - two unit tests: with no slack the hold is the shortfall and own growth is never held; on a roomy host only a drop
    into the daemon's room is held.
- `crates/server/tests/recovery.rs`:
  - `memory_others_take_from_a_roomy_host_is_held_only_where_it_eats_into_the_daemons_room` (failing first: the drop
    from the slack was held). The host first has four times the daemon's capacity free, then three times, then half;
    only the last holds, and exactly half the capacity.
  - The two earlier pressure tests now show the daemon a host with exactly its capacity free (no slack), so a later drop
    is still held whole, at any machine's derived capacity. Before, they injected 8 GiB.
- The pool-crash test passed 10 of 10 afterwards at load 73, against 3 of 6 failing before.

## Siblings

- The hold is divided evenly among the shards, while the content pool (A-98) lets one shard own more of the capacity
  than another. On a host under real pressure, the hold could withhold a shard's admission while another shard's
  capacity sits idle. Not changed here. **Fixed 2026-10-10:**
  `docs/bugs/2026-10-10-the-pressure-hold-was-split-per-shard-while-the-pool-is-shared.md`. The hold is the daemon's,
  withheld once: from the pool's extents, then from the shards' free room, then as a debt.
