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
- On Windows, the same two tests in `crates/cli/tests/start_judgment.rs` (`cli.rs` is Unix as a whole). They suspend the
  successor whole (`NtSuspendProcess`), as `winfsp.rs` suspends its anchor, and the stuck one also requires the anchor's
  start-kill line. To name the successor without `pgrep -P`, the anchor's restart line now says "restarted as pid N",
  and its start-kill line says "the starting daemon (pid N)". Both CI Windows lanes run them.

## Measured at the extremes (2026-11-07)

On this Mac under clients' recipe (`frozen-load.sh`, eight nice-19 spinners a CPU, 144 on 18, with other agents' work
alongside: 240 runnable processes), debug build of 2810a612:

| test, runs | starts | starts of a progressing daemon killed | test result |
|---|---|---|---|
| `a_frozen_daemon_is_killed_at_its_heartbeat_lapse_and_restarted` ×5 | 5 first, 3 successors | **0** | 1 passed; 4 failed on the harness's fixed waits |
| `a_starved_start_that_makes_progress_is_not_killed` ×5 | 5 first, 4 starved successors | **0** | 3 passed; 2 failed on the harness's fixed waits |
| `a_stuck_start_is_killed_and_restarted` ×3 | 3 first, 1 stuck | 0; the stuck one killed: "made no progress for 1104 ms (the 1000 ms liveness window)" | 3 failed on the harness's fixed waits |

Before the fix, clients counted 104 kills in one frozen run at load 150-233. The failures above are the tests' own
fixed waits, not the anchor: `await_daemon`'s 20 s `START_WAIT` for a successor that at about 1.5% of a CPU serves
just after it (`cli.rs:3212`), and `await_stable_daemon`'s attempt streak (`cli.rs:291`). Told clients, 2026-11-07.

Those waits are now judged as the anchor judges a start (`StartProgress` in `cli.rs`): a wait fails once the instance's
daemon processes made no CPU progress for the start wait, or at a ceiling of the start wait at the slowest share the
suite holds a start to (1/64). The frozen and stuck tests no longer bound the successor's whole boot on the wall: the
frozen test checks the kill came within a window of the lapse, and the stuck test reads the anchor's start-kill line
and checks "no progress for N ms" lies in [window, 2 × window). `held_rendezvous.rs`, the rendezvous handoff, ran on
Unix only because it killed with `kill`; it now ends the daemon with `TerminateProcess` on Windows and runs in all three
Windows CI lanes.

On CI (diag-liveness-10, two spinners a CPU on four runners), the stuck start was killed at 1,001-1,007 ms on both
Linux runners, 1,007-1,070 ms on windows-latest and 1,002-1,009 ms on windows-11-arm, three runs each, and the
starved start was never killed (`start_judgment.rs` on Windows, `cli.rs` on Linux).

## Left open

- **Recovery in one poll.** Since A-228 the watchdog starts before the shards' recovery ends, so a start beats during a
  short recovery; but a shard's recovery runs as one poll, and past a window of CPU the watchdog counts it spinning and
  withholds the beat. GAPS measures one green's recovery at 246-936 ms. Slicing it is the next piece.
- **The client's reconnect budget** is still `recovery + liveness` (2 s, `Deadlines::derive`); a starved restart that is
  no longer killed can outlast it. Owner: clients.
- **`Facts::query()` for the identity** gathers every machine fact to read one (676 ms at the p50 above).
- **The anchor itself** refused to start under the same load: "measure the machine profile: measurement timeout:
  wake". Fixed separately, by the same rule applied to the wake probe:
  `2026-11-07-a-busy-machine-refused-the-anchors-boot-at-the-wake-probe.md`.
