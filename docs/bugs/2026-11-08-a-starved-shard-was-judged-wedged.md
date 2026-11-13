# A starved shard was judged wedged (2026-11-08)

## Description

On adacachy, at load 24-33 with fixtures' `CPUWeight=1` units, two anchors killed live daemons after `shard N is wedged:
no step and no CPU progress for 1,072/1,073 ms` (`rungs8/r3k-clean2`: 8 wedged lines, 2 lapses, 86 reconnects, 420 resends,
a 1.78 s outage, SubmitAccepted p99 31.5 s). A-228's rule held that a starved shard "keeps consuming some" CPU. Under a
weight of 1 it can get none for over a window.

## Root cause, measured

The rule had only the thread's CPU clock, and a clock stands still alike for a thread off the CPU and one blocked in a call.

- Fixtures' sampled `rungs9/ab-clean-2`, shard 0 before its wedged line: state `R` on every sample, CPU and run-queue wait
  both frozen from -702 ms to -200 ms, then 745 ms of wait credited at -100 ms.
- Linux adds to `run_delay` only when a task reaches a CPU or leaves the queue (`kernel/sched/stats.h`,
  `sched_info_arrive`, `sched_info_dequeue`), so run-queue wait alone cannot say a thread is waiting now. A SCHED_IDLE
  thread behind 63 pinned spinners stood still for 5.7 s (CachyOS 7.3) and 7.9 s (Fedora 6.19) with state `R` on every
  sample and wait growth of 0; a blocked thread read `S` on every sample.
- macOS' `TH_STATE_RUNNING` is `TH_RUN`, "running or on runq" (XNU `osfmk/kern/thread.h`). No per-thread runnable time
  exists; `ri_runnable_time` is the whole process's.
- Windows' `SYSTEM_THREAD_INFORMATION.ThreadState` read Ready on all 89 samples of an IDLE thread behind 4 TIME_CRITICAL
  spinners (8.9 s, no cycles) and Waiting on all 89 of a blocked one. It holds no ready time. One whole-system snapshot
  took 3.06 ms at p50 and 5.3 ms at most.

## Exact edits

- `crates/rt/src/thread_clock.rs`, `registry.rs`: one reader, extending reland-a207's run state. `RunReading { state,
  waited_ns }`, read in a batch so a beat takes at most one Windows snapshot. The Linux handle carries the tid. The parsers
  are pure and compile on every host. `slates-rt`'s unsafe budget rises by 3, all Windows.
- `crates/server/src/watchdog.rs`: `judge` asks whether the shard moved: CPU rose, runnable at this look, or a wait
  credited. Wedged is a whole window with none. A whole window with no CPU but one of the others is the new `Starved`,
  which beats and is logged once an episode. The reading is taken only for shards with no step and no CPU rise.
- `crates/cli/src/anchor.rs`: the start judgment passes no reading, and treats `Starved` as alive.

## Tests

Red first, on today's rule (`73d200c2`):

| Test | Linux x86_64 | Windows x86_64 | macOS arm64 |
|---|---|---|---|
| Four starved replays | judged wedged at turn 11 | judged wedged at turn 11 | judged wedged at turn 11 |
| Real-thread starved | clock still 2.98 s, beat stopped 2.01 s | 2.99 s, 1.91 s | not forced (86 ms) |
| Blocked in a read | stopped at 1.00 s | 1.11 s | 1.11 s |

- Replays: a wait credited late, a wait that stands still, no wait to read, a shard woken and kept waiting, a starved
  shard that then blocks, and a shard the scheduler never saw runnable (asleep, stopped, no reading).
- Hostile input on the `stat`, `schedstat` and snapshot parsers: a name holding `) R`, truncated lines, `0 0 0`, a snapshot
  cut at every length, counts and offsets past the buffer.
- A frozen process is still killed at its lapse: 1,000 ms on Linux, 1,005 ms on macOS.

## Measured at the extremes (2026-11-08)

CI fleet `diag-a228-5` (run 42488922231, ten repetitions a platform, all passed):

| Platform | CPU clock stood still | Largest beat gap | Blocked in a read or channel: beat stopped | Starved verdict |
|---|---|---|---|---|
| ubuntu-latest | 3,005-3,008 ms | 90.8-91.0 ms | 997-1,009 ms | 10 of 10 |
| ubuntu-24.04-arm | 3,002-3,004 ms | 90.7-91.6 ms | 996-1,009 ms | 10 of 10 |
| windows-latest | 2,985-3,003 ms | 99.1-110.6 ms | 1,023-1,074 ms | 10 of 10 |
| windows-11-arm | 2,985-3,013 ms | 94.0-111.0 ms | 1,000-1,018 ms | 10 of 10 |
| macos-latest | 83-1,465 ms | 166-240 ms | 914-1,253 ms | 1 of 10 |

The run-queue wait did not change in any of the 20 Linux runs, so the state carried the rule there. On macOS the reader read
`Runnable` at every look but a full flat window came once, so its other nine runs exercised the reader and not the rule.
A blocked shard on macOS can log `is starved` as its block ends: the thread wakes and reads runnable, after the wedged
verdict and the stopped beat.

Fixtures' `r3k clean` at `CPUWeight=1` on `47de249b`, run on adacachy by this plan's agent (fixtures was paused): 0 wedged,
0 starved, 0 spinning, 0 lapsed, no flat CPU window of 800 ms, at load 17-22. **The rung did not exercise the rule**, so it is
no evidence for it, and the drop from the baseline is not attributed to it (GAPS has the table).

## Left open

In GAPS, "A starved shard was judged wedged": the spinning kill in `ab-clean-2`, the siblings (`ShardTime`, the anchor's start
judgment), a shard blocked on a starved one, macOS' limit, and `a_spinning_shard_stops_the_beat` at extreme load.

## The spinning test judged by its shard's CPU (2026-11-08, bug-sweep)

`a_spinning_shard_stops_the_beat` gave the spin ten windows of wall. At load 185 on the 18-CPU Mac the shard's thread
got about a tenth of a CPU, so ten windows of wall were one window of its CPU and the beat had not yet had cause to stop
("never stopped in 10 s"). The test now waits on the shard's own CPU clock (`slates_rt::registry::shard_cpu`), the
quantity the rule reads, until the spin has run a window of it, then requires the beat to stand still for a window of
wall; the wall is only a backstop of 60 windows, a sixtieth of a CPU, and a failure prints the CPU it saw.
- Measured interleaved on the 18-CPU Mac with 160 spinners (load 152 to 187): the old test failed 3 of 3, the new
  passed 3 of 3, a window of the shard's CPU taking 11.6 to 12.2 s of wall.
- Red-checked: with the watchdog's spinning verdict disabled, the new test fails ("a shard that ran 16.6 s of CPU
  without a step never stopped the beat", 359 beats after its window).

## The starved test's precondition and its charge (2026-11-08, bug-sweep)

- **The precondition hinged on nice, not on the core count.** On adacachy2 the test failed "did not starve" (the clock
  flat 890 ms at most, the supervisor's run). It pins the SCHED_IDLE shard and its 63 spinners to one CPU on any box,
  so the core count was not the cause. adacachy2 runs ananicy-cpp, which renices `cargo` to 19; a test cargo starts
  inherits it, and spinners at nice 19 weigh 15 against SCHED_IDLE's 3. The same binary under `nice -n 19` failed 3
  of 3 (flat 81-142 ms at most) where at nice 0 it passed 3 of 3 (2.5-3.0 s). 13 of 13 passed at nice 0 before the
  fix. Each spinner now takes nice 0, or the nearest `RLIMIT_NICE` allows (31 there), and the count is derived from the
  kernel's weight of that nice, so the weight against the shard is 63 nice-0 spinners' anywhere. The watch starts once
  every spinner has settled: at nice 19 they run at the inherited nice until they renice, and the shard got CPU then
  (flat 2.31 s at most against 2.9-3.0 settled). After: 10 of 10 there, 5 at nice 0 and 5 at nice 19 (flat 2.3-3.0 s),
  and the whole liveness suite at nice 19, 8 of 8.
- **The beat is charged only to what the rule controls.** The watchdog thread now counts its looks and the looks at which
  it withheld the beat for a stuck shard (`watchdog::Turns`, `Daemon::watchdog_turns`). The test requires looks and no
  withheld one, and holds the beat's gap under a window only where the spinners are pinned to the shard's CPU (Linux,
  Windows). On macOS the spinners cannot be pinned and starve the watchdog's thread too: 12-14 looks in 3 s where 30
  were due, at load 45 on the 18-CPU Mac, which was the 1.146 s gap seen at load 40. Red-checked on adacachy2: a starved
  shard judged wedged withheld the beat at 20 of 30 looks and the test failed saying so.

## The `ShardTime` sibling (2026-11-08, bug-sweep)

`crate::observe::ShardTime::next_window_ns`, which observations and the cross-shard waits (`xshard::within_shard_time`)
judge a shard's time by, ended a wait at the first window with no CPU progress without asking the scheduler. A starved
shard, runnable with its clock standing still, was given up on. `an_observation_of_a_starved_shard_is_answered` starves
the non-control shard as the beat's test does and observes it with a one-window budget: on adacachy2 it ended
`Deadline { stage: Admission }` at 1.0 s, 3 of 3, while the shard's clock stood still 3.0 s. A window with no CPU
progress now ends the wait only if the scheduler does not have the shard runnable (`ShardTime::runnable`, from
`shard_run_readings`). After: answered 6 of 6 there (3 at nice 0, 3 at nice 19); the liveness suite 9 of 9;
`tests/observe.rs`, whose wedged questions sleep and are not runnable, still ends them, 8 of 8. A stored run-queue wait
was left out: Linux credits one only once the thread reaches a CPU, so the state decides, as the watchdog's notes say.

## The transitive wait (2026-11-08, bug-sweep, the supervisor's order)

A shard blocked on another thread that is itself starved never runs and is never runnable, so the rule read it wedged:
on adacachy2 a shard's poll parked on a thread held off its CPU withheld the beat at 30 of 40 looks, gaps 3.05-3.09 s, 3
of 3. The shard's joins of another thread are three (GAPS lists them: the FUSE notify thread, the landing barrier
thread, the WinFsp mount thread). Each is now spawned by `slates_rt::registry::spawn_waited` and joined by
`WaitedThread::join`, which marks the shard's registry entry with the joined thread's clock handle for the join, and
`shard_run_readings` reads a blocked shard as runnable while the thread it waits on is runnable. A joined thread that is
blocked too is still a deadlock. `a_shard_waiting_on_a_starved_thread_keeps_the_beat`: red 3 of 3, green 6 of 6
(adacachy2, nice 0 and 19). `a_shard_waiting_on_a_blocked_thread_stops_the_beat` holds the deadlock.
`a_shard_joining_a_thread_reads_as_the_thread_runs` (`crates/rt/tests/waited_thread.rs`) found the first version's
hole: a join made at once after the spawn ran before the thread had published its handle and was never marked. Such a
join now waits for the handle marked as waiting on a starting thread, which is runnable until it runs. Red-checked with
the consult removed: the shard read blocked while the joined thread spun.

## The anchor's start judgment (2026-11-08, bug-sweep)

The anchor judges a start, before its first beat, in the whole process's CPU time (`slates_machine::process_clock`) and
passed the watchdog's rule no scheduler reading, so a start whose CPU stood still for a window was "made no progress" and
killed, however starved: "the starting daemon made no progress for 1014 ms; killing it" in `held_rendezvous` on CI's
Windows runner. `slates_rt::process_runnable` now answers whether any thread of another process is runnable
(`/proc/<pid>/task/*/stat` on Linux; `proc_pidinfo(PROC_PIDTASKALLINFO)`'s `pti_numrunning` on macOS; one system snapshot
on Windows, walked by `process_thread_states_in`), and the start judgment passes it as the run reading.
`a_start_held_off_its_cpus_is_starved_not_stuck` starts a busy shell at SCHED_IDLE pinned beside spinners (an Idle-class
PowerShell beside High-class ones on Windows) and judges it a look a beat for three windows: with no reading, 2 of 2
judged it "made no progress for 1001 ms" on adacachy2; with it, starved at 14-23 looks and never stuck, 7 of 7 there,
at nice 0 and 19. `process_runnable` itself is held by use on every platform (`a_running_process_reads_runnable`).
