# Forwards to one owner went one at a time (2026-11-06, §4.8 "Lookup", §4.10)

The network load test's fault one (GAPS, "The many-client load test over the network"), owned by agents/loadtest-scale
since 2026-11-06 (the supervisor's decision).

## Description

A verb a node cannot serve locally is forwarded to its owner over this node's record session to that peer
(`FORWARD_STREAM`). The forward serialized at both ends of that session.

- **Origin.** `fleet::forward_batch_over_leader_session` borrowed the session (`take_sessions`) for the forward's
  whole round trip. Every other forward to that owner found it out and polled for it on a sleep of
  `HEARTBEAT_NS / POLL_PER_PERIOD` (10 ms).
- **Owner.** `serve_peer_records` served one request of the peer at a time (`Endpoint::serve_once_async`). A forward
  waiting on the owner shard therefore held every later request on that session, the peer's record commits and
  prepares included.

The session itself runs many exchanges at once (`Endpoint::begin`, `take_reply`, `drive` on the dialing side;
`next_request`, `reply` on the serving side). Only its use was serial.

## Measured

- **Load test** (agents/loadtest-net, Linux, loopback, f = 0, load 22 to 45): a forwarded `CreateWork` p50 1.71 s at
  300 clients and 4.30 s at 1,000, against 45 µs on the owner's own node. 536 `CreateWork` and 7 edits were refused
  `OwnerUnreachable` at 1,000, queued past the failover bound.
- **The failing test** (`crates/server/tests/fleet.rs`, `forwards_to_one_owner_never_wait_behind_each_other`). The
  setup is three nodes, each its own region, two shards each. On a there are two volumes: `slow` on a's non-control
  shard, which is held for 500 ms, and `fast` on its control shard. A read of `slow` goes through one client of b,
  and 50 ms later a read of `fast` through another.
  - On 00e5269c's forward path: `fast` took **471.8 ms**, and b counted `fleet.forward.session_out: 1`, lent to
    `forward_batch_over_leader_session`.
  - With the fix: **1.77, 2.41, 1.85 ms** in three runs, with `fleet.forward.begun` and
    `fleet.forward.taken_for_another` moving (the non-vacuity counts). Mac, load 46 to 62.

## Root cause

The borrow-for-the-round-trip discipline predates multiplexed exchanges on the session, and it was kept for forwards.

## Exact edits

- `crates/server/src/forward_lane.rs` (new), origin side. Each peer has one lane in the control shard's state:
  forwards queued, exchanges open, replies taken. Whichever waiting forwarder finds the lane undriven drives it. A
  turn borrows the session, abandons what forwarders gave up, begins every queued forward the stream credit admits,
  and takes every reply that has arrived, waking its forwarder. It drives until a packet, a timer or a newly queued
  forward, then returns the session, so record commits and discovery pages still borrow it between turns. A
  forwarder whose replies have come stops driving and wakes the next.
  - Bounds: a lane holds at most `clients_per_shard × slots × shards` forwards (refused past it,
    `fleet.forward.lane_full`). The session's stream credit bounds the open exchanges. Each forwarder gives up once
    none of its forwards has made progress for its stall budget, as before.
  - An exchange is tied to the session it was begun on. If the link replaced the session, the forward is answered
    as unanswered (`fleet.forward.session_replaced`), as one cut off by its session's loss always was.
- `crates/server/src/forward_lane.rs`, owner side (`ServedForwards`, `next_request_or_forwarded`). The serve loop
  races the peer's next request against forwards finished on their own child tasks. A forwarded verb runs as a
  child task of the loop, so one waiting on its owner shard holds nothing else. Every other request is served
  inline as before. A finished child is joined as it answers, and the peer's stream credit bounds the children.
- `crates/server/src/fleet.rs`: `forward_batch_over_leader_session` delegates to the lane, and `serve_peer_records`
  uses the race. `FORWARD_STREAM` and `LENT_TO_LINK` are `pub(crate)`.
- Sibling, found while making the session shared: `Endpoint::abandon_begun_since`. Three borrowers abandoned
  `last_exchange()` when their request failed: `slates_cluster::request_within`, the plane epoch exchange in
  `member_task.rs` and the discovery page in `fleet.rs`. That is the newest exchange on the session, which is
  theirs only if their `begin` succeeded. On a session where forwards are left open between turns, a refused
  `begin` (`Backlogged`) would have abandoned a forward's exchange instead. Each now abandons only an exchange
  begun after the one it saw first.

## Not changed, reported

- A driving turn holds the session while it waits inside `drive()` for a packet or a timer, as a forward's round
  trip did before. A record commit wanting the session meanwhile waits for the turn to end, not for every forward
  to end. Waking the driver when another borrower asks would shorten that wait.
- The batch's `streamed_bytes` and `streamed_ns` are now its own replies' (nothing in the server reads them).
- `fleet.link.stale_return` appeared in 1 of 3 runs of the new test: the link replaced the session's slot while a
  turn had it out. The one-at-a-time forward was exposed to the same.
