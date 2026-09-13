# SWIM retires a live peer that is merely starved of CPU: a fixed probe deadline, a suspect dropped from the probe rotation, an exhausted suspicion gossip, and a stale reply counted as a miss

Date: 2026-09-13
Area: `crates/cluster/src/detector.rs` (the SWIM protocol-period machine), `crates/cluster/src/swim.rs`
(the live probe), `crates/server/src/fleet.rs` (the daemon's per-peer probe task), with a test-facing
injection in `crates/server/src/daemon.rs`.
Severity: fleet correctness under load — a **live** consensus voter is declared dead, the root leader
retires its region, the voter refutes and is re-admitted, and the fleet's configuration oscillates
(`version 0→1→2→3→4`, `regions 3→2→3→2→3`) at operator-visible cadence with nothing killed. Recorded as
owed in `docs/bugs/2026-09-13-consensus-voters-outside-record-neighbourhood.md` ("Finding under extreme
external load", measured on a box spike to 1-minute load 41 on 18 cores).

## Description

The design derives the membership detection timeout from the measured round trip ("detection timeout for
membership from RTT p99 × k; SWIM period = max(k × RTT p99, scheduler quantum)", §4.8 "Derived
constants"). The daemon's probe instead waited a fixed one heartbeat (100 ms) for every acknowledgement,
so a peer whose control shard was descheduled on an oversubscribed box answered *late, not never* — and
six late answers in a row were six misses. Reproduced deterministically by use: hold B's control shard
busy for three liveness budgets (3 s; `Daemon::starve_control_shard`) and watch A's membership. Before the
fix A retired B every time (`a_starved_but_live_peer_is_not_retired` failed 4/4 at 6.8–7.4 s, load 6–16,
2026-09-13 14:21–14:30); after it A keeps B (3/3 at 8.8–9.1 s, load 3–9).

## Root cause — four defects, each measured

Instrumenting the probe task (each probe's deadline, outcome, consecutive-miss count and the detector's
belief) and the fold into the shared membership showed, with the first fix alone in place:

```
MISS waited 100 ms  deadline 100 ms  misses 0  state Alive
MISS waited 200 ms  deadline 200 ms  misses 1  state Suspect
MISS waited 400 ms  deadline 400 ms  misses 2  state Suspect
MISS waited 800 ms  deadline 800 ms  misses 3  state Suspect
MISS waited 1000 ms deadline 1000 ms misses 4  state Dead      ← periods 4 ≥ window 4
FOLD  before Alive@0  after Dead@0
```

1. **A fixed probe deadline.** `probe_budget()` was `CommitBudget::hard(HEARTBEAT_NS, …)`: 100 ms for every
   probe, whatever the measured round trip and however many probes in a row had just missed. The old
   death span for a silent peer was therefore `6 × (100 ms deadline + 100 ms sleep) ≈ 1.2 s` — and a live
   peer starved for 1.2 s (routine at load 41 on 18 cores) died.
2. **A suspected member dropped out of the probe rotation.** `Detector::next_target` drew only from
   `Membership::alive()`, which excludes `Suspect`. The tick that suspected B therefore returned no ping
   and left `probing = None`; on every later tick nothing was resolved, so (a) the local-health multiplier
   froze where the first miss left it — the window stayed `2 × 2 = 4` periods instead of dilating to the
   cap's `2 × 3 = 6` — and (b) a direct acknowledgement from the suspected peer could never register
   (`on_ack` credits only the member being probed). The trace above is exactly this: dead at periods 4.
   SWIM §4.2 and memberlist's `probe` keep probing a suspect (only dead and left members are skipped).
3. **The suspicion's gossip is exhausted before a stalled peer hears it.** A suspicion is disseminated
   `λ·ln(n+1)` times (four pings at two nodes) and a direct acknowledgement does not clear a suspicion
   (only the member's own refutation at a higher incarnation does, SWIM §4.2). A peer back from a stall
   longer than those four pings was never told it was suspected, never refuted, and died while answering
   every probe. Lifeguard's **buddy system** (memberlist `probeNode`: a ping to a suspected node always
   carries the suspect message) is the named remedy.
4. **A stale acknowledgement ended the probe as a miss.** `probe_once` returned `TimedOut` the instant a
   reply carried another probe's nonce. After a stall the peer answers the *first* buffered ping (stale by
   then) and — because the session serves one exchange at a time — drops the later pings that arrived
   while that reply was unacknowledged (`serve_once` phase two; a new offset-0 frame on the completed
   stream is a duplicate). So the fresh probe was neither answered nor re-sent: the stale reply was the
   sixth miss.

## Impact

- A live, CPU-starved node is retired and its objects taken over (a full recovery driven for nothing),
  then re-admitted when it refutes — repeatedly under sustained load. Operator-visible configuration
  churn; at three nodes with `f = 1` a spurious retirement also puts a region at its minimum.
- The direct probing of consensus voters (`60265e3`) exercised this on more pairs, which is how the flap
  was caught.

## Exact edits

- `crates/server/src/fleet.rs`: `probe_budget()` replaced by `ProbeTiming { rtt: RttEstimator,
  consecutive_misses }` — the deadline is `max(pto, HEARTBEAT_NS) × 2^misses` capped at
  `LIVENESS_BUDGET_NS`, where `pto` is the transport's RFC 9002 §6.2.1 probe timeout
  (`smoothed_rtt + max(4·rttvar, granularity)`, Jacobson's mean-deviation tail bound — the running form
  of "RTT p99 × k") over the peer's measured probe round trips (Karn: a miss yields no sample); floored at
  the heartbeat as the scheduler quantum, doubled per consecutive miss (RFC 9002 §6.2.4), capped at the
  anchor's liveness budget (the tree's own definition of a live daemon). `probe_and_apply` feeds it
  (`acknowledged(rtt_ns)` / `missed()`) and builds the ping's gossip with `Detector::ping_gossip`.
  Unit test `the_probe_deadline_is_derived_from_the_round_trip_backed_off_and_capped`.
- `crates/cluster/src/detector.rs`: `next_target` draws from alive **and suspected** members
  (`is_probed`); `ping_gossip(target, max)` — the buddy system — injects the current suspicion of the
  ping's target into the batch even after its transmits are spent. Tests
  `a_suspected_member_is_still_probed_and_its_acknowledgement_counts`, `a_dead_member_is_not_probed`,
  `a_ping_to_a_suspected_member_always_carries_the_suspicion`.
- `crates/cluster/src/swim.rs`: `probe_once` drives `exchange_until_echoed`, which discards a stale
  acknowledgement and **re-sends the same probe** (same nonce) until the echo arrives, the whole exchange
  still raced against the caller's deadline. Live test (simulated UDP, real sessions)
  `a_stale_acknowledgement_is_discarded_and_the_resent_probe_is_acknowledged` (`TargetMode::StaleThenServes`):
  failed before (`timed_out`), passes after; the existing stale-nonce rejection test still passes (a stale
  reply with no follow-up still times out and suspects).
- `crates/server/src/daemon.rs`: `Daemon::starve_control_shard(span_ns) -> Option<Receiver<u64>>`, the
  test injection — a task spun on the control shard's clock, reporting its measured span.
- `crates/server/tests/fleet.rs`: `a_starved_but_live_peer_is_not_retired` (B starved for
  `STARVATION_NS = 3 × LIVENESS_BUDGET_NS`; A must keep B for the span plus a settle; the hold's measured
  span asserted; the mesh whole after).

## Validation (2026-09-13, 18-core box shared with other work; load averages as noted)

- By use, before: `a_starved_but_live_peer_is_not_retired` FAILED at the "kept" assertion 4/4
  (7.37 s, 6.94 s, 6.79 s, 6.83 s; load 6–16). After: 3/3 passed (8.83 s, 9.07 s, 8.87 s; load 3–9), the
  trace showing five misses at 100/200/400/800/1000 ms deadlines and the sixth probe acknowledged
  (85 ms round trip including the stale-reply re-send) carrying B's refutation `Alive@1`; the only folds
  into the shared membership were `Alive@0 → Alive@1`.
- `cargo test -p slates-cluster`: 115 unit + all live suites green (swim live 4/4; detector 17).
- `cargo test -p slates-server --lib`: 20/20. Full serialized in-process fleet suite: 26/26 in 135.9 s at
  rest (load 2.8–3.0) before the predicate correction, and again after it (the commit message carries
  that run's time).
- Under held total load ≈ 17–18 (14 busy-spin processes over a baseline of 3, 14:37–14:38):
  `a_starved_but_live_peer_is_not_retired` 3/3 (9.18 s, 9.07 s, 9.05 s) and the previously flapping
  `a_root_learner_fetches_the_committed_region_membership_over_the_transport` 3/3 (5.90 s, 5.86 s, 6.62 s).
- The gated three-process deployment test (`SLATES_TEST_CLI=1`, a real `SIGKILL` of the owner, the
  successor mounted through the kernel): failed at `assert_retired` after the 40 s fleet wait before the
  predicate correction (42.12 s), passes after it in 8.17 s (14:43, load 4.7) — the survivors retire the
  dead owner, count one peer probed, and serve its volume.
- Gates: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask check`
  all clean.
- Cost accepted: a truly dead peer is now declared after six backed-off misses, ≈ 4.1 s at rest
  (100+200+400+800+1000+1000 ms of deadlines plus the sleeps) instead of 1.2 s — the Lifeguard trade, far
  below the suites' 30 s retirement deadlines and the CLI deployment test's 40 s fleet wait; under load
  the base deadline rises with the measured round trip, so the tolerance grows exactly where it is needed.

## Correction to `60265e3`, found by running the gated three-process deployment test

`keeps_direct_contact_with` — the dial/probe/retire predicate `60265e3` introduced — claimed that "a dead
voter's link and probe idle the moment its group commits its retirement". Wrong: the voter sets are Raft's
`all_voters`, which do not shrink when the configuration retires a member (no Raft membership change is
wired), so a **dead voter stayed probed and linked for good**: its probe session still counted as formed
(`slates status` `fleet_peers_probed` stayed at 2 on both survivors after the owner's `SIGKILL`;
`SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death`
failed at `assert_retired` after the 40 s fleet wait, 42.12 s, 2026-09-13 14:40, load 4–8) and its record
link re-dialed into the void a handshake budget (~20 s) at a time. `60265e3`'s validation ran the in-process
suite, whose retirement tests assert membership, not the probed-peer count; the gated process test was not
run. Fix (`crates/server/src/fleet.rs`): the predicate excludes a peer this node's membership holds **dead**,
whatever its role; a retired peer that comes back is re-admitted alive by its own probes
(`serve_peer_probes`) and regains contact then. Validation: see below.

## Siblings swept

- `Detector::health_multiplier` documents that "the caller multiplies its probe-period timer by this" (the
  Lifeguard probe-cadence dilation); `probe_peer` sleeps a flat `HEARTBEAT_NS`. Documented, unwired —
  reported, not changed here (it would compound the death span by up to 3× the sleep and has no failing
  test yet).
- The transport serves one exchange per session and drops a request that arrives while the previous
  reply is unacknowledged (a new offset-0 frame on a completed stream is a duplicate). The SWIM re-send
  compensates at its layer; fresh stream ids per exchange (RFC 9000 §2.1) remain the transport's owed fix.
  The record plane's retry after a stall is next period's idempotent re-ship, so it is not exposed the
  same way; whether a stale record acknowledgement can be folded as the current dispatch's was not
  examined here.
- `DetectorTiming::suspicion_periods` stays two *probes*; with the deadline adaptive the window is now a
  span of backed-off waits, which is the composition the design's "derived from RTT p99 × k" intends.
