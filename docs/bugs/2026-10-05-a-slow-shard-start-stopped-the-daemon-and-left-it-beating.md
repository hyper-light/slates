# A slow shard start stopped the daemon, and left it beating

Date: 2026-10-05. Scope: the daemon's boot (§2.6), the control loop's client-identity recovery (§4.9), the anchor's
`daemon.alive` (§4.14).

## Symptom

One macOS recovery run (load average 16–17) failed three restart tests together: each daemon logged
`client identity recovery refused on shard N`, and its clients saw `DaemonGone` or no rendezvous. It did not recur in
the next six runs.

## Root cause

Two defects, one behind the other.

1. Before serving, the control loop asks every shard for its retained client-id high-water mark
   (`restore_client_ids`) within the 1 s liveness budget. The call queues behind the shard's start, `init_shard`,
   which runs recovery as one step. A start longer than a second (a large recovery, or a loaded host) failed the
   call, the control loop returned, and the daemon never served. The deadline could not tell a shard still working
   from a stuck one, or from one whose start had failed.
2. The heartbeat task was spawned and detached before that call. When the control loop returned, the heartbeat beat
   on, so the anchor saw a live daemon that would never serve, and never restarted it.

Why starts got near a second that day: each shard's start is now in the boot log, and in the restart suites starts
were p50 26 ms, p99 811 ms, max 978 ms, nearly all of it A-99's tag store built whole at store construction (268 MB of
tags and a 16.7 M-granule buddy per shard, zeroed by hand on recycled memory, under many parallel daemon starts). That
store now grows lazily (`fb48a27`): p50 0.8 ms, p99 52–76 ms. This record is the deadline and the heartbeat.

## Impact

A daemon whose start outran a second on any shard served nothing until killed by hand, while reporting itself alive.
The trigger was rare on a quiet host and likelier on a loaded one, or with a large recovery image.

## Fix

- Failing tests first (`crates/server/tests/recovery.rs`), with a boot fault the tests inject
  (`DaemonConfig::with_boot_fault`, `BootFault { partition, kind: Busy | Stuck, for_ns }`; `None` in any deployment):
  - `a_shard_whose_start_outlasts_a_liveness_window_while_working_does_not_stop_the_daemon`: shard 1 spins for two
    windows; the daemon must serve a create. It failed: no rendezvous.
  - `a_shard_stuck_in_its_start_stops_the_heartbeat_so_the_anchor_restarts_the_daemon`: shard 1 sleeps for two
    windows; the heartbeat must stop within the client's start wait. It failed: the heartbeat beat on.
- `retained_client_ids` (daemon.rs) asks once and awaits the answer a liveness window at a time. After a window with
  no answer the wait goes on only if the shard's thread used CPU in that window (`registry::shard_cpu` progress moved),
  so a working start is waited out and a quiet shard is refused after one window. An answer with no state (a failed
  start), a refused call, or an unreadable clock refuses.
- The heartbeat is spawned as the control loop's child (`futures::spawn_child`), not detached: the runtime cancels and
  joins it when the loop returns or is cancelled. A refused boot returns, so beating stops and the anchor restarts the
  daemon. A first version kept the heartbeat joinable until the boot was accepted and detached it then; a shutdown
  that cancelled the control loop before that left a joinable perpetual task, and its shard never finished stopping:
  `an_acknowledged_replica_survives_a_warm_daemon_restart` hung in `Daemon::stop` on every run (a stack sample showed
  the test joining a shard thread parked in `kevent`), and passed 3 of 3 with the heartbeat detached at once and 5 of
  5 with the child.

Edits: `crates/server/src/daemon.rs` (`retained_client_ids`, `restore_client_ids`, `control_loop` (the heartbeat a
child), `hold_start`, the start-time log line), `crates/server/src/config.rs` (`BootFault`, `BootFaultKind`, `boot_fault`, `with_boot_fault`),
`crates/server/src/lib.rs` (re-exports), `crates/server/tests/recovery.rs` (the two tests, and a stop during a long
start).

## Siblings

- The control shard's own start blocks its heartbeat, which runs on the same shard; a control-shard start longer than
  the anchor's liveness budget is killed by the anchor. That is the anchor's rule working (a restart follows), but a
  recovery that legitimately takes longer than the budget on the control shard would loop. Not changed here; recorded
  in GAPS.
- Other cross-shard calls under a deadline (`call_within`: fleet.rs 10, verbs.rs 8, landing.rs 4, reap.rs 3,
  merge_service.rs 1) were read for the same failure: the fleet ones run in periodic loops (record periods,
  materializing greens, delivering pairs, retiring tombstones), where a missed answer is skipped or defaulted and asked
  again next period, and the rest serve a verb, which refuses typed. None ends a loop or the boot on a miss, so this
  was the only call whose miss was fatal.
