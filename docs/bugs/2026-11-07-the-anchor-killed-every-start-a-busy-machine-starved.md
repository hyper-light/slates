# The anchor killed every start a busy machine starved (2026-11-07)

## Description

A starting daemon that had not beaten within the anchor's fixed recovery budget (`RECOVERY_BUDGET_NS`, one wall second)
was killed, and every restart started over (`crates/cli/src/anchor.rs`, "the daemon never beat inside the recovery
budget; killing it"). On a busy machine a start took longer than a wall second, so the anchor looped: clients measured
104 kills in one run of the frozen-daemon test at load 150-233 on this Mac, and loadtest-scale saw four kills in a row
and 439 of 1,000 clients give up at load 60-89.

## Root cause, measured

A start-phase log (each phase stamped with the shared monotonic clock and the daemon's own CPU time, diagnostics only)
under clients' recipe on this Mac, 144 nice-19 spinners on 18 CPUs, debug build, 153 starts:

| phase | n reached | p50 | max |
|---|---|---|---|
| anchor spawn → daemon `main` | 152 | 202 ms | 787 ms |
| `Facts::query()` (for the identity) | 79 of 152 finished | 676 ms | 1,178 ms |
| segment attach | 19 | 473 ms | 604 ms |
| profile bytes | 17 | 0.1 ms | 306 ms |
| profile parse | 17 | 0.9 ms | 187 ms |
| config derive, runtime, recovery, watchdog, first beat | 0 | | |

All 153 were killed 1.07-1.22 s after their spawn, none past its config. A start that reached its profile read had used
16-18 ms of CPU in over a second of wall time: the machine starved it; it was not stuck. A fixed wall second cannot tell
the two apart. A running daemon was already judged by progress, in each shard's own CPU time (A-228); a start was not.

## Exact edits

- `crates/machine/src/process_clock.rs` (new): another process's CPU clock, read by its supervisor: `/proc/<pid>/schedstat`
  on Linux, `proc_pidinfo(PROC_PIDTASKINFO)` with `mach_timebase_info` on macOS, `GetProcessTimes` and
  `QueryProcessCycleTime` on Windows.
- `crates/cli/src/anchor.rs`: until its first beat, a start is judged by the watchdog's own rule
  (`slates_server::watchdog::ShardWatch`) over the liveness window, in the daemon's CPU time: killed once it makes no CPU
  progress for a window ("made no progress for N ms") or runs a window of CPU without beating ("ran N ms of CPU without
  beating"); waited for while it progresses. The fixed second no longer decides a start. The rule is logged at boot.
- `unsafe-budget.toml`: `slates-machine` 53 → 61, the eight FFI sites named.

## Tests

- `a_starved_start_that_makes_progress_is_not_killed` (`crates/cli/tests/cli.rs`): the successor is run one sixty-fourth
  of every heartbeat for three start budgets, the share measured above. On main the anchor killed it 1,081 ms after its
  spawn; with the fix it is the daemon that answers, with no start killed.
- `a_stuck_start_is_killed_and_restarted`: a successor stopped for good is killed, "made no progress for 1081 ms (the
  1000 ms liveness window)", and replaced.

## Left open

- **Recovery in one poll.** Since A-228 the watchdog starts before the shards' recovery ends, so a start beats during a
  short recovery; but a shard's recovery runs as one poll, and past a window of CPU the watchdog counts it spinning and
  withholds the beat. GAPS measures one green's recovery at 246-936 ms. Slicing it is the next piece.
- **The client's reconnect budget** is still `recovery + liveness` (2 s, `Deadlines::derive`); a starved restart that is
  no longer killed can outlast it. Owner: clients.
- **`Facts::query()` for the identity** gathers every machine fact to read one (676 ms at the p50 above).
- **The anchor itself** refused to start twice under the same load: "measure the machine profile: measurement timeout:
  wake".
