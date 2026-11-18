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

## Siblings, changed in the same push

Three more tests held a shard asleep for exactly one window in a daemon no anchor supervises:
`crates/client/tests/acknowledge.rs` (`HOLD`, two holds), `crates/server/tests/ack_release.rs` (`HOLD`, `hold`) and
`crates/server/tests/forward_retry.rs` (`HOLD`). The stuck verdict comes at the first look a window after the shard last
moved, which can fall before a one-window sleep returns, so each such hold could end its binary now and then. On the
same macOS run `acknowledge` passed 2 of 2 and `ack_release` 6 of 6; `forward_retry` did not run (the fleet binary ended
first). A test that can end its own binary now and then is a failure, so each now takes the form its assertions need:

- `acknowledge.rs`, both holds: the starved loop for the whole window. The acknowledgement goes out once a count of
  calls is due (half the command ring's slots, which the machine's measured rate sizes), and must go out within half
  the hold, so the hold keeps its whole window.
- `ack_release.rs`, `hold`: the starved loop for the whole window. `check_typed_refusal` reads half of `HOLD` as a
  part's half-window wait for room, so `HOLD` stays a window.
- `forward_retry.rs`: half a window, asleep, A-232's own form (`HOLD_PARTS_OF_WINDOW` in the client's held-daemon
  tests). Its hold only has to outlast the flood and the forward's first try, microseconds; the answer's own bound
  stays a window.

`ack_release.rs`'s `hold_until_answered` was never a wedge: it waits in 10 ms turns, and each wake shows CPU progress.

## Two more, from the landing's Linux run of every binary

`diag-a232-land-1` ran every test binary with `--no-fail-fast` on both Linux runners, past main's known red that stops
CI's own Test step first (`owner_turn.rs:295`), beside the same run of main (`diag-a232-land-main-1`). On ubuntu-latest
two failures were the landing's alone:

- `crates/server/tests/shards.rs`, `a_guest_waits_past_one_window_for_a_starved_daemon_and_counts_it`: its
  `hold_starved` held the shard on the CPU a fixed fifth of the time (10 ms on, 40 ms off) for 7.5 s, so the one poll
  ran 1,013 ms of CPU, which the watchdog calls a spinning shard: "shard 2 is spinning, 1013 ms of CPU without a step
  against the 1000 ms window", exit 4, the binary with it. `hold_starved` runs `starve_this_thread` now: its share is
  derived from the span, so the whole hold's CPU is half a window, and the watchdog still sees the shard work every
  beat, which is what the test's guest waits on. The other fixed-fifth holds run 1.5 s, 300 ms of CPU, and stay.
- `crates/cli/tests/cli.rs`, `a_frozen_daemon_is_killed_at_its_heartbeat_lapse_and_restarted`: it waited for the
  anchor's withheld-beat kill line ("the daemon's heartbeat lapsed ("). A-232's anchor kills a frozen daemon by its
  process judgment instead, a window after the lapse ("has not looked for 2004 ms, its heartbeat 2004 ms old; its
  process made no progress for 1004 ms (the 1000 ms liveness window); killing it"), so the test timed out. It now
  waits for that line and holds the anchor to it: a process judgment, not a withheld beat; the beat at least a window
  old; the process still for at least the window and at most one more; the window the liveness budget.
