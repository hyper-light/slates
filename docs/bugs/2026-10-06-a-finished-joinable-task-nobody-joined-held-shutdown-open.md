# A finished joinable task nobody joined held shutdown open

**Found:** 2026-10-06, by the focal session's port of this runtime into `hyper-rt`, which met it there and asked
whether slates has it. It does. Reproduced here first by a failing test.

## Description

`futures::spawn` admits a joinable task and returns a `TaskId`, not a guard. A joinable task that finishes keeps its
slot as `Done`, so a later `join` can read its outcome, until it is joined or detached. A shard leaves its loop only
when its arena is empty (`shutting_down && arena.is_empty()`). So a runtime holding one finished task that nobody
joined never shut down: `Runtime::shutdown` never returned. The same held for a joinable task still running at
shutdown: the shutdown's cancellation finished it, and it then held its slot too.

CLAUDE.md recorded this as a usage rule ("a perpetual task must be detached or shutdown never completes", the
daemon's serve loop, `SLATES_DESIGN.md` near the boot amendment), so every known caller detached. The runtime itself
still hung for any caller that did not.

## Root cause

Nothing released a finished joinable task's slot except a join or a detach, and a shutdown has neither: it cancels
every task, any would-be joiner included.

## Fix

`crates/rt/src/shard.rs`:
- **`release_finished`**, run as a shutdown begins: releases every task that has already finished, joinable or not.
- **`complete`**: while the shard is shutting down, releases a finishing task even if it is joinable. A joiner already
  waiting is woken first and reads the task as gone.

Admissions are refused from the shutdown on, so a released slot is never reused under a stale parent link.

## Tests

`a_shutdown_completes_beside_a_finished_task_nobody_joined` (`crates/rt/tests/admission.rs`) has two joinable
children, one finished before the shutdown and one cancelled by it, neither joined. The shutdown returns. Before the
fix the test failed on its 10 s wait. With either half of the fix removed it fails again: each half is needed.

Every rt test binary passes (23), and the rt Miri suites pass.

## Sibling sweep

- While a shard runs, a finished task nobody joins still holds its slot, by design: a join may yet come. CLAUDE.md's
  gotcha now says that, and that detaching is right for such a task.
- `hyper-rt` has the same fix (focal, commit 70173d6, local).
