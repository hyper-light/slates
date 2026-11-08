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
