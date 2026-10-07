# A forwarded verb's reply could be dropped on its way home

**Found:** 2026-10-07, in the sweep of discarded results (`let _ =`) in `slates-server`.

## Description

A verb forwarded to another shard sends its reply home as a task: a `Control::Spawn` on the origin shard's control
channel. The verbs are owner forwards, region promotions, fleet listings, grants, reports and acknowledgements;
`xshard::call_on`'s awaited calls do the same. Two paths lost the reply.

1. **A full channel dropped it.** `registry::send_control` refuses `ControlFull` when the origin's channel is at its
   bound, and seven of the eight sites discarded the refusal (the eighth counted it and dropped the reply anyway).
   The waiter learned only by its own deadline: the client's reply deadline, or `call_within`'s. A full channel is
   a burst on a busy shard, which is exactly the load the p99 target is about, so a burst cost a whole deadline.
2. **A refused borrow left the client waiting.** The owner forward's task began with
   `let Some(..) = with_state(..) else { return; }`. A refused borrow ended the task with no reply sent at all, and
   nothing counted it.

## Fix

- **The runtime.** `registry::send_control_or_return` hands a refused message back (`RefusedControl`).
  `send_control` and `send_control_to_holder` are thin forms of it, so there is one send path.
- **The server.** `xshard::send_back(origin, back).await` carries every result home. A full channel is retried at a
  tenth of a coordinator period (`HEARTBEAT_NS / POLL_PER_PERIOD`) for at most one period
  (`POLL_PER_PERIOD + 1` attempts), so a burst no longer costs the reply. A reply still refused after that, or whose
  origin is gone, is counted on the sending shard (`xshard.reply_dropped`), and its waiter's deadline answers it as
  before. All eight sites use it.
- **The refused borrow.** The owner forward answers its client `HomedElsewhere`, as for an owner not found, through
  the same delivery (`deliver_owner_forward`), and the borrow's refusal is counted (`with_state_counted`).

## Tests

- `slates-rt` `registry::tests::a_control_message_refused_full_is_handed_back_and_sends_once_there_is_room`. Miri:
  registry tests 8/8.
- `slates-server` `xshard::tests::a_result_sent_home_to_a_full_channel_waits_for_room`. A stand-in slot whose
  channel holds one message is filled; the reply arrives once the filler is taken off. With one attempt (the old
  behaviour) the test fails.

## Full fleet suite runs (load average 6–9)

| Tree | Full-suite runs | Failed |
|---|---|---|
| `5404fd78`, before this change | 1 | `a_cross_region_client_finds_the_copyset_successor_instead_of_an_unrelated_live_peer` |
| This change | 3 | the copyset test twice; `a_slow_first_round_candidate_is_hedged_after_the_measured_p95` once (the traced run, placed at 3.001 s against a 3 s hold) |

Both pass alone: the copyset test 3 of 3, the hedge test 5 of 5. The copyset failure is pre-existing and open.

**The hedge failures were the trace, not this change.** Both happened in traced runs (`SLATES_FLEET_TRACE=1`; a
second traced run failed it the same way, placed at 3.0006 s). With the trace on, a wait's opening line observed
every daemon's refusal map with the full observation budget, including the candidate whose control shard the test
holds. So it waited out the 3 s hold before the poll began, and the poll's first answer came after the hold. Fixed:
the trace reads refusals within one liveness budget (`Daemon::fleet_refusals_within`). Traced, alone: passes 3 of
3; with the old read restored, it fails. Untraced, it passed in every full-suite run before and after this change.

## The copyset test, diagnosed so far (2026-10-07)

Untraced full fleet suites: 4 failures in 9 runs (traced: 0 in 4), always `retry not served: HomedElsewhere`.
The test's failure message now carries the foreign daemon's routing counters. The one failure caught with them:

- `fleet.forward.*` did not move: the forward was never the failure.
- `owner_location.round` went 1 → 4, `claim` 1 → 2, `not_owner` 2 → 8, `unavailable` 0 → 2.
- Three rounds over three surviving home peers should give nine answers. One claim and six "not mine" make seven,
  and neither `no_reply`, `session_out` nor `refused_by_peer` moved. So two rounds never asked the successor:
  this node held no record session to it. A round does not wait for a peer with no session (the bounded
  neighbourhood keeps none to most peers), so with no claim the round answered `Unavailable`.

That is the working hypothesis; it is not yet confirmed by a counter. Two counters are added so the next failure
says it outright:

- `fleet.owner_location.no_session`: eligible peers a round never asked because no session existed.
- `fleet.owner_location.forward_unanswered`: a forward that reached its owner but was not answered in time, which
  surfaced as `HomedElsewhere` with no counter of its own.

Three untraced runs since passed 71 of 71. The fix follows once a failure shows `no_session` moving: either dial a
claimant the round skipped, within its liveness budget, or route through a peer that holds a session to it.

