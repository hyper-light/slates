# server: an observation read a starved shard as wedged

**Date:** 2026-10-03. **Design:** §4.14 (observations typed to their stage; one budget), §4.3. **Found by:** the Linux
server tests' startup timeouts. One CI run of the Linux server tests hit four of them. Reproduced in a Linux container
(Docker Desktop on an M5 Max, 18 cores) with the library suite beside 108 CPU burners: 7, 13 and 10 observations per
run ended `Deadline`.

## Description

Every daemon observation (`crates/server/src/observe.rs`) ran under one absolute wall budget of 10 s across
submission, admission and execution. A shard that got little CPU on a loaded host still took the question and answered
it, just slower than the budget. The observation refused `Deadline` anyway, so a starved but working shard read the same
as a wedged one. Sampling the shards' `/proc/<tid>/stat` while observations timed out showed the shards runnable
(state R, preempted), not blocked.

## Root cause

The budget was counted in wall time. On a host with more runnable threads than cores, wall time measures the
scheduler's share, not the shard's progress. The design's intent for the budget is to end an observation of a shard
that will not answer, not one that the host will not run.

## Impact

Observations failed on a loaded host:
- the daemon's `bootstrap` (the "startup timeouts");
- the tests' and the operator's accessors.

Each one failed although its shard was working. No state was changed or lost; the refusal was typed and spurious.

## Exact edits

- `crates/rt/src/thread_clock.rs` (new): a thread's CPU clock that another thread can read.
  - Linux: `pthread_getcpuclockid` with `clock_gettime`.
  - macOS: the thread's Mach port with `thread_info(THREAD_BASIC_INFO)`.
  - Windows: the thread id, opened for each read with query rights only. `QueryThreadCycleTime` (exact) tells
    whether the thread ran; `GetThreadTimes` (kernel plus user time, charged a scheduler tick at a time) is what it
    spent.
  - Miri: none.
- `crates/rt/src/registry.rs`: each shard entry records its thread's clock as the shard starts (`record_cpu_clock`,
  called in `run_worker` before the shard reports ready). `shard_cpu(holder)` reads it, and answers `None` once
  that registration no longer holds the slot.
- `crates/server/src/observe.rs` `ShardTime`: at a wall deadline, the observation goes on only while the shard
  consumed CPU during the window just ended. It continues for the part of the budget the shard has not yet run, as
  wall time.
  - A starved shard keeps consuming CPU and is answered.
  - A wedged shard consumes no CPU and ends the observation at the end of the first whole window it spends without CPU: one window if it was wedged when the observation began, two if it wedged partway through the first.
  - A busy shard that never answers ends it once it has run the budget. An uncontended shard does that at the wall
    clock's pace, so it gets no extra window.
  - The extension applies to the receipt's wait, the answer's wait, and a capacity refusal held at the deadline.
- `unsafe-budget.toml`: slates-rt 63 → 73, naming the ten clock sites (Linux 2, macOS 3, Windows 5).

## Proof

- `crates/server/tests/observe.rs` `a_starved_but_working_shard_is_answered_past_the_wall_budget`: red before the
  change.
- `a_wedged_shard_ends_the_observation_long_before_its_wedge_does`: a question that blocks its shard off the CPU for
  five budgets ends at its deadline before three. A first version asserted one window and failed on CI's macOS lane
  (run 37178742493, 656 ms against 600): the shard ran the task's start in the first window, which the rule rightly
  reads as working, so the bound is two windows plus the deadline's lateness.
- `crates/rt/src/thread_clock.rs` `a_threads_cpu_clock_reads_from_another_thread_and_grows_as_it_runs`.
- Every observe test passes on macOS and Linux.
- Beside 108 burners, the observations that ended `Deadline` went from 7, 13 and 10 per run to 1–2.
- Every remaining one had a target shard that consumed no CPU for the whole budget, which the change correctly
  refuses. Those shards were blocked on the memory-map lock, behind a strict volume's arena lock
  (`docs/bugs/2026-10-03-locking-an-arena-stalled-every-shard-on-the-memory-map-lock.md`). With that fixed, six runs
  end no observation `Deadline`.

## Sibling sweep

Every wall-clock wait on a shard in `crates/server`:
- `observe.rs` (fixed: submission, receipt, answer, capacity retry).
- The client's reply deadline (`crates/client`) waits on a daemon in another process. It asks liveness on a cold path
  (`Liveness`) and is unchanged.
- Windows has the same rule: the native Windows CI lane runs the clock test and the observe tests.
