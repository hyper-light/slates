# The anchor never found a wedged daemon once its own waits ran a window late (2026-11-18)

Contracts: §4.14 `daemon.alive` (the anchor kills a daemon whose beat lapsed), A-228 (the lapse bound is the liveness
window plus the anchor's own measured lateness p99), the 2026-10-04 rule of `lapsed` (a look more than the bound after
the one before is taken for the host having slept), A-232 (a lapse with the watchdog silent is judged by the process).

## Description

On a host that delays the anchor's timed waits by more than a liveness window (its lateness p99 past `W`, a machine as
loaded as the ones A-228 and A-232 were measured on), the anchor stopped finding lapsed beats at all. A daemon whose
shard wedged withheld its beat as it should, and the anchor, looking at it all along, never judged the beat lapsed: the
daemon was never killed or restarted, and its clients, which wait while their daemon lives (A-232), waited for good.

Found by g255 while deriving the observation cadence (A-467, goal 85, class D), from the code and then by a replay of
the loop's own arithmetic; no field report.

## Root cause

Two rules met. `lapse_wait` let the observation loop wait up to half the lapse bound between looks at a running daemon
(`OBSERVATION_GAP_PARTS`, a `Shape:` two), whatever the anchor's own lateness. And `lapsed` takes any gap between two
looks longer than the bound for a span the anchor did not watch (a host asleep), and ages the daemon's beat from the
look that ended it. With the bound `W + p99`, a look comes `(W + p99) / 2 + p99` after the last, past the bound once
`p99` passes `W`. Then every look was taken for a resumption: the beat's age restarted at each, and never passed the
bound, so `lapsed` never returned true, and the A-232 judgment behind it was never asked.

A runaway lateness reading made it worse: the bound had no ceiling, so two host pauses among the last hundred waits held
every lapse past the pause, the anchor blind that long even with looks on time.

## Impact

Liveness, on every platform, under heavy load only: a wedged daemon stayed up with its clients' calls outstanding, and a
crashed-but-unreaped one kept its anchor's restart from running. No data at risk. On a quiet host (lateness a tick or
less) nothing changed: the gap stayed well inside the bound.

## Exact edits (A-467, main `ea26f5d1a`)

- `crates/api/src/role/anchor.rs`: `Lateness::lapse_bound_ns` keeps `W + p99` and caps it at A-455's ceiling
  (`slates_server::watchdog::WINDOW_CEILING_NS`); `Lateness::observation_gap_ns`, the bound less the lateness p99 and a
  beat, never less than a beat, replaces `OBSERVATION_GAP_PARTS`; `lapse_wait` takes it. The kill line and the loop's
  banner name the ceiling and the gap.
- Tests: `a_wedged_daemon_is_found_lapsed_though_the_anchors_own_looks_run_twice_a_window_late` (the loop replayed with
  every look two windows late: the old rule never found the wedged daemon; the new one finds it after the bound,
  within the lateness of it) and the runaway case added to `the_lapse_bound_is_the_window_plus_the_anchors_own_lateness`.
  Red-checked with the old rule restored (Mac Studio, kiln r386).

## Siblings (reported, not changed here)

- The daemon's watchdog had the same shape (A-308): a look a window or more after the one before restarted a shard's
  still time (`ShardWatch::judge`, `unwatched`), and the watchdog looks a beat apart whatever its own lateness, so a
  watchdog delayed by a window less a beat at every look never judged a wedged shard. Fixed by A-474:
  `2026-11-18-the-watchdog-never-judged-a-wedged-shard-once-its-own-looks-ran-a-window-late.md`.
- The daemon's reap loop paces its sweep by a fixed tick too (`daemon.rs` `reap_loop`); it is moving to an
  event-driven reaper (A-470's entry), which retires the tick.
