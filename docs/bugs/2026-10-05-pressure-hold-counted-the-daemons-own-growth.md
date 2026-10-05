# The memory-pressure hold counted the daemon's own growth (2026-10-05)

## Description

Under a 1 GiB container cap with two shards, one dynamic volume was refused `ENOSPC` at 218–264 MiB. That was
although its shard held the whole content pool (357.8 MB) and the budget showed only 244 MB committed. No budget
refusal was counted, and the pool had nothing left to hand out.

## Root cause

Diagnostic lines on the quota-denial path showed the refusal:
`committed=243793920 headroom=2621440 hold=111171584 capacity=357826560 claimable=0`. The pressure hold, 111 MB per
shard, closed the gap. The hold is the host's available memory below the daemon's first sample, divided among the
shards (`refresh_pressure_hold`, §4.2, admission.md). The host's available memory had fallen because the daemon had
itself stored 244 MB of content. Those bytes were committed already, so the hold charged them a second time. With own
usage U, committed ≈ U and hold ≈ U, so a volume reaches about half the content capacity on any host.

## Impact

Every daemon: admission stopped near half the content capacity once the host's available memory reflected the
daemon's own use. A loaded host also lost real headroom it had. The two-daemon pressure test did not see it, because
it injects the available reading and never grows the daemon.

## Edits

- `crates/machine/src/facts.rs`: `Facts::resident_now`, the process's resident memory. Linux reads `/proc/self/statm`;
  macOS reads `proc_pid_rusage`'s `resident_size`, through one `rusage_v0` helper shared with `locked_bytes`;
  Windows has none, as it has no available reading.
- `crates/server/src/daemon.rs` `refresh_pressure_hold`: the shortfall less the daemon's own resident growth since
  the first sample, which keeps a resident baseline beside the available one.
- `crates/server/src/state.rs`: `injected_resident`, `pressure_resident_baseline`.
- `Daemon::inject_resident_memory` (test support).
- Test, red first: `a_daemons_own_growth_is_not_memory_pressure` (recovery.rs). Available falls 512 MiB as resident
  rises 512 MiB: no hold. A further 256 MiB fall: 128 MiB held per shard.

## Measured after

The same container: one volume holds 338 MiB (355.2 MB committed of the 357.8 MB pool, the rest the operation
headroom), the first 200 files' SHA-256 intact, 0 panics, 0 restarts, `oom_kill 0`.

## Siblings

- Several daemons in one process (the in-process test fleets) share one resident reading, so each counts the others'
  growth as its own and holds back less than the pressure. That shape is test-only; production runs one daemon per
  process.
