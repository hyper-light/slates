# Two fleet tests held a lone daemon as A-232 ends one (2026-11-17, A-232's landing; §4.14)

Found by the landing's own CI (PR #10, `p/land-a232` at `8a74e1a3`): the `fleet` test binary ended with exit code 4 on
the macOS gates lane, and on the Windows lane, where each fleet test runs in a process of its own, two tests failed that
main's last five Windows nightlies passed. Test faults, not the product's: A-232's rule did what it says.

## Description

A-232 ends a daemon no anchor supervises (`SegmentSource::Create` or `Handoff`) on its watchdog's stuck verdict: a shard
that moved in no way for a whole liveness window. Every daemon of `crates/server/tests/fleet.rs` is such a daemon, in
the test's own process. A-232 moved the tests that hold a shard past a window into supervised children (`observe`,
`nfs_hostile`, `liveness`, `cross_shard_wait`, `recovery`), and made `Daemon::starve_control_shard` run as a starved
thread runs, a little CPU each beat, so the watchdog sees it move. Two fleet tests it did not reach:

- `a_silent_owner_leaves_no_forward_the_lane_admitted_unrun` holds node a's non-control shard silent for two windows
  with a sleep inside one observation. Asleep, the shard moves in no way, so a's watchdog judged it wedged a window in
  and ended the process: "shard 1 is wedged, unmoved for 1093 ms against the 1000 ms window" (macOS), exit 4 on both
  lanes. In the macOS lane that killed every fleet test still running with it.
- `a_slow_first_round_candidate_is_hedged_after_the_measured_p95` starves the first-round candidate's control shard for
  `SLATES_TEST_HEDGE_STARVATION_SECS` (20 s on CI), then polls the owner until the hedged seal places. With
  `SLATES_FLEET_TRACE=1` (CI), the poll's trace reads every daemon it watches, each within one liveness budget
  (`755e6924`, so a trace never waits out a held shard). That budget is the observed shard's own CPU time (A-65). The
  spinning hold before A-232 spent it in a second of wall time; the starved hold spends 2.5 ms of each 100 ms beat over
  20 s, so the read of the held candidate waited out the whole hold, and the test saw the seal placed at 20.016 s
  against its 20 s hold.

## Fix

- `slates_server::daemon::starve_this_thread`: the starved loop `starve_control_shard` ran, as a function of its own,
  which `starve_control_shard` now calls. One loop, two callers.
- The silent-owner test runs it in its observation in place of the sleep: the shard steps nothing else for the two
  windows, silent to b as before, and its watchdog sees it move.
- The hedge test's poll watches the daemons the hold leaves running, not the held candidate; `describe`'s note says why
  a held daemon is left out.

## Siblings (reported, not changed)

Tests that hold a shard asleep for exactly one window in a daemon no anchor supervises:
`crates/client/tests/acknowledge.rs` (`HOLD`, two holds), `crates/server/tests/ack_release.rs` (`HOLD`, `hold`) and
`crates/server/tests/forward_retry.rs` (`HOLD`). The stuck verdict comes at the first look a window after the shard last
moved, which can fall before a one-window sleep returns, so each such hold can end its binary now and then. On the same
macOS run `acknowledge` passed 2 of 2 and `ack_release` 6 of 6; `forward_retry` did not run (the fleet binary ended
first). A-232's own held-daemon tests hold half a window (`HOLD_PARTS_OF_WINDOW`) for this reason.
