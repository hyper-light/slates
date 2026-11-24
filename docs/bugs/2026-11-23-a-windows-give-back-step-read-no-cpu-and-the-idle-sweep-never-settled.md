# A Windows give-back step read no CPU, and an interrupted give-back waited a whole interval (2026-11-23)

## Description
`idle_sweep::tests::an_idle_shard_runs_no_sweep_pass` and `a_write_on_a_settled_shard_arms_a_pass` failed on CI's
Windows runners with "the first pass ran and settled the shard". Both failed on windows-11-arm in every main run read.
On windows-latest both failed in run 45176536354 and one failed in 45210822121. They pass on macOS and Linux. Each
waits up to sixty sweep intervals for the shard's first sweep pass to settle it.

## Root cause
There are two causes, and each was found by logging every look's state.

**A give-back step read no CPU on Windows** (diag run 45238179939, windows-latest). The first pass ran 332 idle purge
steps in about 3 ms and gave back nothing (`content.purged_bytes` 0, `recovery.returned_bytes` 0). Every step left
more. After that it ran one step a pass, and the give-back slice stayed 0, so each step walked one page. The walk was
the emptied write log's. A daemon opens that log over the whole area a predecessor may have written, and walks it page
by page. A give-back step is sized from the rate the steps before it measured on the shard thread's own CPU clock
(A-226, A-491). On Windows that clock is `GetThreadTimes`, which the scheduler charges one tick at a time (15.625 ms).
So a step of a few microseconds read no CPU unless a tick fell inside it, and taught the rate nothing. The rate folds
steps together until one shows CPU, which took thousands of one-page steps.

**An interrupted give-back waited a whole interval, and a look once an interval locked in phase with it** (diag run
45275623386, with the first cause fixed). The slice is now learned within eight steps: about 11 MB a step for a 13 µs
quantum on windows-latest. One uninterrupted pass walks the rest of the log in 2.6 to 5.9 ms. But a give-back stops at
the first step across whose yield another task ran (A-226), and until now the rest waited for the next pass, armed a
whole interval after this one ended. The test looks once an interval, and each look is a task on the shard. A look cut
a pass short. The next pass was armed one interval after that cut, and the next look came one interval after the last
one. On Windows both wake on the same 15.6 ms scheduler tick, so they met again every time. The failing test's log is
60 passes of "1 steps in about 130 µs, ended OthersRan", one at each look. Of the 1.43 GB walk, 680 MB was still left
at the 60-interval bound. windows-11-arm showed the same one-step passes, but its timing drifted out of the lock after
a few passes, so it passed, in 11.7 to 27.7 s a round where one uninterrupted pass needs milliseconds. On Linux and
macOS the thread clock is exact and the timing drifts.

## Impact
On Windows, an idle shard's give-back of its emptied write log and of dead recovery memory crept forward a page at a
time. On every platform, anything that ran once a sweep's interval could cut an interrupted give-back to one step a
pass, for example a client polling once a second, and on Windows the shared tick kept it locked there. The shard kept
arming passes, waking every interval, and did not reach the settled state the idle sweep exists to reach. Both
idle-sweep tests kept the Windows server lanes red.

## Exact edits
- `crates/server/src/daemon.rs`: `shard_thread_cpu_ns` is a pair of `#[cfg]` functions. On Windows it reads the
  thread's cycles at the rate the runtime measures once a process, before any shard runs
  (`slates_machine::own_cpu::own_cpu_ns`, the clock the long-poll attribution reads, A-871). Elsewhere it reads the
  thread clock as before. `GiveBackRate::fold`'s doc no longer names Windows' clock as coarse.
- `crates/server/src/daemon.rs`: `purge_if_idle` returns how the give-back ended: `GiveBack::{Settled, Busy,
  Interrupted}`. A-226's rule is unchanged: a give-back still stops at the first step across whose yield another task
  ran. `seal_idle_content` returns a `PassEnd`: `Settled`, `Owed`, or `GiveBackLeft`. `resume_give_back` runs the rest
  of the give-back.
- `crates/server/src/idle_sweep.rs`: a pass whose give-back was interrupted stays under way and resumes the give-back
  at the shard's next idle point. There, `idle_work`, the shard's idle work, wakes the sweep (counted as
  `content.give_back.resumed_at_idle`). The interval now paces only a fresh pass after one that owed it. Under
  sustained load there is no idle point, so the give-back waits. `sweep.passes_started` counts passes.
- `crates/server/src/daemon.rs`: the shard's start registers `idle_sweep::idle_work` in its one idle-work slot. A
  shard that cannot register it is a typed initialization failure.
- `crates/server/src/content_ranges.rs`: arming a content-range sweep no longer takes the idle-work slot. It sets the
  sweep running, and `idle_work` advances it through `idle_slice`, as the slot did before.
