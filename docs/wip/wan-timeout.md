# The RTT-derived election timeout for a real WAN (§4.8 "Derived constants")

Status 2026-09-14, branch `agent/wan-timeout`. Design: §4.8 "Derived constants" — *"membership lease from
heartbeat RTT p99 × k; election timeout for the configuration group ≥ 10 × broadcast RTT p99 with the
randomization span from RTT variance; SWIM period = max(k × RTT p99, scheduler quantum)"* — and D-14 (the
hecate Raft dialect: PreVote, CheckQuorum, the regional council and the root group). Evidence tier A for the
ten: Ongaro & Ousterhout, "In Search of an Understandable Consensus Algorithm" (ATC 2014) §5.6, the timing
requirement `broadcastTime ≪ electionTimeout ≪ MTBF` with its example ranges (0.5–20 ms broadcast against
10–500 ms timeouts); for the estimator: RFC 9002 §5.3 and §6.2.1, Jacobson (SIGCOMM 1988) for the
mean-deviation tail bound, Karn & Partridge (SIGCOMM 1987) / RFC 6298 §3 for what may be sampled.

Owed since `66e9204` (2026-09-13), where the rule was **measured inert on one host**: SWIM RTT p99 17 ms and
consensus broadcast p99 33 ms both sit inside the 100 ms heartbeat, so a derived timeout could only ever be
the ten-period floor there. This record is the WAN case: what was built, what it measured, and what the KIND
lane must still confirm on a real network.

## 1. Inventory as found (2026-09-14 morning, `263ba04`)

