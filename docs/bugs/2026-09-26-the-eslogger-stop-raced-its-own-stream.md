# The eslogger stop raced its own stream and cut the landing out of the trace

Date: 2026-09-26. Contracts: AC-9.7 (hermeticity evidence), R1/R10 (the tracer proves no disk write
outside a granted landing). Found by CI run 36275755772 (`4cea8a4`, macOS conformance).

## Symptom

The macOS hermeticity suite failed its verdict with `123 write-capable calls: 0 inside the granted
target (0 matched to Written)`. The landing had reported 6 entries written, and disk verification of
the target passed. The previous runner run (36275244114, `e7eac87`) passed the same suite with 18 calls
inside the target and 6 matched to Written.

## Root cause

`EsLogger::stop` counted the log's lines, ran the binary once as a drain marker, and stopped eslogger
as soon as the log had *more* lines than before. Any line arriving counted, including a lagging
event from earlier in the lifecycle. eslogger was then sent `SIGTERM`, and the events it had not yet
written were lost.

Evidence, from the kept traces (the `conformance-anchor-logs-macos-latest` artifacts of both runs):

| Run | Kept lines | Event types (ES numbers) | Lines naming the target | Last kept event |
|---|---|---|---|---|
| 36275244114 (pass) | 1,094 | open, close, create 6, link, rename, setmode, unlink, write | 36, incl. 6 creates at 22:17:45.458 | 22:17:45.647 |
| 36275755772 (fail) | 734 | open, close, write only | 2 (the plan's open and close, 22:28:14.969–.978) | 22:28:14.980 |

The failing trace ends 2 ms after the landing plan's close, before any event of the grant or the
`land --grant` that followed it. `global_seq_num` was continuous, so the kernel dropped nothing: the
harness stopped the tracer too early.

## Fix

- `xtask/src/conformance/hermeticity.rs`: the marker is now a child whose process id is known
  (`run_marker`), and the stop waits until an event *of that process* is in the log
  (`has_event_of_process`).
- Endpoint Security hands the client its events in `global_seq_num` order, and the filter already
  fails the run on any gap. So once the marker's event is logged, every event before it (the landing
  and the daemon's teardown) was delivered.

## Tests

- `the_eslogger_stop_waits_for_the_markers_own_event`: the failing run's last kept line, a lagging
  daemon close, does not count as the marker's arrival. The marker's own event does, and a torn line
  never does. Under the old rule (any growth) the first case counted as caught up.
- The macOS hermeticity lane proves the rule on the next CI run.

## Edits

`xtask/src/conformance/hermeticity.rs`, this record, `docs/wip/TBD_FIXES.md`.
