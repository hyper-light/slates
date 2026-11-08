# The Linux process clock read the main thread alone (2026-11-07)

## Description

`slates_machine::process_clock::process_cpu` on Linux read `/proc/<pid>/schedstat`. That file is per task: for a
process it holds the main thread's run time (`task->se.sum_exec_runtime` in the kernel's `proc_pid_schedstat`), not the
whole process's. The anchor judges a starting daemon by this clock (`docs/bugs/2026-11-07-the-anchor-killed-every-start-a-busy-machine-starved.md`).
So on Linux a start whose main thread waited on its shard and watchdog threads for a window, for example on a slow
recovery, read as making no progress and would be killed as stuck. macOS (`proc_pidinfo` task info) and Windows
(`GetProcessTimes`, `QueryProcessCycleTime`) read the whole process.

## Root cause, measured

On fedora (kernel 6.19.10, x86_64), a process whose second thread spun for half a second:
`/proc/<pid>/schedstat` moved from 17.55 to 17.73 ms (the main thread waking), while `/proc/<pid>/stat`'s utime moved
from 22 to 72 ticks. The existing self-reading test passed on CI by chance: libtest's main thread wakes whenever another
test finishes. Run alone on fedora it failed too.

## Exact edits

- `crates/machine/src/process_clock.rs`: Linux reads the process's CPU-time clock, `clock_getcpuclockid` then
  `clock_gettime`: the nanoseconds all its threads have run, exact. The kernel lets any process read another's process
  clock (`lookup_task` with `gettime`).
- `unsafe-budget.toml`: `slates-machine` 61 → 63, the two calls named.

## Tests

`a_process_whose_work_runs_off_its_main_thread_shows_its_cpu_rise`: a child of the test binary whose main thread waits
while another thread spins. On fedora with the old clock it failed 3 runs in 3 (0.77 ms, flat, a span apart), as did
`a_process_that_runs_shows_its_cpu_rise`. With the fix both pass 3 in 3, and on this Mac.
