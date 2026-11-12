# A starved shard released between two reads was judged wedged

- **Date:** 2026-11-09. **Status:** fixed (observations and cross-shard waits); two siblings open (GAPS, A-228's
  siblings). **Design:** §4.14, A-65, A-228. **Found by:** CI's clients timing-all-5 lane on windows-latest x86_64
  (commit 972210ae, `diag-results/clients-timing-all-5-windows-latest` adffef60): `an_observation_of_a_starved_shard_is_answered`
  failed once, "a starved shard's observation was given up on", its non-vacuity check passing.

## Description

A wait judged in a shard's own time (`observe::ShardTime`: observations, `xshard::within_shard_time`) ends at a window
with no CPU progress only when the scheduler does not have the shard runnable (dd4d3c54). It read the shard's clock,
then asked the scheduler. A starved shard released between the two reads ran, finished what it was doing and parked in
its idle wait before the scheduler was asked: the clock said no progress, the scheduler said not runnable, and the wait
ended as if the shard were wedged, at the moment it ran.

## Evidence

- On jupiter (Windows 11 build 26200, x86_64, 32 CPUs) the test alone passed 10 of 10. The whole liveness suite, run as
  CI runs it, failed it 1 of 10: `Deadline { stage: Admission, waited_ns: 3043041600 }`, just after the test released
  the starvation, not at the one-window budget.
- The give-up now logs why and what the scheduler said (`ShardTime::gave_up`, `slates_rt::registry::shard_run_words`). In
  1 of 12 whole-suite runs: "no CPU in the window, and the scheduler does not have it runnable; ... began 31250000, mark
  75583833, now { spent_ns: 31250000, progress: 75583833 }; the scheduler: ThreadState 5, WaitReason 15". Waiting on
  `WrQueue` is a thread parked on its completion port, which the starved shard does once its poll ends; and its clock
  had not moved since the wait began, so the shard had not run until after the clock's read.
- The Windows snapshot (`NtQuerySystemInformation`, every thread of the system) is slow enough to leave the shard time to
  finish its poll and park between the reads; on ARM64 the lane passed.

## Root cause

Two readings of one thread taken at different instants and judged as one: no progress by the first, not runnable by the
second. A thread cannot go from runnable to blocked without running, but it can between the reads.

## Fix

`observe::WindowJudge::judge`, the window's judgment over readers it is handed: when the clock stood still and the
scheduler says not runnable, the clock is read again; a clock that moved since says the shard ran, and the wait gets
another window with its mark moved. Only a clock still unmoved ends it. `ShardTime` keeps the holder and the readers;
every give-up is logged with its reason, the clock and the scheduler's own words (on Windows its `ThreadState` and
`WaitReason`, or which part of the read failed).

## Tests

- `observe::tests::a_shard_that_ran_between_the_clock_and_the_state_is_not_given_up_on`: jupiter's readings, the clock
  unmoved on its first read and moved on its second, the scheduler saying not runnable. Red with the second read
  removed (the wait ended "no CPU in the window, and the scheduler does not have it runnable"); green after.
- `a_shard_unmoved_on_both_reads_and_not_runnable_is_given_up_on` and `a_starved_shard_unmoved_but_runnable_gets_another_window`
  hold the rest of the rule.
- On jupiter, the whole liveness suite 12 times after: the observation answered 12 of 12, no give-up logged.

## Impact

An observation or a cross-shard call of a starved shard could be refused `Deadline` at the moment the shard was released,
on any platform; Windows' slow snapshot made it likely enough to be seen.

## Siblings (fixed 2026-11-11, liveness)

- The watchdog (`watchdog::read_runs`) asked the scheduler only about shards its clock reading suspected, after that
  reading: a shard released between them read wedged for one look, its beat withheld once.
- The anchor's start judgment (`anchor::judge_start_of`) read the process's clock, then `process_runnable`: a start
  released between them read wedged, and one wedged look killed it.
- Both now take the scheduler's answer through `Look::told`, which reads by the rule `observe::WindowJudge` reads by,
  now one function, `watchdog::ran_while_asked`: after a not-runnable answer the clock is read again, and a clock that
  moved replaces the first reading. Red first, each with a replayed reading:
  `a_shard_released_between_its_clock_and_the_scheduler_is_not_judged_wedged` (`crates/server/src/watchdog.rs`) and
  `a_start_released_between_its_clock_and_the_scheduler_is_not_killed` (`crates/cli/src/anchor.rs`).
