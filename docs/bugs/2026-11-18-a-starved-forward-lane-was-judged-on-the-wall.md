# A starved forward lane was judged on the wall: its tests' bounds, its recovery count and its failover bound (2026-11-18, §4.8 "Lookup", A-228, A-315, A-352)

Goal g255 of the supervisor's stack ("every cross-shard and remote wait is derived"), from the integrator's owed runs of
goal g245's two dead-session tests under starvation, relayed 2026-11-18 10:45 UTC with the question: does the forward
lane judge "hears nothing" in its own time, so the tests must assert in that time, or on the wall, so a starved lane
gives up a live session?

## Description

`a_forward_lane_leaves_a_session_that_hears_nothing_for_a_new_one` and
`a_read_through_a_dead_session_keeps_its_place_behind_a_burst` (`crates/server/tests/fleet.rs`) make b's record session
to a deaf and hold that b judges it dead where a borrower returns it, redials, and answers on the replacement: the first
with four readers keeping b's lane borrowing the session, the second with a probe read sent before a 256-client burst.
At nice 19 beside two spinners a CPU both failed every run. Three things were judged on the wall:

- **The tests' own waits and bounds.** The recovery bound was 1.1 s of wall time after the drop (a liveness budget and a
  heartbeat); the harness waited 8 s of wall time for the readers' first reads, for the injections, and for the burst's
  first round (256 connects); the probe and each reader gave up their call after 11 s of wall time; and a client's
  connect gave up at 5 s of wall time (`CREDIT_WAIT`).
- **The lane's own recovery count.** `fleet.forward.replaced_answered_in_budget` compared the gap from the drop to the
  first answer on the replacement with a liveness budget of wall time.
