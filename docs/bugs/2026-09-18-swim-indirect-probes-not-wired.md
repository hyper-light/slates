# The live SWIM path omitted the indirect-probe stage (AUD-15)

Date: 2026-09-18. Contracts: §4.8 "Membership" (direct probe → k indirect proxies → SUSPECT → DEAD),
AUD-15 in `docs/bugs/2026-09-14_AUDIT.md`, GAP-A9-7. Design evidence: SWIM §4.1 (Das, Gupta, Motivala,
DSN 2002: `k` ping-requests after an unanswered direct ping), Lifeguard (Dadgar, Phillips, Currey, DSN
2018), Vivaldi relay selection (§4.8 "a per-peer RTT prediction that selects the indirect-probe relays
nearest the target").

## Symptom

The pure detector implemented the indirect probe (`Detector::request_indirect`, `on_ping_req`,
`on_indirect_ack`, a `PingReq` wire variant, and unit tests), but the daemon never called it: its
probe task (`probe_and_apply`) sent a direct probe and folded the acknowledgement or the timeout, so a
node that could not reach a peer directly — while another node could — suspected and retired that peer
without trying a relay. Source-confirmed by the September 14 audit; no live path exercised
`PingReq`, and the daemon's serve side refused one (it carries no boot_nonce, so the identity gate
returned no reply).

## Root cause

Not a bug in a stage that existed: the stage was never composed into the daemon. Composing it is not a
method call, because the daemon's shape differs from the pure detector's: one detector runs **per peer**
(scoped to that peer, so its own membership view holds no relay candidates) and each probe task **owns
the session** to its peer, so a requester cannot send a ping-request over a relay's session itself, and
a relay's serve side cannot probe the target inline (the target's session belongs to another task).

## Fix

The stage is a hand-off between probe tasks over bounded queues on the control shard
(`ShardState::indirect`, `crate::fleet::IndirectProbes`), keyed only by authenticated members this node
keeps direct contact with:

- **Requester.** A direct probe that times out begins the stage (`begin_indirect_stage`): up to `k`
  relays are chosen from the alive peers this node holds a formed probe session to, ranked nearest the
  target in Vivaldi coordinate space (`indirect_relays`, the same ordering the pure detector uses, over
  the shared view; each peer's coordinate is recorded from its acknowledgements), and a ping-request is
  posted under each relay. The relay's probe task is woken and sends it on its session before its own
  probe (`carry_indirect_traffic`, `deliver_once` — the same race and abandonment as a direct probe).
- **Relay.** The serve side accepts a ping-request only from the member its session's anchor currently
  announces and only for a target the relay keeps direct contact with (`receive_ping_request`; anything
  else is refused and counted), posts the ask under the target and wakes the target's probe task, which
  probes it at once. The target's acknowledgement answers every ask for it (`answer_relay_asks`), posting
  the result under each requester and waking the requester's probe task, which carries it back as a new
  wire message, `IndirectAck` (tag 4: relay, target, the requester's probe nonce echoed, the relay's
  boot_nonce, gossip), on the relay's own probe session to the requester.
- **Requester again.** The serve side validates the relay's identity as it validates any acknowledgement
  (`learn_member`), records the answer under the target (`receive_indirect_ack`) and wakes the target's
  probe task, which credits it **before the tick** that would resolve the unanswered probe
  (`credit_relayed_answer` → `Detector::on_indirect_ack`), so the target is not suspected on a lost
  direct packet. An answer about a probe older than the suspicion window (`INDIRECT_ACK_LAG_PROBES`)
  is dropped.
- **Latency.** Probe tasks park between probes on `sleep_or_wake`, cut short when traffic is posted for
  them (`wake_probe_task`, the record-link waiter pattern), so a relay probes its target and a requester
  credits the answer within a round trip of the miss, not a period later — what keeps the stage inside
  the suspicion window.
- **Derived.** `k` = the bit-length of `n+1` for the neighbourhood (`indirect_fanout`), the same size
  term the gossip budget uses, capped by the relays that exist; the confirmation-curve `K` now equals it.
  Every exchange runs under the probe's own deadline law. Nothing is a hidden constant.
- **Bounds.** Each queue is keyed by member id and holds one entry per (peer, other peer) pair — a newer
  request replaces the older — so the whole is bounded by the neighbourhood squared; a request naming an
  unknown member is refused and counted (`fleet.probe.indirect.refused`); a retired peer's queues are
  dropped with its session.

## Failing test first, and regression

`crates/server/tests/fleet.rs::an_indirect_probe_through_a_relay_keeps_a_peer_the_direct_path_lost_and_losing_both_paths_retires_it`
(AC §4.8): three nodes form; B is made deaf to A's direct probes only (`Daemon::inject_probe_deafness`,
at B's serve side — the transport is untouched); the test requires A to have credited a relayed answer
(`fleet.probe.indirect.acked ≥ 1`) and C to have relayed one (`fleet.probe.indirect.relayed ≥ 1`), then
holds for 100 of A's coordinator periods — past the six backed-off misses (≈ 4 s) a silent peer takes to
be declared dead — requiring B to stay a member of A's fleet; then B is made deaf to C too and both
survivors must retire it (the stage does not weaken eventual detection).

- **Negative control (this box, 2026-09-18):** with relay selection disabled (`relays.truncate(0)`),
  the test fails at its first assertion — `acked=0, requested=0, relayed=0` — after the period-charged
  poll budget (446.46 s). A direct-only implementation cannot pass it.
- **With the fix:** passes in 14.22 s (formation, relay stage, 100-period hold, double-loss retirement).
- Wire: `every_message_round_trips` (both new shapes), `indirect_ack_has_a_golden_encoding`,
  `a_truncated_indirect_message_is_refused` (every header cut of a `PingReq` and an `IndirectAck` is
  `Truncated`, never read past the bytes that arrived).

## Validation (this box, 18 cores, 2026-09-18)

`cargo test -p slates-cluster --lib swim`: 9 passed. `cargo clippy -p slates-server -p slates-cluster
--all-targets -- -D warnings`: clean (the probe loop's complexity was split into `probe_cycle`,
`credit_relayed_answer`, `begin_indirect_stage`). fmt clean. The in-process fleet suite, run alone:
**45 passed, 0 failed, 295.67 s** (the 44 existing histories unchanged by the wake and the queues, plus
this one). `cargo xtask check` ok.

## Siblings

`Detector::request_indirect` remains the pure form (used by the detector's own unit test); the daemon's
`indirect_relays` mirrors its ordering over the shared view because a per-peer detector holds no relay
candidates — the reason the two are not one function is recorded on both. The root group and the
council do not probe; their liveness comes from this membership view, so they gain the stage for free.
No other caller of the probe path exists.
