# A stop asked by a sender that exited: kiln saw its instance serve on for two minutes; slates' own stop does not wait on the sender

## Report

kiln's real-instance test (K5, `crates/kiln/src/workspace/captured_tests.rs`, g267, kiln runs r407, r443 and r457 on
the Mac Studio, 2026-11-18) starts a Slates anchor through `StartSpec::anchor_command`. The anchor runs as kiln's own
executable in Slates' roles, in a process group of its own (`spawn_detached`), its standard output and error appended
to a log file in the test's temporary directory, and the launcher's secret on its standard input. At its end the test
ran `kill -TERM <anchor>`.

- **r407.** The test sent the signal and returned at once, so its temporary directory went with it. The anchor and
  daemon were seen still serving about two minutes later. The anchor's log was in that directory, so none was kept.
- **r443 and r457.** The test sent the signal and waited, polling `Instance::attach` until it failed. The instance
  stopped 204 ms and 176 ms after the signal.

## What a sender's exit could change, read in the code

- **A parent's pipe closing.** The roles' lines are written by `slates_machine::log_line!`, which never panics. A line
  refused `EPIPE` is counted and dropped (`crates/machine/src/log.rs`), and a daemon's shard threads write through a
  bounded queue that never waits. kiln wrote the anchor's lines to a file, not a pipe. So a closing pipe changes
  nothing.
- **A process group.** The anchor leads its own group (`StartSpec::anchor_command`, `in_its_own_group`). Nothing signals
  a group when its starter ends, and the stop is the anchor's `SIGTERM` alone.
- **The anchor waiting on its child.** `stop_gracefully` steps the supervisor every heartbeat. `Supervisor::stop_step`
  kills the daemon once its beat is a window past the stop request with no acknowledgement, or a window past the
  deadline it declared (`stop_overdue`), or once its beat lapses. So the anchor ends any stop within its own bounds
  while its loop runs. That loop waits in `poll` between looks, at most its lapse bound, and a signal ends that wait.
- **A stop that waits on a client's acknowledgement.** The daemon's stop asks no client anything. It drains
  leadership, gives back green replicas (`give_back_replicas`, up to the observation budget, A-457), shuts the runtime
  down and stops the log writer. None of those steps reads a client.

## The reproduction

`an_anchor_whose_stop_was_asked_by_a_sender_that_exited_stops_within_its_bound` (`crates/api/src/role/stop_tests.rs`)
re-invokes the test binary as a sender, which:

1. starts the liveness tests' anchor role as its child, in a process group of its own;
2. connects a client;
3. sends the anchor `SIGTERM`;
4. exits at once, without waiting for the anchor and with no destructor run.

The test then watches the anchor and its daemon by process id alone, with no client of its own. Results on the Mac
Studio d1323898 (kiln runs, 2026-11-18, debug build):

| sender's client at its exit | anchor and daemon gone after the signal | their lines |
|---|---|---|
| connected, nothing else | 283 ms | "stop requested; drained", then "stopped (daemon exit Some(0))" |
| a volume made, attached for writing and written, the attachment held | 191 ms | the same |

The waiting test's 0.2 s, without waiting. So slates' own stop does not wait on the process that asked for it.

## What kiln's run had that this one has not

Not yet tested:

- the anchor run as kiln's executable, with its allocator and thread-stack setup before the role;
- the launcher's secret on standard input, and `until_served`;
- the anchor's log in a file;
- the K5 scenario's work: captured clones over a host folder, and their bases;
- the test's temporary directory removed right after the signal. That folder holds the base the volumes overlay,
  kiln's state for the instance, and the anchor's log;
- whatever kiln's runner does with a run's leftover processes on the Studio, such as its headroom pacer stopping
  processes and starting them again.

## What would find it

kiln's K5 test in its fire-and-exit shape, with the anchor's log kept outside the temporary directory. For the two
minutes after the signal, also take `ps -o pid,stat,wchan` of the anchor and daemon and a `sample` of each. The anchor's
and daemon's lines then say whether the stop was asked, acknowledged and drained. The process states say whether
either was stopped, blocked in the kernel, or running. Filed for main in `run-requests.md`.

## Changes

- `crates/api/src/role/stop_tests.rs`: the test and its sender role, the guard of the rule that a stop completes within
  its bound whether or not the sender stays.
- `crates/api/src/role/liveness_tests.rs`: the roles' constants and helpers, shared with it (`pub(super)`).
- `crates/api/src/role/mod.rs`: the module.
