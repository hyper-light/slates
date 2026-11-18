# A heal miscounted its repairs and left heads naming holders without the content (2026-11-17)

Contracts: §4.10 (the healer: content a recorded holder lost is placed again, and the head names the holders that hold
it; anti-entropy repairs only differing subtrees), §4.8 "Promotion and takeover" (a successor fetches the content from
the holders the head names), banned items 8 (every retry is bounded) and 9 (no swallowed error). Amendment A-276.

## Description

`a_holder_that_lost_placed_content_is_repaired_by_the_healer` (three nodes, `f = 1`) drops the placed content at the
holder the head names and waits for the holder to hold it again with the owner's repair count moved. It failed once in a
whole-suite gate at load 54 (head 92dcd16c, `~/.cache/slates-scratch/fanout/l37-gates.log`):

```
the healer re-put the lost content and the holder holds it whole again (repairs Ok(0) → Ok(0))
```

The message did not say whether the holder held the content again. live-fixes' account of 2026-11-08 (a heal offered the
first-ranked candidate first and placed there, a "two-loss" mechanism) was read from the code, never measured, and
retracted on 2026-11-09: on three nodes every member holds the head, so the door refuses that candidate's put
(`content_retention::admits`), and the test passed 3 of 3 on the old code (diag-live-fixes-66). Goal g248 found the
cause by reading the round's collection rule and measured it; two siblings were found on the way, and live-fixes'
unlanded replace (A-276, 6ec41144a, `origin/agents/live-fixes-1`) was taken over.

## Root cause

Three faults in the healer.

1. **A repair acknowledged after its round stopped collecting was never counted.** A content round's collection is
   bounded by its budget, whose base deadline is the hedge delay (the p95 of the owner's put readings) with one
   extension granted only to a round still progressing (`content_budget_at`). The round's progress measure is its
   acknowledged set, and that set starts with the owner's own hold (`put_content`'s `owner_holds`), so the witness's one
   advance is the round's first judge; once the deadline passes, the round is judged stalled and stops
   (`DispatchWait::judge`). A holder's transfer restarts the wait when it begins, so a refill has about one hedge delay,
   rounded up to the collection poll (a tenth of a period), before the round stops. A refill that outlasts that
   acknowledges as a straggler. `fold_late_content` folded the straggler into the placement, the job recorded and was
   dropped, and no repair was counted, since only the in-round fold counted `refilled ∩ acked`. Every later heal found
   the holder whole (an empty missing set, no refill). So the holder was restored and the count never moved. On a
   loaded machine the refill outlasts the round more often, since the hedge delay is the p95 of puts measured earlier.
2. **A restore could place where the head names no holder.** The heal re-opened the snapshot as an ordinary seal on
   the current cohort, offered it in rendezvous order, placed on the first `f + 1` acknowledgements and re-recorded that
   set at the head's sequence. A cohort member that holds no head for the object (it refused the owner's records, or a
   restart left it none) admits a put at the head's sequence as ahead of its records, and it acknowledged first. The
   named holder stayed empty while the head named it, and the owner's head value at that sequence changed under the
   holders that hold the old one.
3. **A departed holder stayed named** (live-fixes, 2026-11-09, `origin/agents/live-fixes-1`): a head is never rewritten
   at its sequence, so a heal's re-recorded placement never reached the head; a replacement put at the head's sequence
   is refused by a cohort member that holds the head and is not named in it; and a heal had no bound, so one refused
   replace left the healer skipping the volume for good. Its cut recorded the placement and advanced the head as two
   records, the placement first: a refused advance or a kill between the two left the placement naming the new holders
   at the old sequence, a head value the holders refuse for good; found by reading this change.

## Impact

1. The healer's repair count (`Daemon::fleet_repairs`, the non-vacuity counter its tests rely on) missed repairs, and
   the test failed under load though the holder was restored.
