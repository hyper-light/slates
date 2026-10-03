# tests: the TSan lane's race canary was not reported on every run, so the lane failed spuriously

**Date:** 2026-10-03. **Audit:** AUD-29-32. **Design:** Part 6 "Concurrency"; `docs/wip/concurrency.md` §4.
**Found by:** CI run 37143592977's TSan job, which failed with "the canary was not reported as a race (ran: true,
reported: false, exit: 0)".

## Description

`cargo xtask tsan` requires ThreadSanitizer's report from a deliberate race before it trusts the suites. The canary
raced two sibling threads' writes to one word. TSan reported it in my container and in two of the first three CI
runs, and missed it in the third, which failed the lane. The canary's module doc had claimed it is reported "on
every run whatever the interleaving". That was wrong.

## Root cause

Two shapes of the canary were unreliable:
- **Writers that could finish before the other began.** CI missed the race once in three runs. I believe TSan
  reused the finished thread's slot, but I have not confirmed it.
- **Writers handshaking so that they wrote at the same instant.** Missed once in 20 runs here. This is consistent
  with TSan's own shadow-memory update being racy when two accesses truly overlap, though I have not shown that
  either.

## Impact

The TSan lane failed spuriously on one CI run in three (run 37143592977). No race went unreported in the suites.
A clean suite run still required the canary's report first.

## Exact edits

- `crates/mem/tests/race_canary.rs`: the first writer writes, then raises a relaxed flag. The second waits for it,
  then writes. TSan models a relaxed atomic as no synchronization, so the writes stay unordered but never overlap.
  Each writer waits for the other's acquire/release "done" before exiting, so neither thread's slot is reused
  while the other writes.
- `docs/wip/concurrency.md` §4 records the measurement.

## Proof

The canary is reported 100 of 100 times in an aarch64 container (nightly 2026-10-02), and 50 of 50 at two CPUs.
The two earlier shapes, measured the same way, missed it once in three CI runs and once in 20 local runs.

## Sibling sweep

The other open TSan observation, `a_client_learns_its_wake_from_the_parks_a_reply_ended` (one CI failure in three,
a timing assertion rather than a race report), is a different matter. It was not reproduced in 40 local runs, and
it stays under watch.
