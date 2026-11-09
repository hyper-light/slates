# A daemon killed while serving was counted toward the crash loop (2026-11-09)

## Description

`a_winfsp_drive_handle_and_view_survive_twenty_killed_daemons` failed on main 80be552a on both Windows runners (run
42528348387 on arm64, run 42609672766 on x64). At kill 20 the anchor logged "slates anchor: the daemon is in a crash
loop (last exit Some(1)); restarting it in 16000 ms" and the test gave up at 10 s, though each of the 19 earlier
successors had recovered and served ("completed 19/20, drives lost 0"). Whether it passed depended on the measured
start p99, since the bound is the recovery budget over it.

## Root cause

`Supervisor::step` (`crates/anchor/src/supervise.rs`) pushed every exit into the window's restart times and judged a
crash loop once more than `max_restarts` fell inside it, whether or not the daemon had finished its start. The module's
rule is about a daemon that "is not recovering": one that served before it exited was recovering.

## Exact edits

- `crates/anchor/src/layout.rs`, `segment.rs`: the supervision block's serving word (`SUP_SERVING`, offset 208, a
  declared word): `Supervision::mark_serving(now_ns)` and `serving_since()`, zeroed by `record_start`. Zero in a block
  an older daemon wrote, so every exit under one counts, as before; no layout change.
- `crates/cli/src/daemon.rs`: the daemon marks it once `Daemon::start_with_fleet` returns (recovery included), where
  it logs "serving on N shards". Not its first beat: a recovering daemon beats, since its recovery runs in slices.
- `crates/anchor/src/supervise.rs`: an exit after the mark is restarted at once, uncounted, and the backoff returns to
  the window; an exit before it counts toward the bound as before.

## Tests

`crates/anchor/tests/anchor.rs`, over the re-invoked child:
- `a_daemon_killed_after_it_served_is_restarted_every_time`: a child that beats, marks serving and exits, bound 3,
  six exits: each restarted. Red before the fix (3 restarts, then `CrashLoop` at the fourth exit).
- `a_crashing_daemon_is_restarted_until_the_derived_bound_and_the_segment_says_so` (a child that never serves) still
  trips the bound after three; it fails with the guard forced to skip every exit.
