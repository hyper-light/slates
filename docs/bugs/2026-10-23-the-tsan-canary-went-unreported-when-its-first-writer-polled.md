# tests: the TSan canary went unreported when its first writer polled past ThreadSanitizer's trace

**Status:** fixed 2026-10-23 (approved by Ada in plan mode). **Audit:** AUD-29-32. **Design:** Part 6 "Concurrency";
`docs/wip/concurrency.md` §4.
**Found by:** the scheduled CI run 39925420604 (TSan job 120320486654, 08:32 UTC, `5e3db6d6`), which failed with "the
canary was not reported as a race (ran: true, reported: false, exit: 0)". The same commit's push run 39920027894,
45 minutes earlier, reported it. Both runs used the same nightly (rustc 1.101.0-nightly `7189d39d9` 2026-10-22) and
the same build cache.

## Description

`cargo xtask tsan` requires ThreadSanitizer's report from a deliberate race before it trusts the suites
(`crates/mem/tests/race_canary.rs`). The last 100 CI runs on main (2026-10-08 to 2026-10-23) ran the canary in 99
TSan jobs. It went unreported in 2 of them, both scheduled runs: 38704730770 (2026-10-15) and 39925420604
(2026-10-23). Each failed the lane.

The 2026-10-03 record (`docs/bugs/2026-10-03-the-tsan-canary-was-not-always-reported.md`) left two causes guessed and
unconfirmed: a reused thread slot, and overlapping updates of the detector's shadow memory. Neither was the cause.

## Root cause

**ThreadSanitizer drops a race whose earlier access it can no longer find in that thread's trace.**
- Before it reports, the runtime replays the earlier access's thread trace to recover its stack (`RestoreStack`).
  When that replay fails, it records the race as spurious and returns without a report
  (`compiler-rt/lib/tsan/rtl/tsan_rtl_report.cpp`, lines 813 to 817; LLVM main, read 2026-10-23).
- A thread's trace keeps `Trace::kMinParts` (3) plus `history_size` (0 by default) parts of 256 KiB, 32,763 events
  each, and older parts are recycled (`tsan_trace.h`; `tsan_rtl.cpp` `TracePartAlloc` and `TraceSwitchPartImpl`).
- Every instrumented memory access appends an event, atomic loads included, and so does every function entry and exit.

**The canary's first writer filled its own trace.** After its write it polled `done[1]` with acquire loads until the
second writer had written, at about 14 events a poll in the unoptimized test build. If the second write came more than
about 6,900 polls later, the part holding the first write had been recycled, and the race was dropped. On the
measuring Mac that is about a third of a millisecond. The second writer was spawned after the first, so any delay in
creating or scheduling it counted against that budget.

## Proof

A scratch probe outside the tree reproduces it: one test binary, the canary's shape with variants, run as separate
processes on this Mac (aarch64 macOS, nightly `0ed41eb41` 2026-09-04, ThreadSanitizer v3).

**A deterministic threshold.**
- **Setup:** the first writer performs exactly K logged loads after its write, then parks. The second writer waits by
  sleeping, so it logs almost nothing.
- **Bisection:** every point gave the same answer on both runs.

| `TSAN_OPTIONS` | trace parts | reported up to K | lost from K |
|---|---|---|---|
| (default) | 3 | 6,912 | 6,976 |
| `history_size=7` | 10 | 23,168 | 23,232 |

The ratio of the thresholds is 3.34, and of the parts 10:3 = 3.33.

**The shipped shape, timed.** Each run measured the time between the two writes (30 rounds of delays 0, 1 and 3 ms,
interleaved, load average 4.6 to 12.9):

| `TSAN_OPTIONS` | runs | reported | largest gap reported | lost | smallest gap lost |
|---|---|---|---|---|---|
| (default) | 90 | 23 | 176 µs | 67 | 2,597 µs |
| `history_size=7` | 90 | 27 | 3,929 µs | 63 | 4,578 µs |

**The shipped shape against the fix,** with a delay injected after the second writer saw the flag, 10 runs each:

| first writer | 0 ms | 2 ms | 10 ms | 50 ms | 200 ms |
|---|---|---|---|---|---|
| polls (as shipped) | 7 reported (load 17 to 20) | 0 | 0 | 0 | 0 |
| parks | 10 | 10 | 10 | 10 | 10 |

The parking writer was reported 10 of 10 at every delay with `history_size=7` as well: 100 of 100 in all.

## Impact

The lane failed spuriously in about 2 runs in 100, always for this reason. No race in the suites went unreported
because of the canary. The same rule does bound what the suites can report (see Siblings).

## Exact edits

- `crates/mem/tests/race_canary.rs`:
  - the first writer writes, raises its relaxed flag, then parks until the second has written, logging a handful of
    events however long the second takes;
  - the second writer waits for the flag, writes, raises an acquire/release flag and unparks the first;
  - the module doc states the mechanism and these measurements.
- `docs/wip/concurrency.md` §4 item 1 and the AUD-29-31–32 row in `docs/wip/GAPS.md`: the measurement and the cause.
- `CLAUDE.md`: the canary as the canonical example of a detector's non-vacuity canary, with this gotcha.

## Verification

**The real canary**, built as `cargo xtask tsan` builds it (`-Zsanitizer=thread --cfg=rustix_use_libc`,
`-Zbuild-std`) but for this Mac's target. The two binaries were compared byte for byte (they differ) and run in
interleaved rounds:

| condition | shipped (polls) | fixed (parks) |
|---|---|---|
| alone, load average 26 to 31 | 0 of 60 reported | (not yet built) |
| interleaved, load average 18 to 24 | 0 of 60 | 60 of 60, every one exiting non-zero |
| interleaved, at background priority (`taskpolicy -b`) | 30 of 30 | 30 of 30 |

- **A live runtime.** The shipped binary prints "Running under ThreadSanitizer v3", so its silence was a dropped
  report, not missing instrumentation.
- **The background-priority rounds.** The shipped canary's outcome depends on how many polls its first writer makes
  before the second writer arrives. That turns on the two threads' relative speed, and at background priority it fell
  under the threshold every time. The fixed canary's first writer makes no polls, so it was reported in every
  condition.
- **Gates:** workspace clippy `-D warnings`, fmt, `cargo xtask check`.
- **CI:** the TSan job of the runs that follow.

## Siblings

- **The suites live under the same rule.** A race whose earlier access lies more than three trace parts back in its
  thread's history (about 98,000 events) is dropped without a word. A spinning shard logs that many in milliseconds.
  `history_size=7`, the maximum, would keep ten parts, at 2.5 MiB of trace per thread instead of 0.75 MiB. Reported to
  Ada; not changed here.
- **No other test depends on a ThreadSanitizer report.** The lane requires no report anywhere but the canary.