2. and 3. A head named a holder without the content. After the owner's death a successor fetches from the holders the
   head names, so the volume had one copy fewer than `f + 1` named, and one further loss lost it.

## Evidence

MEASURE: the red arm (main e7c031aa1's healer with the test support and DIAG counts, `diag-g248-red`) and the green arm,
on one machine, for each test; and the original test under CPU contention.

## Exact edits

- `crates/server/src/fleet.rs`
  - `heal_for`, `heal_decision` and `Heal`: a **restore** of the named holders that are candidates while every named
    holder is a member; a **replace** when one has left (`fleet.heal.named_gone`) or fewer than `f + 1` are named
    (`fleet.heal.named_short`); a named member outside the candidates is left out
    (`fleet.heal.named_outside_candidates`); no heal when no named remote holder can be put to, as at `f = 0`.
    live-fixes' cut replaced a placement naming no remote holder, so on a one-node fleet (`f = 0`) every healer step
    would have advanced the head's sequence by one, for good; the decision is now one pure function with its unit test
    (`the_heal_restores_named_candidates_and_replaces_only_a_departed_or_short_placement`).
  - `start_seal`: a restore's placement is the named holders (`Placement::of`); a replace puts at the head's sequence
    plus one (`SealJob::put_sequence`; `fleet.heal.epoch_exhausted` at the largest).
  - `seal_complete`, `restoring`: a restore is complete once every holder it restores holds the content.
  - `content_work`: a restore offers to every named holder at once; a complete job waits for its record; every heal is
    bounded by the membership horizon (`fleet.heal.unfinished`).
  - `put_seal_content`, `fold_round`, `fold_content_ack`, `fold_late_content`: a restore's refilled holders are kept on
    the job, and a holder's first acknowledgement after a refill is one repair however it arrives
    (`fleet.heal.repaired_late`; `fleet.heal.refill_outlasted_round` for the refills a round did not collect); a round's
    and a straggler's acknowledgements fold only into the job put at their snapshot and sequence.
  - `record_placed_seals`: a restore records nothing (`fleet.heal.verified`); a replace's identity, placement and head
    advance are one transaction (`record_replacement`), every operation checked before it opens and nothing recorded
    unless the head is still the snapshot at the sequence before (`fleet.heal.republished`,
    `fleet.heal.republish_refused`).
  - Test support: `injected_content_ack_delay` in the serve loop holds a content acknowledgement.
- `crates/server/src/head.rs`: `SealJob::put_sequence`, `SealJob::republish`, `SealJob::refilled`.
- `crates/server/src/state.rs`, `crates/server/src/daemon.rs`: `injected_content_ack_delay_ns`,
  `Daemon::inject_content_ack_delay`.
- `crates/server/tests/fleet.rs`: the five tests named in A-276, and the original test's failure message (the holder's
  hold and the owner's heal counts).
- `docs/wip/SLATES_DESIGN.md` (§4.10, A-276), `docs/wip/GAPS.md`, `docs/wip/content-replication.md`.

## Siblings swept

- A named holder that is a member but outside the object's current candidates: neither restored nor replaced (GAPS,
  open, with a recommendation).
- A departure is healed at the at-rest cadence, one volume per hundred periods (GAPS, open, with a recommendation).
- The mirror has no healer (AC-8.15; GAPS, open).
- A takeover's re-commit and a neighbourhood change re-place content under a new holder set (GAPS, open; owed a test of
  this shape).
- The other straggler folds, read 2026-11-17. A late council, root or configuration-fetch reply folds through the same
  function as one collected in its round (`fold_late_replies`: `fold_reply`, `adopt_fetch`). A mirror shipment's late
  acknowledgement is folded and counted apart (`mirror::fold_late_acks`, `fleet.mirror.records_acked_late`), and
  `fleet.mirror.acks` is defined as the acknowledgements returned within their rounds. Only the healer's repair count
  depended on which path an acknowledgement took.
