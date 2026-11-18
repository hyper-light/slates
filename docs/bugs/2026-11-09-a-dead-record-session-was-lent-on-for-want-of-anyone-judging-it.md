# A dead record session was lent on, because nobody that held it judged it (2026-11-09, §4.8 "Lookup", A-261)

Found on the f = 1 primary-kill rung under load (adacachy, ambient load 42 to 108, 2026-11-08) by loadtest-scale: after the
primary's restart, every request through the client node waited 25 to 135 s, with oracle 0. The restarted primary kept
its member id, so the client node's record session to it looked valid to the link, and its borrowers kept forwarding into
a session the new process had no state for.

## Description

A record session is lent whole to one borrower at a time (`fleet::take_sessions`): the forward lane, the council's
replication, epoch announcements, location rounds and the rest, about 25 of them. The link task replaces a session only
when it finds it in its slot: a discovery page that fails, a member id that changed. Under load the session was almost
never in its slot. Its peer was gone without a word, and no holder of it judged that.

## Measured

- The rung's run 7 (two status dumps 11 s apart): client-0's session to the primary `lent=true` in both, its bytes
  consumed unchanged; `fleet.forward.session_never_returned` 9,058 then 16,531; `batch_given_up` 10,606 then 19,223.
- `crates/server/tests/fleet.rs`, `a_forward_lane_leaves_a_session_that_hears_nothing_for_a_new_one`. Under a 256-client
  burst through b, b's session to a is made deaf at its receive path (`Daemon::inject_session_deafness`, test only), and
  b's discovery to a is held (`Daemon::inject_discovery_hold`) so that only a borrower can rescue it.
  - A structured trace: after the deafness, the forward lane got 30 turns on the session, all in the first 600 ms. Then
    `drive_council_replication` borrowed it 52 to 120 times in 10 s, holds of 49 ms on average and up to 1,763 ms, and the
    lane's forwarders gave up without beginning.
  - Red without the rule, 5 of 5 (load 30 to 33): no answer on a replacement within the watch.
  - Green with it, 5 of 5 (load 27 to 47): judged once each. Dropped silent after 1,998 to 2,207 ms against a window of
    1,998 ms, and the lane's first answer on the replacement came 26 to 336 ms after the drop.
- The redial is not the cost: 4 to 53 ms to establish, 0 to 7 ms to enroll, under the burst.

## Fix

- `fleet::return_sessions` drops a returned session that is `peer_silent_past(LIVENESS_BUDGET_NS)` (the transport's one
  rule, live-durability's silence-1: an ack-eliciting packet unanswered for a liveness budget or the session's
  persistent-congestion duration, whichever is longer), and its link entry with it. The link task dials again at its next
  pace. A peer's stateless reset (A-261) is checked first, and counted as a reset.
- The forward lane's forwards on the dropped session are answered unanswered by its next settle, and sent again under
  their own ids, so nothing runs twice.
- Counted: `fleet.link.session_judged_dead`. Logged: each drop's silence against its window. Counted: the lane's first
  reply after any drop of a failed or silent session, within a liveness budget of the drop or past it
  (`fleet.forward.replaced_answered_in_budget`, `…past_budget`).

## Why no timer

Every borrower's hold is bounded by a derived budget (25 borrow sites: `broadcast`/`CommitBudget`, `request_within`,
`commit_record`, `within`, the location round's budget, `record_exchange_budget_ns`). So a borrowed session reaches
`return_sessions`, and is judged there, within its window plus one bounded hold. Because the judgement runs only when a
borrower returns, how late past its window a silent session is dropped is the lane's return cadence: 0 to 209 ms past
the 1,998 ms window in the runs above. A session that sits unborrowed in its slot is the link's discovery refresh's to
catch: a page on a dead session fails, and the link drops it. A first trace that seemed to show a judgment missed was
the discovery refresh rescuing at 0 ms, before any return could see the silence.

A starved node can judge a live peer dead. That costs one session replaced and the forwards sent again under their own
ids, never a second run, and each judgment is counted.

## Not fixed here

A read through the client node during the dead session still misses its 10 s failover bound behind the resends' backlog
(about 45,000 to 57,000 batches given up). It is its own finding (b), measured before it is fixed. Fixed 2026-11-18
(A-306): a resend keeps the place its first send had in the lane
(`2026-11-18-a-resent-forward-lost-its-place-in-the-lane.md`).

## Since (2026-11-18)

The test above missed its 1.1 s recovery bound under load, at 1.33 s (load 41), 1.339 s (load ~52) and 1.384 s (load
28 to 47). In the last, a sent its first reply on the replacement 896 ms after the drop and b's lane took it at 1,384
ms, 488 ms later, while the burst's resends were cycling through b's control shard (62,000 batches given up that run);
the replacement's handshake took 665 ms, its cause not measured apart. The judgment came 104 ms past its window (2,102
ms against 1,998) and the redial began 89 ms after the drop: they were not the cost. The test now keeps the lane
borrowing the session with four readers and no burst, discovery still held: green 5 of 5 and red 5 of 5 with the
return's judgment removed, at load 33 to 56 beside 8 spinners (GAPS).
