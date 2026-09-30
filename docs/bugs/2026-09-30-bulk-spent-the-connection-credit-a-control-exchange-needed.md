# A bulk exchange spent the connection credit a control exchange needed

**Date:** 2026-09-30. **Area:** `slates-transport` (§4.10a; the constrained-link design §5.3).

## Description

`a_control_exchange_is_not_queued_behind_a_bulk_one_on_the_same_session` failed 1 run in 3 after
6939175 (AUD-29-49). Seed 3's worst control ping took 118.5 ms against a 118.4 ms bound: RTT (40 ms), plus
the queue drain (40 ms), plus four packets' serialization (38.4 ms).

The simulation is deterministic apart from the random test certificates. With the probe printing every
latency, the three trees measured:

| Tree | Worst ping |
|---|---|
| 4f90a5a | 88 ms on every run |
| ce26905 and 62f6872 | 98 ms |
| 6939175 | 117.6–117.9 ms, and 118.5 ms on the failing run |

6939175 changes only the handshake (Initial padding, the amplification allowance). So it moved the phase of
the pings against the bulk flow; it did not change the data path.

## Root cause

A trace of the client's `poll_transmit`, taken whenever a control stream had data, showed the ping held with
`stop=Credit` while little was in flight:

- The ping begun at 401.9 ms first left at 447.1 ms.
- The ping begun at 701.9 ms first left at 770.2 ms, after a 68 ms wait.
- Every hold was for connection credit.

The receiver's connection window equalled one stream's window (`FlowController::connection_max`: consumed
plus `window_ahead`). A lone bulk stream therefore spent every byte of the connection's credit, and a
control exchange on the same session waited for the peer's next `MaxData`, up to a round trip or more.

The packet schedule took classes in priority order. Connection credit was first come, first served. The
module doc's claim that "a bulk transfer never starves a control frame" held only for the stream tier. The
bound's four-packet slack had been absorbing the credit wait until the phase moved.

## Impact

On any session (the fleet's record plane carries record commits, forwarded verbs and content transfers on
one session), a control exchange behind a credit-limited bulk transfer waited up to one credit round trip
more than the design allows. Membership, fencing and register traffic sit in the control class.

## Fix

- **`connection.rs`, `class_credit_reserve`.** A sender of a class leaves one packet's stream bytes of
  connection credit unspent for each class above it. `class_credit` is what a class may spend.
  `next_fresh_frame` caps each class at its own credit and reports `Credit` only when a class with data was
  held.
- **The sender's initial `peer_max_data`** is the initial window plus the bulk reserve. Both ends derive it
  from the shape (R8).
- **`flow.rs`.** `FlowController::new` takes the reserve. `connection_max` advertises consumed, plus the
  window, plus the reserve. The window auto-tunes to the ceiling less the reserve, so the connection's
  credit stays within the receive ceiling, and never below the initial window.
- **Blocked reports** (`note_blocked`, `still_owed`) and the probe's `fresh_can_leave` are judged per class.
- **`streams.rs`.** `Priority::classes_above`.

## Tests

- `a_control_exchange_leaves_at_once_when_bulk_has_spent_its_credit` (unit).
  - Setup: two bulk exchanges spend the bulk class's credit, then a control exchange begins.
  - It was red on the old code: "the control exchange left without a credit update".
  - Now: the control exchange leaves within the pacer's next releases, with no credit update, and the bulk
    class still had the whole window.
- `the_receive_window_autotunes_to_its_ceiling` now asserts window plus reserve equals the ceiling. That
  is the design's rule, and the reserve is inside the budget.
- `a_blocked_sender_reports_even_with_a_path_probe_in_flight` sends until its stream has nothing its credit
  lets it send. A lone stream is now held by its stream window, not the connection's.
- The head-of-line test: the worst of twelve pings (seeds 1–3) is 79 ms, down from 118 ms. The bound is
  tightened to RTT, plus queue, plus two packets (99 ms): the packet being serialized and one pacer gap.

## Sibling sweep

- **Stream slots (fixed the same day).** Bulk exchanges could hold every stream the peer allows
  (`docs/bugs/2026-09-30-bulk-exchanges-held-the-stream-credit-a-control-exchange-needed.md`).
- **The congestion window (by design).** A control frame still waits for the congestion window over bytes
  already in flight. That wait is the queue it cannot jump, and the bound counts it.
- **Retransmissions (reported).** Queued retransmissions go before fresh control frames
  (`take_retransmissions`), so a bulk loss burst can delay a control exchange by the retransmitted bytes.
