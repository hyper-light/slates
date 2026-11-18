# An observation read the shard's clock before the scheduler, and gave up on a shard released between them (2026-11-18)

## Description

`an_observation_of_a_starved_shard_is_answered` (`crates/server/tests/liveness.rs`) failed once on CI's Windows runner
(windows-latest, 4 CPUs): diagnostics run 42671057142, job 129347594615, 2026-11-09, the whole liveness suite at
`972210ae` (on `d0314abd`). It panicked at the answer's assertion: the shard had starved (its clock stood still past
the observation's deadline, the first assertion held) and the observation was not answered `Ok(42)`. That run's log
kept only lines matching its filter, so what it ended with was lost. No lane of `ci.yml` runs the liveness suite on
Windows (`docs/wip/ci-matrix.md`), so main never ran it there.

The test starves the daemon's second shard (one poll spinning at IDLE priority, pinned beside four TIME_CRITICAL
spinners), asks it a question with a one-window budget, keeps it starved past three windows and releases it. The
observation must be answered.

## Root cause, from the logs

`ShardTime::next_window_ns` (`crates/server/src/observe.rs`), which observations and the cross-shard waits
(`xshard::within_shard_time`) judge a shard's time by, ended each window this way: read the shard's CPU clock; if it
showed no progress since the last window's end, ask the scheduler (`shard_run_readings`) and wait another window only if
the shard was runnable. A starved shard released between the two reads runs, finishes its poll, admits and answers the
observation and parks in its driver before the scheduler looks. The clock read, taken before it ran, says no progress;
the scheduler says blocked; the rule judged it wedged and ended the wait. On Windows one look at the scheduler is a
whole-system snapshot, two `NtQuerySystemInformation` calls of about 3 ms (`slates_rt::thread_clock`), so the gap
between the two reads is milliseconds there and microseconds on Linux and macOS.

The test lands its release on a window's end by construction. The observation's windows are its unspent budget: 1 s,
or 1 s less one 15.625 ms tick once the shard has been charged one. The test releases the shard after three windows
of watching. In all 28 logged runs of diag-g243-1 (main `ba78159a`, every window's decision and the shard thread's
raw `ThreadState` and `WaitReason` logged, 2026-11-18) the third window ended within 20 ms of the release. In 3 of them
the rule's look at the scheduler straddled the release. In two, the rule's snapshot found the shard runnable, and a second
snapshot about 3 ms later found it parked (`ThreadState` 5, `WaitReason` 15, `WrQueue`: its I/O completion port wait),
the observation answered in the same millisecond.

diag-g243-2 ran the failing run's own tree, `972210ae`, with the same logging: 1 of 32 runs failed, on windows-latest,
and its lines are the whole sequence (milliseconds since the epoch, the shard `SlotHolder { shard: 1, generation: 2 }`):

```
G243 test: watch ended at 1794990263097                  (the test releases the shard)
G243 test: spinners joined at 1794990263101
G243 test: starving poll joined at 1794990263102         (the shard ran: its poll ended)
G243 runnable ... at 1794990263106: reading Some(RunReading { state: Blocked, .. }), raw ... Some((5, 15))
G243 window ... at 1794990263107: ... mark Some(91588530) now Some(CpuReading { spent_ns: 46875000, progress: 91588530 }) ... -> None
G243 test: the observation ended at 1794990263107: Err(Deadline { stage: Admission, budget_ns: 1000000000, attempts: 1, waited_ns: 3018954300, last_refusal: None })
panicked at crates\server\tests\liveness.rs:862:3: a starved shard's observation was given up on: Err(Deadline { stage: Admission, .. })
```

The clock read showed the mark (no progress); the scheduler, asked after it, found the shard parked; the rule ended the
wait 3.019 s after it began. The 31 other runs passed, as did 28 of 28 on main (diag-g243-1).

## Impact