- `crates/rt/src/shard.rs`: with the idle work registered from the start, its state borrow runs before every park, and
  a borrow's hooks may wake a task. The loop parked as soon as the idle work answered that it did nothing, the woken
  task left ready on the shard's own queue until the park's deadline or a kick, with none on a shard with no timer
  armed. The loop now runs such a task, and every park begun with a task ready is counted (`parked_with_ready`).
  Found on run 11's windows-latest suite: `deadlines::tests::a_lapsed_cli_attachment_ends_at_its_term_without_a_tick`
  failed once ("the attachment ended within half a liveness window"), which only a late keeper fails.
  `crates/rt/tests/idle_work.rs` gains `idle_work_that_wakes_a_task_leaves_it_to_run_not_to_wait_out_a_park`, and
  every deadlines and idle-sweep test of the daemon asserts the count stays 0.
- `crates/server/src/state.rs`: `give_back_waits_idle`.
- `crates/server/src/deadlines.rs` and `idle_sweep.rs`, test support only: the keeper wakes a test's poller beside each
  pass it starts (`TEST_POLLER`).
- `crates/server/src/daemon.rs`, a new test: `the_give_back_clock_reads_a_step_of_microseconds`. Eight spins of 50 µs on
  the clock a give-back step is timed by, each read as its CPU. It is the cycle clock's own red, apart from the sweep:
  with the thread's charged time on Windows such a step reads none.
- `crates/server/src/idle_sweep.rs`, a new test:
  `a_pass_cut_short_at_each_start_settles_in_that_pass_at_the_shards_idle_points`. A poller that the keeper wakes with
  each pass cuts every pass short at its first step on every platform, as the look did on windows-latest's tick. The
  two original tests stay as written and are the red on windows-latest.

## Measured and rejected
- A first clock fix timed a step by its cycles, scaled by the thread's own charged time per cycle, read from the same
  thread clock. A shard thread charged no tick yet has no time to scale by, so its steps still taught nothing. With it
  both idle-sweep tests failed on windows-latest, and each failing test's shard had started with "0 µs of its CPU"
  (diag run 45260798132). The measured process-wide rate has no such gap.
- The cycle clock alone (diag runs 45269168513 and 45275623386): 5 of 5 rounds on windows-11-arm, but 3 of 5 and then
  4 of 5 on windows-latest, which is the phase lock above.
- Pacing the test's looks off the interval would have hidden a starvation a client polling once a second causes in the
  field. Ending a pass only when other work ran across two yields in a row would have changed A-226's rule and doubled
  the sweep's intrusion under sustained load. Main ruled both out on 2026-11-23.

## Evidence
- Red, the clock (diag run 45269168513, before A-881): both idle-sweep tests failed on both runners with the cycle
  clock reversed. With A-881 in, the sweep tests no longer depend on the clock (diag run 45286588671: all three passed
  with it reversed on both runners), so `the_give_back_clock_reads_a_step_of_microseconds` is the clock's own red.
- Red, A-881 (diag run 45286588671, with the clock in): the drift-free test failed with the resume turned off on
  windows-latest (111.6 s), windows-11-arm (72.8 s) and ubuntu (70.4 s).
- Green (the same run): the idle sweep's tests 5 of 5 rounds on windows-latest (10.5 s a round, where rounds took 35
  to 124 s and failed 2 of 5 before), windows-11-arm (10.8 s) and ubuntu (11.4 s); macOS 4 of 4 (25.9 s). The server
  unit suite on both Windows runners: 1,197 passed, the 2 failures A-877's store tests (w-perf's, not this change).
  Windows clippy of the server failed only on A-877's arena_windows lints (fixed on main by 80ef30c1e).
- Red, the lost wake: `idle_work_that_wakes_a_task_leaves_it_to_run_not_to_wait_out_a_park` failed without the run
  loop's change on macOS (2026-11-23) and on ubuntu (diag run 45309498515): the woken task did not run within 2 s
  (idle_work.rs:198). Green with it (the same run): every rt test binary on ubuntu and macOS, loom 6 of 6, and rt
  clippy for x86_64 and aarch64 Linux and Windows. CI's Miri set on slates-rt (diag run 45310233924): the library's
  68 tests, with the OS runtime's sync tests skipped as on main, and the alone, differential, ownership, readiness and
  timers files.
- Not this change. `idle_spin` failed two of its three tests on kiln's adacachy at a load average of 35.5, its control
  arm lost to a runner restart, and passed 5 of 5 rounds with the change and 5 of 5 without on ubuntu (diag run
  45309498515). `spin_kick`'s `a_kick_the_spins_harvest_takes_still_gets_its_ring_served` failed once on macos-latest
  with the change (the same run), then passed 10 of 10 rounds with it and 9 of 10 without it (diag run 45311228860):
  an older race, recorded open in GAPS.
- The first build of the drift-free test's poller was itself the load that held the pass off: it took its baseline
  after the pass's first step and took turns until a second, which a pass waiting for the idle point never took (macOS:
  "running true", one idle step, nothing resumed, at the 60-interval bound). It now stops at the first step since it
  last looked.

## Sibling, reported
The emptied write log is opened over the predecessor's whole area (`written = capacity`), and walking it gives back
almost nothing when the predecessor wrote little: 12,288 bytes purged for a 1.43 GB walk on windows-latest. With the
idle-point resume that walk takes milliseconds, so a residency-aware walk (A-744's territory) waits; it is recorded in
GAPS.
