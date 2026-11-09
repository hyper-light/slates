# The start judgment held a process's summed CPU to one thread's window (2026-11-08)

## Description

On main 92dcd16c, `slates anchor --quick` crash-looped on the 32-CPU Mac Studio (adas-mac-studio.local-2, load 2.7):
"the starting daemon ran 1585 ms of CPU without beating (the 1000 ms liveness window)", then 1,630 ms and so on, while
23 shards each initialized in about 70 ms, in parallel (perf's run, `~/.cache/slates-agents-scratch/perf/runs/w1ab-main-1000-1/anchor.err`
on the Studio). Reproduced on this 18-CPU Mac: 18 shards came up, 48 and 96 were killed every start (release build,
`--quick`, about 1,350 ms of CPU each, before any shard had initialized).

## Root cause

The anchor's start judgment (`crates/cli/src/anchor.rs` `judge_start`, A-228's, 2026-11-07) read the daemon process's
CPU (`slates_machine::process_clock::process_cpu`), the sum of all its threads, and held it to the watchdog's spinning
rule over one liveness window. The rule is a thread's: a window of CPU on one thread without a step. A start whose
shards begin in parallel sums past a window while no thread comes near one, so the more CPUs a machine has, the surer
its every start is killed.

## Exact edits

- `crates/machine/src/process_clock.rs`: a reading carries `busiest_thread_ns`, the most CPU any one thread of the
  process has run: Linux each `/proc/<pid>/task/<tid>/schedstat`'s first field; macOS `proc_pidinfo(PROC_PIDLISTTHREADS)`
  then `PROC_PIDTHREADINFO` per thread (same user, no entitlement); Windows a ToolHelp snapshot's threads, each opened
  with the limited query right for `GetThreadTimes`. `None` where the threads cannot be read.
- `crates/cli/src/anchor.rs`: `judge_start` judges spinning on the busiest thread's CPU and wedging on the process's
  progress, as before. Where the threads cannot be read nothing is judged spinning; a start making no progress for a
  window is still killed. The kill line now says "on one thread", and the boot line says whether threads are read.
- `Cargo.toml`: windows-sys's `Win32_System_Diagnostics_ToolHelp`. `unsafe-budget.toml`: slates-machine 63 to 70, each
  site named.

## Tests

- `crates/cli/tests/cli.rs` `a_start_whose_shards_sum_past_a_window_comes_up`: measures what a shard adds to a start
  (a start of as many shards as the machine runs at once, less a start of one), starts an anchor with as many shards as
  sum past three windows, and holds it to serving with no start killed. Red on main (52 shards here: never served,
  "ran 1671 ms of CPU without beating"); passes (69 shards here). It looks for the serving line anywhere in a line, since
  the anchor's and daemon's lines share one pipe and arrive glued.
- `crates/machine/src/process_clock.rs` `a_process_whose_threads_spin_reads_its_busiest_thread_below_its_sum`: a child
  with four spinning threads reads a busiest thread above nothing and below the sum.
- `a_stuck_start_is_killed_and_restarted` and `a_starved_start_that_makes_progress_is_not_killed` pass unchanged.

## Siblings

- No other reader of `process_cpu` holds it to a window: `crates/cli/tests/cli.rs`'s `StartProgress` sums progress only
  to see that a daemon moves, which is what a sum can say.
- A start's main thread still can be killed for a window of CPU of its own before the first beat, which is the rule.
