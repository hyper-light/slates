# A resent forward lost its place in the lane (2026-11-18, §4.8 "Lookup", A-306)

Finding (b) of `2026-11-09-a-dead-record-session-was-lent-on-for-want-of-anyone-judging-it.md`: "A read through the
client node during the dead session still misses its 10 s failover bound behind the resends' backlog (about 45,000 to
57,000 batches given up)." Goal g245 of the supervisor's stack: "a resend keeps its original place in the lane".

## Description

A verb a node cannot serve is forwarded to its owner through the peer's forward lane (`crate::forward_lane`): the
forwards queued, begun on the record session as its stream credit admits them, their replies taken. A forwarder gives
its batch up once none of its forwards has made progress for its stall budget, a liveness budget plus the path's tail:
one begun and never answered, or one still queued and never begun. Its caller, `verbs::resolve_and_forward`, sends the
same requests again under their own ids while the owner is not confirmed dead and the failover bound allows.

Each send was a new batch: `enqueue` issued fresh tickets, and the lane begins queued forwards in ticket order. So a
resend went to the back of the lane, behind every forward queued since its first send. Behind a backlog the lane
begins more slowly than a stall budget, a resend was given up again before it was begun, and sent to the back again,
while forwards first queued after it went ahead. The read's wait had no bound but the failover bound, which refused it
`OwnerUnreachable`.

## Measured

`a_forward_lane_leaves_a_session_that_hears_nothing_for_a_new_one` as it stood on main 4f65126ab (256 clients of 32
reads each through b, b's session to a deaf, b's discovery held), with a probe read through b sent just after the
deafness and timed (a diagnostic copy, never landed), on adas-macbook-pro (15 CPUs) beside 8 spinners:

| | Probe answered after | Refused | Batches given up a run |
| --- | --- | --- | --- |
| main, 10 runs, load 37 to 46 | 2.3 to 10.5 s | 1 (`OwnerUnreachable` at 10.5 s; 9.2 and 9.5 s in two more) | 20,349 to 45,106 |
| the place kept, 10 runs, load 27 to 33 | 4.1 to 6.5 s | 0 | 23,185 to 46,987 |

The lane held 6,400 to 8,100 forwards when the replacement session came (the first turn's queue). Main's probe was
served early only when a resend happened to land at the front; with its place kept it waits for the forwards first
queued before it, and for nothing queued after.

## Fix

A forward's place is the ticket its first send was given (`forward_lane::Place`); the lane's queue is ordered by place
and then ticket. `resolve_and_forward` keeps the place across its resends, so a resend is queued where its first send
was, ahead of every forward first queued after it. A batch's requests keep their own places in order. Counted:
`fleet.forward.resent_in_place`, the resends queued at a kept place.

Tests:
- `forward_lane::tests::a_resend_is_begun_ahead_of_every_forward_first_queued_after_it`: P queued, then three more,
  P given up and resent; P is begun first. Red with the place ignored (the resend's own tickets as its place).
- `forward_lane::tests::a_lanes_forwards_are_begun_in_the_order_they_were_first_queued`: generated histories of first
  sends, give-ups (queued or begun), resends and settles against a model of the rule. Red with the place ignored.
- `crates/server/tests/fleet.rs` `a_read_through_a_dead_session_keeps_its_place_behind_a_burst`: the probe sent
  first after the deafness and before a 256-client burst, so its place is the lane's first; it is answered on the
  replacement within its failover bound, and resends were made at kept places. On a Mac beside 8 spinners (load 33 to
  56), interleaved: answered in 2.19 to 2.50 s, 5 of 5; with the place ignored, refused at its bound in 2 of 5
  (`OwnerUnreachable` at 10.5 s; no answer by 11 s) and answered in 2.9 to 5.7 s in the rest. With the return's
  judgment removed it fails 5 of 5, since the session is never replaced.

## Not fixed here

A forward still queued behind the lane's stream credit is given up at its stall budget and resent, though nothing of
it was sent: with its place kept that costs it nothing, but the lane's backlog is cycled once a budget on the control
shard (20,000 to 47,000 batches given up a run above). Owed: a queued forward waits while its lane progresses, given up
only when the lane does not move for a stall budget. GAPS records it.
