# A Windows shard's idle spin outran its watchdog, and its parked wake waited out the spinners' turns

**Date:** 2026-11-18. **Area:** `crates/rt/src/iocp.rs` (the blocked wait), `crates/rt/src/shard.rs` and
`runtime.rs` (the idle window and holds), `crates/server/src/config.rs` (the idle window's rule, the idle spin's
ceiling); design §4.3 "Drivers", §4.7; A-223's open item (C); A-361. **Found by:** g258's now-x86 lane (CI run
44325673298, windows-latest, 4 vCPUs, two spinners a logical CPU, WinFsp drives): the anchor killed main's daemon for a
lapsed beat in 3 of 3 runs, where the arm from before A-223 (69cd38b2) never lapsed and windows-11-arm did not lapse.

## Description

The lane's log (`ab.txt` on `diag-results/g258-44325673298-now-x86`), a one-shard daemon under its anchor:

```
slates daemon: idle_window_ns = 1298300: max(provision_floor_ns, wake estimate × loaded_wake_ratio), ...
slates daemon: shard 0 is spinning: 1015 ms of CPU without a step, past the 1000 ms window; the watchdog withholds the beat
slates anchor: the daemon's heartbeat lapsed (1139 ms old, past 1077 ms: ...); killing it
```

With one shard nothing is forwarded, so A-326's closed-loop hold, the first suspect, held nothing. The watchdog judged
the shard spinning: a window of CPU with no step and no park. The arm from before A-223 derived its window as
`spin_before_park_ns × IDLE_WINDOW_RATIO` (28–42 µs at boot) and never lapsed.

## Root cause

Two faults, one under the other, both measured.

1. **A parked Windows shard woke late under load.** Windows raises a thread a priority level when its wait on an event,
   a semaphore or an address ends (the dynamic boost), and gives none to a thread dequeued from an I/O completion port,
   which is how a shard parks. Beside spinners at its own level, the kicked shard stayed queued until a spinner's turn
   (a quantum) ended. Measured by a probe on both CI Windows runners (`diag-g234-wake-1`, run 44325923565; the jobs run
   BelowNormal, base priority 6; two threads at one level on one CPU took turns of 31.6–32.2 ms): after a completion-port
   wake the waiter's priority was 6, after an event's or WaitOnAddress's 7. Loaded medians: completion port 29.1 ms
   (windows-latest) and 14.0 ms (windows-11-arm), the runtime's own parked shard 25.5–29.6 and 14.0 ms; an event 15–17
   and 6 µs; an event with its boost disabled 29.2 and 14.0 ms; a completion port raised a level for its wait 15–17 and
   6–7 µs. Quiet, all 6–15 µs.
2. **The window was a hundred times an estimate that counted that wait.** A-223 (2026-11-06) made the Windows estimate
   learn only confirmed sleeps, and the supervisor's decision A (2026-11-11) kept Windows' window at `max(F, estimate ×
   100)` because of fault 1. Under load the estimate counted the 14–42 ms waits, so the window was seconds. A WinFsp
   request notes client activity; the shard then had nothing to run and spun out the window with no step, and the
   watchdog's verdict ("1015 ms of CPU without a step") withheld the beat. Before A-223 the estimate learned handoffs of
   0.14–0.85 µs, so the window stayed small and the lane never lapsed.

Nothing bounded an idle spin against the watchdog's window: the window rule and the watchdog were derived apart.

## Impact

