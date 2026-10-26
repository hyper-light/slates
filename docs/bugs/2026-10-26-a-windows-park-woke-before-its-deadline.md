# A Windows park woke before its deadline

- **Date:** found 2026-10-26 (while landing A-179, on jupiter), fixed 2026-10-26.
- **Status:** fixed 2026-10-26, decided under the standing goal, not in plan mode.
- **Subsystem:** the runtime's IOCP driver (§4.3): `crates/rt/src/iocp.rs`, `IocpDriver::wait`.
- **Severity:** the driver broke its seam's promise. `Driver::wait` blocks until a kick, a completion, or `timeout_ns`
  passes, but a timed park returned before its deadline. A shard then finds its timer not yet due and parks again,
  so no work was lost. What was lost is the scheduler-overrun measurement (`ShardContext::scheduler_overrun_ns`,
  which feeds the fleet's detection windows, §4.8): it under-reported by however early the park woke. The overrun
  test failed for it on Windows.
- **Exposure:** Windows only. Neither Windows CI lane ran `crates/rt/tests/overrun.rs`, so only a run on jupiter showed
  it.

## Symptom

`crates/rt/tests/overrun.rs`, `a_wait_stepped_past_its_deadline_reports_the_lateness`, on jupiter (Windows
10.0.26200.9711):

```
the step 47728000 ns past its wait's deadline reports at least the 50000000 ns it was held off
```

It failed 1 run in 12, 2 in 8, and 5 in 40 across loops. The overrun always fell short of 50 ms, by 0.01 to 2.4 ms.

## Root cause

The test parks its shard until a 5 ms timer's deadline, holds the shard 50 ms, then steps it. It assumes the park
returns at or after the deadline. `IocpDriver::wait` passes `GetQueuedCompletionStatusEx` its timeout in whole
milliseconds, rounded up. But Windows expires a timed wait on its timer's tick, which can fall before the deadline
as the shard's clock (`Instant`, QPC) reads it.

**Logged first.** In a scratch copy on jupiter the test printed the park's return against its deadline
(`PARK-RETURN woke-deadline=…`), 60 runs:
- both failures woke early, at -2,324,300 ns (overrun 47,728,000 ns) and -182,000 ns (overrun 49,857,400 ns): each
  fell short of 50 ms by its early wake;
- all 58 passing runs woke after the deadline, from +384,700 to +16,208,600 ns. The last is one 15.6 ms timer tick.

## Fix

A wait that times out before its deadline waits out the rest (`IocpDriver::wait`, with the one
`GetQueuedCompletionStatusEx` moved into `dequeue`). Each further wait asks for at least a millisecond, so the rest
shrinks every time. A kick or a completion still ends the wait at once. The test is unchanged: its premise is the
seam's promise, and the driver now keeps it.

## Verification

- **Red, HEAD 07635ff9 on jupiter:** 2 of 60 runs failed, both with early wakes (above).
- **Green, with the fix, the same scratch copy and logging:** 0 of 100 failed and 0 woke early. Wakes ran from
  +151,300 to +16,296,600 ns.
- **Windows:** the whole `slates-rt` suite passes on jupiter: overrun 3 of 3, pipe 6 of 6, and every other binary.
  `cargo clippy -p slates-rt --all-targets -- -D warnings` is clean.
- **CI:** the three Windows jobs now run `overrun.rs`, so a regression shows there.

## Siblings checked

- **epoll:** `epoll_wait`'s millisecond timeout is rounded up to the clock's granularity and does not expire early.
  The man page: "the timeout interval will be rounded up to the system clock granularity". Not measured here.
- **kqueue:** not observed early. On macOS under a launchd job's policy, timed waits come back late, not early (the
  async SDK record of 2026-10-26).
- **The simulation:** virtual time.
- Unmeasured on the other drivers, so no claim is made beyond these.
