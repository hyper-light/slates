# A false death let the members that exist now stand in for the holder of the newest entries (2026-11-18)

## Description

NoLoss failed in `a_member_that_never_held_the_log_never_stands_in_for_its_holder` (`crates/cli/tests/live_chaos.rs`) on
main ca216dbf8, the integrator's diag run 44288273676, Linux x86_64. Writes chaos-166 to 169 were acknowledged at 4.197
to 4.255 s, and after the owner's loss every survivor answered them `NotFound`. The same flow passed on arm64 in that run.

The flow, on five nodes at `f = 1`:
- The owner a, without a council seat, streams writes to a live volume.
- Its recorded holders end up as c and b. b is stopped 155 ms while the writes go on, so c alone holds the newest entries.
- The owner is killed. The moment a survivor (b) has adopted the volume's registers, c is stopped for 3 s.

The flow's rule, A-240's, is that a member that never held the log never stands in for its holder: phase one must wait
for c, and a survivor must serve the volume whole once c resumes.

## Root cause

From the run's log:

- **c was falsely retired.** b, d and e' each folded c Dead at incarnation 0, and Alive at incarnation 1 once it resumed
  and refuted. The 3 s stop outlasted the detector and the council's death confirmation, so the council retired c while
  its process, and its copy of the log, lived.
- **b's live phase one waited for c, then gave up on it** (`fleet.live.takeover_waits_quorum` 20). Once c was retired, only
  b of the recorded holders {c, b} was still a council member. A-309's piece 3 then called the volume past tolerance
  (`live_ship::past_tolerance_wait`, judged against the council's members alone): no `f + 1` of the recorded holders could
  ever answer.
- **The members that exist now stood in for c.** Past tolerance, phase one waits for every member that may hold the log:
  the members among the recorded holders and the owner's cohorts, here b, d and the replacement e'. It adopted the
  greatest head among them (`fleet.live.taken_over_past_tolerance` 1): b's own copy, head 167 at epoch 1. b then shipped
  under epoch 2 from 168, and every entry only c held was lost. Neither d nor e' had ever held the log.

A-309 states the round "waits for every one of them, so a holder of a newer record is never passed over". But a retired
host is not a member, so a holder of the newest record that was alive, only judged dead, was passed over. A-309's
motivating case is a holder whose copy is really gone: a node replaced at its anchor, its store destroyed, which comes
back under another member id. A death verdict is not that: SWIM and the council cannot tell a stopped or cut-off process
from a dead one, and a falsely retired host refutes with its RAM, its copy of the log among it, whole.

## Impact

Acknowledged writes lost: the flow's four, and in general every entry a live volume's owner made durable on a holder
that was falsely judged dead (a stopped or starved process, a partition that heals) while the owner was lost too. A
takeover within tolerance is unaffected. The register takeover past tolerance (A-309 piece 2, `slates_db::takeover`) has
the same shape for a falsely retired holder of the newest register record. It is reported here and not fixed.

## Exact edits (A-345, amending A-309 piece 3)

- `crates/server/src/state.rs`: `LearnedMember::previous`, the member id an anchor had before its last restart.
- `crates/server/src/fleet.rs`: `learn_member` keeps it on a restart (`LearnedOutcome::Restarted`).
- `crates/server/src/live_ship.rs`:
  - `past_tolerance_wait` counts a recorded holder that is not a member as one that may hold the log unless this node
    saw it restart (`restarted_ids`). A false death therefore keeps the takeover within tolerance, and A-240's count waits
    for that holder's report; once it refutes and reports, its newer head is adopted.
  - Past tolerance, which now means that at least one missing holder was seen restarted, it waits for every recorded
    holder that may still hold the log, as well as the cohorts' members.
  - The unit test `a_retired_holder_never_seen_restarted_is_waited_for_and_its_newer_head_adopted` is the flow's decision
    alone: red before (past tolerance at once, b's 167 adopted) and green after (waits, then c's 170). With c seen
    restarted it adopts past tolerance, as A-309 does.
- `crates/server/src/takeover.rs`: a share is not done while an object its round adopted is still being materialized
  here (`materializing`, a fact of `share_done`). This was found by the first run of the flow with the change above
  (Mac Studio, release, 2026-11-18). The loss was gone, but the successor gave the volume up after 99 waits for its
  quorum. The stale holder e refused it every report once c came back (`fleet.live.hold_refused.fetch_unauthorized` 82,
  after 18 reports). e keeps no record of the head (`fleet_held_records` 0), so it authorizes a reader through the
  departed owner's kept retirement (`fleet::content_authority`). The round had adopted the registers, so its share had
  confirmed, and the retirement completed and went while the live takeover still waited (c's re-admission needed it
  gone). Held by `a_round_confirms_only_once_every_object_it_learned_is_adopted` and
  `a_round_is_materializing_only_while_an_object_it_adopted_is_pending_here`.
- §4.10's chaos status, A-345, GAPS.

## What it costs, and the decision left

A recorded holder that is retired and not seen restarted is waited for until the failover SLO's worth of attempts
(`fleet::takeover_attempts`), the bound every takeover gives up at. A false death shorter than that (the flow's 3 s
stop, against 10 s by default) loses nothing now.

A holder away longer than that, whether dead and never replaced or partitioned for longer, has the volume given up on
the successor (`Refusal::NotRecovered`). That is final there: the successor stops attempting it and claims it, and the
holder's copy is kept unasked. A-309 would have served such a volume from whatever the other members held. The choice is
a decision for Ada:
- (a) keep this rule: an acknowledged write is never dropped silently, and such a volume stays unserved until an
  operator acts;
- (b) past the SLO, recover from the members that exist now, as A-309 does: the volume is served, possibly without the
  writes only the absent holder held.

The recommendation is (a), since the live-shipped policy promises that "promotion has no loss window" (§4.10).