Every wait judged in a shard's own time, on every platform, at the moment its shard recovers from starvation:
observations (the harness's and the tests' questions to a daemon), and every cross-shard call waited for in its
target's own time (`xshard::within_shard_time`, `call_in_shard_time`, `task_in_shard_time`): the consensus verbs'
calls to the control shard and the verbs forwarded to their volume's shard (`crates/server/src/verbs.rs`, refused
`Overloaded`), the NFS server's calls to an owner shard (`crates/server/src/nfs.rs`), remote attachments, direct maps
and work routes. Each gave up on a target that had just answered. The gap is widest on Windows; a starved shard must be
released inside it.

## Exact edits

- `crates/server/src/observe.rs`: the rule moves into `OwnTime` (the budget, the start, the mark), which takes its
  readings through closures so its tests script them; `ShardTime` keeps the shard and feeds it
  `slates_rt::registry::shard_cpu` and `shard_run_readings`. On a window whose first clock read shows no progress, the
  rule asks the scheduler and then reads the clock again: a rise at either read is a shard that ran, given the budget
  it has not run; with none, the scheduler's answer stands for the whole window, since a thread leaves the run queue for a
  block only by running first, which its clock would show. The scheduler is still asked only when the clock stood
  still. The doc sentence that said a reading at or below the mark ends the wait now says it counts as no progress,
  which it has since A-228's sibling fix of 2026-11-08.

## Tests

- `observe::tests::a_shard_that_runs_and_blocks_between_the_reads_is_given_the_window`: the clock flat at the first
  read, the scheduler finding the shard blocked, the clock risen at the read after; then a window flat at the new mark
  and blocked. Red on main's order: with the second read removed it failed (`left: None, right: Some(984375000)`) on the
  Mac Studio, 2026-11-18, as did the second half of `a_shard_that_has_run_its_budget_ends_the_wait`, which only the read
  after sees (`left: Some(1000000000), right: None`). Green with it, with the two other rule tests.
- On windows-latest, diag-g243-3 (run 44267440645, 2026-11-18): both arms widen the gap between the clock read and the
  scheduler's look to 200 ms, so the release lands in it, and run the test alone 8 times. Main's rule: 8 of 8 given up,
  `Deadline { stage: Admission }` 3.18-3.21 s after asking, each at the third window: clock at the mark, then not
  runnable. The fix: 8 of 8 answered `Ok(42)` 3.18-3.21 s after asking, each through the second read (clock at the
  mark, not runnable, then the clock risen, `98282160` to `98904657` in the first run), and the rule's tests pass there.
- Without the widened gap, the fix answered 3 of 3 alone on jupiter (Windows x86_64, 32 CPUs), 3.008-3.011 s after
  asking, and the rule's tests pass there. Main's rule fails without it only when the release lands inside the 3 ms
  gap: 1 of 32 on the failing tree, 0 of 28 on main (diag-g243-1), which is what the widened gap makes certain.

## Siblings (reported, unchanged)

The same two reads, in the same order:

- The watchdog's look (`crates/server/src/watchdog.rs`, `look_at` then `read_runs`): a shard's steps, parking and clock,
  then the scheduler for those with none. Its rule asks for a whole window with no movement, so one look taken across a
  shard's run and block can only judge a shard that had sat still the window before it, and that moved inside the look.
  A lone daemon ends its process on that verdict (A-232).
- The anchor's start judgment (`crates/api/src/role/anchor.rs`, `judge_start_of`): the process clock, then
  `slates_rt::process_runnable`, under the same whole-window rule.

Neither has been seen to fail. Each would take the same second read to close.

Closed 2026-11-18 (g251): the watchdog folds a second read of each suspected shard's pulse and clock, taken after the
scheduler's answer, into its look (`after_the_answer` in `crates/server/src/watchdog.rs`), and the anchor's start
judgment reads the process clock again when the scheduler answers a block (`judge_start_by` in
`crates/api/src/role/anchor.rs`). Their tests script the reads, as the rule's own do:
`a_shard_that_moves_between_the_look_and_the_schedulers_answer_has_moved` and
`a_start_that_runs_between_its_clock_and_the_schedulers_answer_has_moved`, each red with the second read ignored.