- **A forward's failover bound.** A forward was refused `OwnerUnreachable` at the failover SLO from its first send
  (`verbs::resolve_and_forward`, and A-315's cap in `forward_lane::Stall`), however long the path it waited on needed to
  judge a silent session dead and replace it. The transport's silence window is the longer of a liveness budget and the
  session's persistent-congestion duration, three probe timeouts (RFC 9002 §7.6.1), which grows with the round trips a
  starved process measures: at about 3 s a probe timeout the window alone is about 9 s, so a forward through a session
  gone silent was refused at the 10 s SLO before the session could be judged dead.

## Measured

The integrator's diagnostics on GitHub (4 CPUs a runner, 10 runs a test, 2026-11-18):

| Run | Load | deaf-session test | burst test |
| --- | --- | --- | --- |
| `diag-integrator-lent2-1`, run 44286748160, main `4855ee322` | 1 spinner a CPU, tests at nice 0 | 10 of 10 on x86_64 and arm64 (recovery 46 to 125 ms) | 10 of 10 on each |
| `diag-integrator-lent-1`, run 44274886122, main `1e8f4a49e` | 2 spinners a CPU, tests at nice 19 | 0 of 10 on each (recovery 2.31 to 13.67 s on x86_64, 2.57 to 7.20 s on arm64) | 0 of 10 on each |

The starved runs' counters answer the question. In all 20 deaf-session runs `fleet.link.session_judged_dead` was 1, the
injected session (its replacement was never dropped), and `fleet.forward.given_up_silent` and `given_up_at_bound` were
0: A-315 gives a batch up only after a turn of its lane begun after the silence has read the session's socket empty,
and no such turn gave up a live owner's forward. Of the 20 burst runs, 19 failed on the 8 s wait for the burst's first
round (its 256 connects at about one percent of a CPU) and 1 on a connect's claim, which left it no probe; the 19
probes were refused `OwnerUnreachable` at 10.60 to 10.92 s (3), answered at 10.60 s (1), or cut off by the test's own
11 s deadline (15), the deaf session judged dead in only 8 of the 19 by then.

So the lane judges a silence in its own time; what failed was everything judged on the wall: the tests' bounds, and a
forward's failover bound that fell below the floor of the path it waited on.

## Fix

- **The tests (A-228's rule).** Every wait of both tests is charged in the daemons' own periods (`poll_for_periods`),
  the injections and the readers' first reads within the forwards' failover bound in b's periods
  (`dead_session_watch_periods`), the burst's first round within `poll_until`'s convergence budget. The readers', the
  probe's and the burst's calls and connects wait while b's control shard runs its periods
  (`Client::try_call_for_periods`, `Client::connect_for_periods`, read off the runtime's registry), and a burst round
  sends only into the room each ring has (`Client::send_burst_while_room`).
- **The recovery, counted in periods (A-352).** A drop is recorded on the wall and in the control shard's coordinator
  periods (`state::WallAndPeriods`, `ShardState::dead_session_dropped_at`), and the lane's first answer on the
  replacement is counted in budget when it came within `REPLACEMENT_ANSWER_PERIODS` (the link's redial pace and a
  forward's stall budget, a liveness budget, in periods). The deaf-session test holds its bound in the same periods
  (`DEAD_SESSION_RECOVERY_PERIODS`).
- **A forward is never refused below its path's floor (A-352).** Each driving turn measures the floor of the path to its
  peer on the session it borrowed (`forward_lane::path_floor_ns`): the transport's silence window, a lane turn's hold
  before the return that judges it (a stall budget), the link's redial pace, and four probe timeouts (the stall budget's
  tail, the replacement's handshake, its first discovery page and the forward's exchange). A forward's failover bound
  (`forward_lane::FailoverBound`, kept across its resends with its `Place`) is the SLO from its first send, or the
  longest floor its path has measured where that is longer; `Stall` and `resolve_and_forward` both judge against it. A
  lane whose floor passes the SLO says so in the log once a crossing (`fleet.forward.floor_past_slo`), and each forward
  the floor holds past the SLO is counted (`fleet.forward.held_past_slo`), as A-129 runs the fleet's timers at their
  measured floors and reports the overrun.

## Tests

- `a_forward_is_never_refused_below_its_paths_floor` (new): three nodes whose failover SLO is one liveness budget, below
  the transport's silence window on any path (floored at three initial probe timeouts, about 2 s); b's discovery to a
  held; b's session to a made deaf; a probe read through b. On adas-mac-studio.local-2 (32 CPUs, load 13 to 20,
  2026-11-18): answered `Status` 2.04 s and 21 of b's periods after its send. Red with the floor removed from the bound
  (`FailoverBound::at_ns` returning the SLO alone): refused `OwnerUnreachable` at 1.004 s, 10 periods.
- `a_forwards_bound_is_its_slo_until_its_path_measures_a_longer_floor_and_never_falls_back` and
  `a_silence_is_held_past_its_slo_to_its_paths_floor` (unit, `forward_lane.rs`): the bound's rule apart from any
  session; the module's 12 tests pass.
- The two dead-session tests, in the daemons' own periods: the deaf session's replacement answered 21 ms and 101 ms,
  1 period, after its drop; the probe behind the burst answered 2.59 s and 2.73 s, 23 periods, after its send; and the
  lane's other tests (`a_held_origin_gives_up_no_forward_whose_reply_came_while_it_was_held`,
  `an_unanswered_forward_is_sent_again_within_the_takeover_bound`) unchanged, all on the same machine.

## Owed

- The integrator's starved lane over both tests again (`diag-integrator-lent`'s script, two spinners a CPU at nice 19),
  which this machine cannot run without starving every other agent's work on it. The deaf-session recovery is expected
  within 11 of b's periods there: the starved runs' recoveries were about 3 to 15 of b's periods by the periods b ran
  in each run's watch, so that run is the evidence the bound needs, and its failures, if any, are the lane's to answer.
- Siblings found on the way, each a wait on a live daemon's answer judged on the wall: the rendezvous claim wait
  (`slates_ipc::rendezvous::CLAIM_WAIT_NS`, one second, which a starved burst's connect met once) and the fleet tests'
  `Client::connect` and `send_burst` (`CREDIT_WAIT`, used by every fleet test). Recorded in GAPS under the R3 debt of
  `LIVENESS_BUDGET_NS`.
