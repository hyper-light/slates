# The watchdog never judged a wedged shard once its own looks ran a window late (2026-11-18)

Contracts: §4.14 `daemon.alive` (a shard still for its window is wedged, A-228), A-308 (a look a window after the
watchdog's last one starts every shard's still time again), A-455 (each shard's window), A-232 (an anchored daemon's
watchdog withholds its beat on a stuck verdict; a lone one ends its process).

## Description

On a host that delays the watchdog's own thread by about a window at each look (the same load the anchor's sibling,
`2026-11-18-the-anchor-never-found-a-wedged-daemon-once-its-own-waits-ran-a-window-late.md`, needs), a shard that
wedged was never judged: the watchdog went on beating, the anchor saw fresh beats and killed nothing, and a lone daemon
never ended itself. Its clients, who wait while their daemon lives (A-232), waited for good. The anchor's own start
judgment and its judgment of a silent watchdog's process run the same rule (`ShardWatch::judge`) and had the same hole
under the same lateness of the anchor's looks.

Found by g255 from the code while recording A-467's bug, then held by a replay of the rule; no field report.

## Root cause

A-308 took any gap of a window or more between two of the watchdog's looks for a span it did not watch (the process
frozen or paging whole), and restarted every shard's still time at the look that ended it. The watchdog looks a beat
apart (`recv_timeout(HEARTBEAT_NS)`), so once its own lateness passed a window less a beat, every gap was a window or
more: every look restarted a wedged shard's stillness, its still time never reached its window, and no verdict came.
Across such a gap the shard's CPU clock and scheduler state still tell whether it moved (a thread leaves the run queue
for a block only by running, which its clock shows), so a gap the watcher's own lateness explains was no unwatched span.

## Impact

Liveness, on every platform, under heavy load only: a wedged shard kept its volumes unserved and its daemon up, the
anchor's and the lone daemon's ends both never came. No data at risk. On a quiet host nothing changed: a look comes a
beat and a tick late, far inside a window.

## Exact edits (A-474)

- `crates/server/src/watchdog.rs`: `slept_past_ns(window, watcher_late)`, the window plus the watcher's own lateness
  p99, never past A-455's ceiling nor under the window, is where A-308 now starts a shard's still time again; `Look`
  carries `watcher_late_ns`; the watchdog keeps its looks' lateness in `LatenessTail` (its last hundred, nearest-rank
  p99), the readings the anchor keeps of its waits too.
- `crates/api/src/role/anchor.rs`: the anchor's `Lateness` keeps its waits in that `LatenessTail`, and its start
  judgment and a silent watchdog's process judgment pass its p99 into each look.
- Test: `a_shard_is_judged_wedged_though_the_watchers_own_looks_run_a_window_late` (a shard still behind looks a beat
  and a window apart, by a watcher whose p99 says so: wedged at the first late look; gaps past the ceiling under a
  runaway reading still start its stillness again, A-308).
