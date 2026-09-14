# The handshake retry forgets its flight, so a peer starved past one budget never meshes

Date: 2026-09-14
Area: `crates/transport/src/endpoint.rs` (`Endpoint::establish`), `crates/server/src/fleet.rs` (`establish_session`)
Severity: liveness — an N-node fleet's probe mesh can stall forever under CPU load; a takeover that
needs that mesh never happens.

## Symptom

During the §4.2 admission merge validation (2026-09-14 00:04, three agent `cargo` builds running
alongside, load 6–7, fresh `target/`), the fleet suite failed once at
`a_takeover_successor_serves_the_dead_owners_content_over_nfs`:

```
node a did not form its full probe mesh within the formation deadline
(it sees members Some([HostId(2923…), HostId(11465…), HostId(15427…)]))
```

`fleet_members` (the seeded alive set) showed all three nodes, but node a's probe mesh never formed.
The test ended at the 4000-period daemon-time budget (~356 s), not the 300 s frozen cap — so the
coordinators were *ticking*, the mesh just never came up. Replayed alone the test passed 3/3 at
9.5–10.5 s, and the full suite re-ran 32/32; it was recorded as an open observation to trace on
recurrence rather than dismissed.

## Root cause

`Endpoint::establish` held the handshake flight it last sent in a **local** variable
(`let mut last_flight`). The dominant handshake loss is a peer not yet listening when first dialed
(a fleet forms as its nodes boot one after another); `establish` recovers it by retransmitting the
pending flight each probe timeout, up to a 32-retransmit budget, then returns `NotReady`
(banned item 8: no unbounded wait).

The fleet's `establish_session` keeps the un-established endpoint on a `NotReady` and calls
`establish` again next period, on the same socket — the discipline that lets a dialer's pinned
`accept` complete rather than a fresh-port re-dial being ignored. But because `last_flight` was a
local, the **second** call began with nothing to send: on each timeout it hit
`if !last_flight.is_empty()` and sent no datagram, waited the whole budget in silence, and returned
`NotReady` again. The socket could never establish. A peer merely starved of the scheduler past the
first budget — the exact effect of heavy CPU load — was therefore unreachable on that socket
forever, and the `N·(N−1)` mesh stalled.

This is a robustness-under-load defect (the mandate: "We MUST BE ROBUST to noisy and heavy CPU
load"). It hid because on a quiet machine the peer always answers within the first budget, so the
second call is never needed.

## Reproduction (failing test first)

`crates/transport/tests/session.rs`,
`a_dialer_that_outwaited_an_absent_peer_completes_the_handshake_once_the_peer_listens`: a dialer
`establish`es against a peer that is bound but not listening (it discards every flight queued during
the dialer's first budget — the fabric never drops, so the receiver plays the loss), spends the
budget (asserts typed `NotReady`), then the peer begins its handshake and the dialer `establish`es
again on the same socket.

- Unfixed (`git show HEAD:…endpoint.rs`): FAILED — `the second establish on the same socket did not
  complete (peer discarded 33 flights): NotReady`.
- Fixed: ok, 0.24 s. The discard count (≥ 1, measured 33) is the non-vacuity witness that the first
  budget's flights really reached the peer and were thrown away.

## Fix

The flight this end last sent moves onto the `Endpoint` as `pending_flight`, taken at the start of
`establish` and left back on it when the call ends `NotReady`, so the next call retransmits it.
`handshake_budgets_spent` counts the `NotReady` calls (reset to zero on establishment).
`establish_session` retries on the same socket while `handshake_budgets_spent() <
ESTABLISH_BUDGETS_BEFORE_REDIAL` (= 2: one budget for a peer not yet listening, one more resending
the flight for a peer merely starved), and on any further budget or any protocol fault drops the
endpoint so the next period dials afresh from a new port (the demultiplexer opens a fresh session,
`a_peer_that_redials_replaces_its_old_session`).

`assert_fleet_forms` now reports, on a formation failure, each node's `fleet_meshed`, seeded
members, `fleet_formed_probe_peers` and coordinator `fleet_progress` — so a future failure says
whether the coordinators were ticking (a genuine non-convergence: the period budget) or frozen (a
wedge: no progress), and which probe sessions never formed. The bare "sees members" message could
distinguish neither.

## Sibling sweep

- `Endpoint::confirm_handshake`'s client half already keeps its `final_flight` on the endpoint and
  resends it across the confirmation wait — correct, and the model this fix follows for the main
  handshake flight.
- The record/ship plane's `establish_session` is the same function as the probe plane's, so both
  planes inherit the fix; no second copy to change.
- No other caller of `establish` retries across calls (the transport tests each call it once), so no
  other site depended on the flight surviving.
