# A shard waiting on the log was judged wedged and ended a lone daemon (2026-11-18, §4.14 `daemon.alive`, A-305)

## Description

A daemon's shard threads are judged by its watchdog (A-228): a shard that stands still a whole window, no CPU progress
and never seen runnable, is wedged, and a daemon nobody supervises ends its own process on that verdict with exit 4
(A-232). Every `log_line!` took the process-wide standard-error lock and wrote under it, a write per piece of the
formatted line. So a shard writing a line behind another thread that held the lock and was kept off its CPU, or behind
a write the log's reader was slow to take, stood still, never runnable (asleep on the lock or in the write), for as long
as the holder took, and past a window the watchdog ended the process. The design had named the hazard for the end line
alone (A-232: "a shard can be wedged in a write to an undrained standard error while holding the lock every log line
takes"), which is why that line is written by a thread of its own.

## How it showed

Seven test binaries exited 4 under load on 2026-11-17: `merge_post_state` (g231, load ~40), `merge_load`, `nfs_hostile`,
`nfs_mount`, `nfs_writeback`, `green_checkpoint` and `fleet` (g240, g246, g230, loads 40 to 50). Every one exited at its
daemons' starts, while several daemons wrote their start lines at once, each with a line cut mid-write at the exit
(`slates-server: shard 0 initialized in 111424 µs, 45949 µs of its CPU, in error: test failed`) and the watchdog's own
end line missing, its writer thread blocked on the same lock for the one beat it is waited for.

## Reproduction

`a_lone_daemon_whose_log_stalls_lives_on_and_serves` (`crates/client/tests/lone_daemon.rs`): a lone daemon in a child
process; a thread of the child holds standard error's lock for three windows while each shard writes a line in a poll.
Unfixed (the route's install removed), the child exited 4 before the stall ended (runner Adas-MacBook-Pro-3, load 52).

## Root cause

A thread whose stillness ends the process waited on the process's log, which other threads and the log's reader
control.

## Fix (exact edits)

- `crates/machine/src/log.rs`: `LogWriter`, an owned thread that writes the lines queued to it, and `LogRoute`, which a
  thread installs so its `log_line!` queues the line and returns at once. The queue is bounded at a pipe's capacity
  (65,536 bytes); a full queue refuses the line, counted in `log_lines_refused`. A line written while a thread's locals
  are torn down goes direct (`try_with`). Every other thread writes as before.
- `crates/server/src/daemon.rs`: the writer is started before the shards, each shard's thread routed before anything it
  logs, and the writer stopped after the shards, waited for a window at most.

## Tests

`a_lone_daemon_whose_log_stalls_lives_on_and_serves` (green 5.7 s; red exit 4),
`a_routed_thread_never_waits_on_standard_error_and_a_full_queue_refuses`.

## Siblings

- Other process-wide locks a starved thread can hold (the allocator's) still wedge a shard behind it; the holder is not
  named (GAPS, the watchdog's entry). Open.
- GAPS 2026-10-22 ("a CI daemon's boot was refused because one shard used no CPU for a whole second while it started",
  sixteen daemons starting in one test process): the same shape, a start burst with the cause not shown. Not claimed
  fixed without a reproduction of that run.