| What | Where | As found |
|---|---|---|
| Council election timer | `crates/server/src/fleet.rs` `drive_config_council` (~3044): `idle >= ELECTION_HEARTBEATS + election_jitter(local, attempt)` | `ELECTION_HEARTBEATS = 10` periods (2620), jitter `(local + attempt) mod 10` (2627): a period-counted timeout uniform in `[10, 20)` coordinator periods of `HEARTBEAT_NS` = 100 ms (`daemon.rs:39`). CheckQuorum every 10 leader periods (3025). |
| Root group timer | `drive_root_group` (~3160, ~3179) | the same three counters, the same constant |
| The Raft core | `crates/cluster/src/raft.rs` `on_election_timeout` (371), `config_group.rs:269`, `root_group.rs:245` | sans-io; "the PreVote/CheckQuorum timer cadence is the caller's clock" |
| RTT measurements per voter session | `crates/transport/src/endpoint.rs` `smoothed_rtt()` (468), `pto()` (474) over `rtt.rs` (RFC 9002 §5.3) — **fed from `std::time::Instant`**; the SWIM probe's `rtt_ns` (`swim.rs:554`, the runtime clock) into `ProbeTiming` (`fleet.rs:441`) and Vivaldi | no p99 kept for the consensus broadcast; `PutLatency` keeps a p95 window for content puts only |
| The probe deadline | `ProbeTiming::deadline_ns` (459): `max(pto, HEARTBEAT) × 2^misses`, capped at the liveness budget | already the design's "detection timeout from RTT p99 × k" (2026-09-13) — the model followed here |
| The round budget | `consensus_budget()` (521): base one period, stall two, ten one-period extensions | fixed, written for a loopback |
| The membership lease | — | not implemented (the owner-lease safety argument is owed in §4.8's status); nothing to derive yet |
| The simulation fabric | `crates/rt/src/sim.rs` `SimFabric::send` (58) | delivered every datagram the instant it was sent |

## 2. What was built (commits on `agent/wan-timeout`)

1. `bea7edb` **rt/sim** — `SimDelay`: a per-pair one-way delay with seeded jitter, in order per flow unless
   the profile says otherwise (`sim_udp_set_delay`, `sim_udp_set_pair_delay`); datagrams wait in flight and
   `run_until_idle` hands them over at their arrival, treating the earliest arrival as a deadline the virtual
   clock may advance to. Additive: the zero path is the old code path; rt 28/28 and transport 103/103
   unchanged. By use: over 80 ms ± 20 ms every datagram flies 60–100 ms and lands in send order at 5 ms
   spacing; the reordering profile overtakes; seed 7 replays to the nanosecond, seed 8 differs.
2. `0e7a0cd` **cluster/timing** — the law (`crates/cluster/src/timing.rs`):
   - `PathRtt`: the transport's RFC 9002 estimator over every round trip this node times to one peer, with a
     sample count; `tail_ns = smoothed + max(4·rttvar, 1 ms)` (Jacobson's tail bound, the running form of
     "RTT p99 × k" `ProbeTiming` already used); `spread_ns` the variation term.
   - `ElectionTiming::derive(heartbeat, paths)`: `base = ⌈ELECTION_MARGIN × max(tail over the other
     voters, heartbeat) / heartbeat⌉` periods, `span = ⌈ELECTION_MARGIN × max(spread, heartbeat) /
     heartbeat⌉`, `ELECTION_MARGIN = 10` (Raft §5.6). The heartbeat is the smallest broadcast time the
     coordinator can observe, so a loopback derives exactly the ten periods it ran before — the floor is the
     same derivation at its floor, never a branch (R8). The span follows the path's **variation**, not the
     whole timeout, so worst-case leader-loss detection is `base + span`, not `2 × base`.
   - `ElectionTimer`: the follower/leader period counting the daemon kept as three loose counters, in one
     type; periods not wall time, so a starved follower waits longer rather than campaigning on its own
     slowness (the Lifeguard direction of 2026-09-13).
   - `round_budget(anchors, tail)`: base `max(heartbeat, tail)`, stall `max(2 periods, tail)`, one-period
     extensions up to `ELECTION_MARGIN` — the loopback's budget with no tail.
   - `request_within` returns a `TimedReply` (bytes + `Some(round trip)` only when the peer answered inside
     the deadline — Karn's rule).
3. `2b9ec09` **transport** — three defects found by the WAN proof and fixed (record:
   `docs/bugs/2026-09-14-transport-rtt-sampled-on-the-wall-clock.md`): the estimator sampled the wall clock
   while its timers ran on the runtime clock (a 160 ms modelled path measured 27 µs); `confirm_as_server`
   dropped the client's first 1-RTT datagram and waited a probe timeout for its retransmit (429 ms first
   exchange on a 160 ms path); the handshake seed was re-stamped on every retransmit (a 33 ms seed on a
   160 ms path). Two tests that had passed on those accidents now assert exact 1 ms oracles over a modelled
   half-millisecond path; the swim harness runs at the fleet's frame class.
4. `77b23c3` **cluster** — `broadcast` moved into the cluster crate (one fan-out for the daemon and the
   harness); `tests/wan_election.rs`, the WAN proof (§4 below). Record:
   `docs/bugs/2026-09-14-consensus-round-expires-inside-the-wan-rtt.md`.
5. **fleet** (this commit) — the daemon wired to the law: `ShardState::peer_paths` (one `PathRtt` per
   rostered peer, fed by the SWIM probe's acknowledgement each period on every node and by every council and
   root round's reply, timely or late; removed with a retired id), `ProbeTiming` reading the shared estimate
   (its private estimator replaced), the council and root drives on the shared `ElectionTimer` and the
   per-period `ElectionTiming::derive` over their voters' paths, the coordinator's round budget derived each
   period from the slowest measured peer path (`consensus_budget(tail)`), the content round spanned by it,
   `ELECTION_HEARTBEATS` and `election_jitter` gone, `Daemon::council_timing` / `Daemon::root_timing`
   exposed, and the boot line logging the rule with its inputs.

## 3. The formulas, with anchors

- **Broadcast RTT p99 estimate.** Per voter path, `tail = smoothed_rtt + max(4 · rttvar, 1 ms)` (RFC 9002
  §6.2.1's probe timeout over §5.3's smoothed RTT and variation; Jacobson 1988: the mean-deviation bound
  covers the tail an RTO must). Per group, the maximum over the group's other voters — a round completes
  when its slowest voter answers. Bounded: one estimator per path, O(1) state; samples counted as the
  non-vacuity witness. Karn: a timed-out exchange is no sample.
- **Election timeout.** `base_periods = ⌈10 × max(tail, heartbeat) / heartbeat⌉`; the follower campaigns at
  `base + ((id + attempt) mod span)` periods without leader contact; the leader judges its quorum every
  `base` periods. `span_periods = ⌈10 × max(spread, heartbeat) / heartbeat⌉`, `spread = max(4 · rttvar,
  1 ms)` — the design's "randomization span from RTT variance", in the tail's own units.
- **Round budget.** base `max(heartbeat, tail)`, stall `max(2 × heartbeat, tail)`, extension `heartbeat`
  up to 10 grants, poll `heartbeat / 10` (the daemon's anchors `HEARTBEAT_NS`, `SUSPICION_PERIODS`,
  `POLL_PER_PERIOD`, the 3/4 lookahead).
- **The probe deadline** (unchanged law, shared estimate): `max(tail or the initial PTO, heartbeat) ×
  2^misses`, capped at the liveness budget. **The SWIM period** the design names, `max(k × RTT p99,
  quantum)`, holds by construction: the probe task awaits each probe's outcome before sleeping the quantum,
  so the cadence is at least the round trip plus a beat — nothing duplicated.
- **The membership lease**, `k × heartbeat RTT p99`: no lease mechanism exists in the tree (§4.8's status
  keeps the owner-lease safety argument owed), so there is nothing to derive yet; a constant with no consumer
  would be a magic number in waiting. It is owed with the lease.

## 4. The proof by use on the fabric (`crates/cluster/tests/wan_election.rs`)

Three voters at f = 1 over the fabric's latency model, each node running what the daemon's control shard
runs (a probe loop per peer feeding the path estimate; a coordinator of `drive_config_council`'s shape over
the shared `ElectionTimer` and `broadcast`, late replies folded at the settle, late pre-vote replies
dropped). The **rule** is the experiment's input: `FIXED` — the daemon before today, floor timing and a
one-period budget; `DERIVED` — the law and the derived budget; `FIXED_TIMING_DERIVED_BUDGET` isolates the
timeout rule. Virtual clock, seed 11, 2026-09-14 09:31–09:32; the box's load (7.8–11, later 67) does not
bear on a virtual-clock run. Commands: `cargo test -p slates-cluster --test wan_election -- --nocapture`
(0.11–0.94 s wall each).

| Profile | Rule | Outcome |
|---|---|---|
| LAN, zero delay, 300 periods | FIXED and DERIVED | **identical histories** to the nanosecond: one campaign at 1.0 s, leader HostId(1) at 1.12 s, held; derived base/span 10/10, tails 1.0 ms (the granularity floor), 1,128/598/598 samples. |
| Inter-region, 80 ± 20 ms one way (120–200 ms round trip), 300 periods | FIXED | **no leader in 30 s**; 56 campaigns (18/19/19), every pre-vote round expired at 75 ms with its replies in flight. |
| — | DERIVED | one campaign per node at 3.17–3.32 s; leader HostId(1) at **3.62 s**, held to the end, 0 campaigns after; base 22/25/22 periods on tails 212.8/242.7/215.0 ms, spreads 48–77 ms (span 10); samples 415/224/222. |
| — leader killed at 15.21 s | DERIVED | successor HostId(2) at 21.58 s — **6.37 s** after the death, inside twice base + span (6.8 s), after one split attempt (17.50/17.74 s) resolved by the jitter rotation (20.15/20.82 s). |
| GEO class, 500 ± 100 ms one way, 1,800 periods | FIXED_TIMING_DERIVED_BUDGET | leader HostId(2) at 15.96 s **held for 180 s** — PreVote refuses a lone timed-out follower — but **59 campaigns against the live leader** (63 in all), one every ~2.7 s. |
| — | DERIVED | 6 campaigns, all before the first leader (5.1–5.3 s split, 18.9–19.7 s won), leader HostId(1) at 21.3 s, **0 campaigns after**; base 131/133/133 periods on tails 1,306–1,328 ms, spans 28–33 on spreads 279–328 ms. |

Two findings the measurement corrected in my own predictions:

1. The WAN-blocking defect was the **round budget**, not the timeout: with a one-period base a council whose
   voters sit more than ~37 ms one way apart (75 ms round trip, the lookahead point) can never elect,
   because every pre-vote round expires before its first reply and a late pre-vote reply is dropped by
   design. The election timeout rule, on its own, would not have unblocked it.
2. The fixed ten-period timeout does **not** depose a live leader at any profile tried, even where the
   leader's heartbeat gap exceeds a follower's timeout: Raft's PreVote refuses the lone timed-out follower
   at the follower that still hears the leader, and the jitter rotation lengthens the campaigner's next
   timeout past the gap. Its cost is spurious campaigns — a pre-vote round each — against a live leader
   (59 in 180 s at the GEO profile), which the derived timing removes entirely. "Flap" as leader change did
   not occur; "elections per N periods" is the honest measure, and it is what the test asserts.

The inter-region profile: 80 ms one way is the far side of a real inter-region pair — from memory, flagged
for verification: AWS us-east-1 ↔ ap-southeast-1 is measured around 220–240 ms round trip and
us-east-1 ↔ ap-northeast-1 around 150–170 ms by the public inter-region latency grids (cloudping.co;
Azure's published round-trip table), i.e. 75–120 ms one way; the ± 20 ms jitter is a quarter of the delay.
The GEO class: one geostationary hop is ~239 ms one way by geometry (2 × 35,786 km at c), so 500 ms one way
is two hops or one hop with terrestrial backhaul — the profile at which the gap exceeds a one-second
timeout, chosen to expose the timeout rule's cost, not as a deployment target.

## 5. The daemon, by use (real loopback)

`cargo test -p slates-server --test fleet -- --exact a_loopback_fleet_derives_its_election_timing_at_the_measured_floor`:
three daemons over real loopback UDP with mutual TLS elect a leader and every node's
`Daemon::council_timing()` reports base 10, span 10, a tail inside one heartbeat, and samples > 0 — the
floor, measured not defaulted. Passed in 6.92 s at box load 67.5 (2026-09-14 09:53).
`three_daemons_elect_one_stable_council_leader_over_the_transport` passes over the wired daemon in 8.81 s
at load 69 (09:54). `cargo test -p slates-server --lib` 39/39.

## 6. What the KIND lane (and a real WAN) must confirm

Proven on this box: the law, the fabric's model of it, the daemon's wiring at the LAN floor, and the
transport over a modelled far path. Not provable here: a real inter-region path's RTT distribution (the
model's jitter is uniform and symmetric; real paths have queueing tails and asymmetric routes — the
estimator is Jacobson's, built for exactly that, but its p99 coverage on a real distribution is an
empirical claim), real loss (the fabric models delay and reordering, not loss; the transport's probe timeout
recovers loss but its interaction with the derived round budget under loss is unmeasured), and the
handshake's retransmit ceilings on a path above ~330 ms one way (the initial-PTO-capped backoff is
RTT-independent; a double-hop GEO path at 1.2 s round trip established in the sim but the confirmation
phase's budgets were not stressed with loss). The KIND lane should run the three-process fleet across two
regions (or `tc netem delay 80ms 20ms` between the pods) and read `slates status` / the accessors for
`base_periods ≈ ⌈10 × tail / 100 ms⌉` and a single stable leader over an hour; then the same with 1 %
loss.

## 7. Siblings found, not fixed here

- The record plane's commits and takeovers now run under the derived per-period budget, but their timely
  acknowledgements (`collect_acks` in the cluster crate) do not sample the path — only the council and root
  rounds and the probes do. Feeding them is additive (`Committed` would carry per-holder round trips).
- The learner fetches (`ConfigFetch`/`RootFetch`) do not sample the path either (rare; small).
- `content_budget` spans its round by the derived budget, but its hedge delay is the put-latency p95 — a
  content put to far holders has the transfer time in its samples, correctly kept apart from the path
  estimate.
- The coordinator serializes its rounds within a period (council, root, then each shard's ships); on a far
  path a period grows by one round trip per sequential round. Not a defect of the timing law (the timer
  counts periods), but a latency the KIND lane will see on `fleet_progress`.
