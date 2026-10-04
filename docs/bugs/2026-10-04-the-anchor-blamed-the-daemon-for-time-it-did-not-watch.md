# The anchor blamed the daemon for time it did not watch (2026-10-04)

## Description

Investigating a daemon restarted mid-workload ("the daemon's heartbeat lapsed; killing it", 4 runs of 4), I found
the trigger was my own `vmmap`: it suspends its target while it walks the address space, and on this daemon (a
194 GB sparse content object, 11 GB reserves per shard) the walk took 1.03 s, past the 1 s liveness budget. That kill
is correct: the anchor watched a daemon that did not beat for a whole budget.

The same code path showed a real defect. The anchor aged the heartbeat from the beat alone. Both processes' clocks
include time the host spends asleep (on purpose: a paused owner's lease must lapse). So after a laptop sleep, an
anchor that observed before the daemon's next beat saw a heartbeat as old as the sleep, and killed a healthy daemon.

## Root cause

`crates/cli/src/anchor.rs`'s observation loop judged `daemon.alive` as `now − last beat ≤ budget`, with no account
of the anchor's own gaps. A span the anchor did not observe (the host suspended, the machine paused, the anchor
starved) was charged to the daemon.

## Impact

On a laptop, every wake from sleep could restart the daemon. That drops in-flight NFS state and stalls every
mount while the daemon recovers.

## Exact edits

- `lapsed(watch, beat, now, budget)`, a pure function. When the anchor's own gap between observations exceeds the
  budget, it records the resumption, and the heartbeat is aged from the later of the beat and that resumption. A
  daemon is killed only after a whole budget the anchor watched without a beat.
- The loop keeps the segment's own conditions (running, beaten at least once) and takes the age from the watch.
- Tests:
  - `a_host_that_slept_does_not_kill_the_daemon_it_could_not_watch`: fails with the old aging;
  - `a_daemon_that_stops_beating_under_a_watching_anchor_lapses`: the kill still comes within a budget plus one
    observation turn.