Every Windows daemon on a loaded machine: the anchor killed live daemons that had served a request, mid-workload
(mount_bench's `contended` pass ended with `The I/O operation has been aborted`), and every parked shard's wake under
load cost a quantum (the loaded single-client p99 had 4 of 9 runs at 56–83 ms on windows-latest under the multiple).

## Exact edits

- `crates/rt/src/iocp.rs`: `IocpDriver::wait` holds `RaisedForWait` across a wait that may block: a thread at the normal
  level goes one level up (`move_level`, `GetThreadPriority` then `SetThreadPriority`) and back before it runs anything;
  another level, or a zero timeout, is left alone. `unsafe-budget.toml`: slates-rt 101 → 102 for that one block.
- `crates/server/src/config.rs`: `loaded_wake_ratio` (0 on Linux and macOS, 100 on Windows) and `idle_window_ns` are
  replaced by `NO_ESTIMATE_MULTIPLE` (0 everywhere): the window is R9's floor on every platform. `IDLE_SPIN_CEILING`,
  the liveness budget less one beat, with a compile-time assertion that it sits a beat under the budget, the least spin
  window the watchdog judges by (A-318); the daemon sets it as `WakeTracking::idle_ceiling_ns`.
- `crates/rt/src/runtime.rs`, `shard.rs`: `WakeTracking::idle_ceiling_ns` (`NO_IDLE_CEILING_NS` for a runtime nothing
  watches); `ShardContext::idle_window_ns` and `hold_idle_window` never pass it, so no idle spin, window or hold, does.

## Tests (failing first)

- `crates/rt/tests/wake_estimate.rs` `loaded_wake::a_parked_shard_wakes_beside_spinners_as_soon_as_a_parked_thread_does`
  (Windows): red without the raise on both runners, 6 of 6 rounds (the shard's median 21.5–42 ms against a parked
  thread's 11–23 µs), green with it, 6 of 6 (33–59 µs against 10–23 µs); `diag-g234-loadedwake-{red,green}-{1,2}`.
  It skips loudly where a job fixes the priority class (below).
- `crates/server/tests/idle_window.rs`, now run on Windows too: a paced client beside spinners must leave every shard's
  window at the boot window. Red on main's Windows rule (`diag-g234-guard-red-1`): "shard 0's idle window grew with
  the load: 324264700 ns against 866800 ns at boot, its estimate 3242647 ns" (windows-latest) and 453.7 ms against
  0.95 ms (windows-11-arm), after 64 paced requests; green with this change on windows-latest, windows-11-arm,
  ubuntu-latest and macos-latest (`diag-g234-guard-green-1`, with `slates-rt`'s suite and the watchdog's and config's
  unit tests).
- `crates/rt/tests/idle_spin.rs` `an_inflated_estimate_or_hold_spins_no_longer_than_the_ceiling`: a runtime pricing its
  window on an estimate a hundred quiet stretches long, at the largest multiple, under a ceiling of ten timer periods;
  red with the ceiling removed (the window reported unbounded), green with it. And
  `crates/server/src/watchdog.rs` `an_idle_spin_held_to_its_ceiling_is_never_judged_spinning`: the watchdog's own judge
  sees a ceiling's spin alive at every look in builds as fast as the machine and ten times slower, and a spin of two
  windows judged spinning.
- Tried and dropped: a supervised daemon whose shards priced their window on such an estimate (`liveness.rs`'s role
  shape) kept its beat on main, with no ceiling, on windows-latest, windows-11-arm and macos-latest (largest gaps
  112–175 ms against debug spin windows of 3.4–8.0 s, the budget held to each build's slowdown); why the spin did not
  reach the verdict there was not found, and a test that cannot fail was not kept.

## Siblings and what stays open

- The boot wake probe parks in `std::thread::park` (WaitOnAddress on Windows), which Windows boosts, so it measured a
  boosted wake while the shards paid an unboosted one, and never showed fault 1.
- A job with `JOB_OBJECT_LIMIT_PRIORITY_CLASS` (kiln's agent job on jupiter: flags 0x2020, BelowNormal) keeps a thread's
  level: `SetThreadPriority` answers success and nothing changes, so there the raise does nothing and a kicked shard
  still waited 30.75 ms against a parked thread's 12.5 µs. The window stays R9's floor, so it costs latency, never a
  kill. A user APC as the kick carried no boost either (loaded median 252 ms). Open in GAPS: park on an address when no
  I/O is outstanding.
- The ipc client's Windows wait is an event, which Windows boosts; it was never subject to fault 1.
