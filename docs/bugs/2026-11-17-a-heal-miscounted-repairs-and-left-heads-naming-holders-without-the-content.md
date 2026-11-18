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
cause by reading the round's collection rule and measured it; three siblings were found on the way (the last, a
clock mismatch on main, by this change's first green run), and live-fixes' unlanded replace (A-276, 6ec41144a,
`origin/agents/live-fixes-1`) was taken over.

## Root cause

Three faults in the healer, and a fourth under it.

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
4. **The first round's dispatch was stamped on another clock** (a sibling on main, found by this change's first green
   run). `put_seal_content` stamped `SealJob::first_round_at_ns` from `futures::now_ns()`, the dispatching shard's
   runtime-driver clock, which counts from that shard's start (`nanos_since(epoch)` in each driver). `content_work`
   judges it by the owner shard's `ShardState::clock`, the host's monotonic clock since boot. The owner's clock read
   every first round as long past, so on main the hedge round to the remaining candidates was never held for the
   measured p95 (`hedge_delay_ns`; §4.8 "hedged to the remaining candidates after the measured p95"), and it went out at
   the next period whatever the p95. In this change's first cut every heal that did not finish in its first round
   ended at the next period as unfinished, so the first round's late acknowledgement found no job and its repair was
   never counted: the green arm of `a_repair_whose_acknowledgement_outlasts_its_round_is_counted` failed with
   `fleet.heal.unfinished 4`, `fleet.heal.refill_outlasted_round 1` and `repairs Ok(0) → Ok(0)` (the Studio,
   2026-11-17). live-fixes' bound, the membership horizon (900 ms at the default beat), was also shorter than the span
   a round's own replies may take (1.1 s at least).

## Impact

1. The healer's repair count (`Daemon::fleet_repairs`, the non-vacuity counter its tests rely on) missed repairs, and
   the test failed under load though the holder was restored.
2. and 3. A head named a holder without the content. After the owner's death a successor fetches from the holders the
   head names, so the volume had one copy fewer than `f + 1` named, and one further loss lost it.
4. On main, every seal hedged to all remaining candidates one period after its first round, however slow the measured
   puts: more puts than the design's trigger sends whenever the p95 exceeds a period (a mirror over a long path, a
   loaded fleet), never fewer copies.

## Evidence

Machine: adas-mac-studio.local-2, Apple M3 Ultra (24 performance and 8 efficiency cores), 96 GiB, macOS 26.4, rustc
1.98.1, debug test binaries, 2026-11-17. Each arm is `e7c031aa1` with its patch, built into its own target directory:
- **red**: `e7c031aa1`'s healer with the test support (`Daemon::inject_content_ack_delay`, the same code as the green
  arms'), DIAG counts and prints of each heal round and late acknowledgement, and the green arms' tests
  (`diag-g248-red`, ad4eb1e85, never for main);
- **green**: be7fc2a5e, this change's first cut;
- **green2**: a68704497, with the first round stamped on the owner's clock and the bound derived (root cause 4).

Each test ran alone (`fleet --exact NAME --test-threads 1`), the arms interleaved, the machine's other users at load 7 to
17:

| Test | red | green (be7fc2a5e) | green2 (a68704497) |
|---|---|---|---|
| `a_repair_whose_acknowledgement_outlasts_its_round_is_counted` | FAILED 135 s: holds `Ok(true)`, repairs 0 → 0 | FAILED 93 s: repairs 0 → 0, `fleet.heal.unfinished` 4, `refill_outlasted_round` 1 | ok 4 of 4, 11–12 s |
| `a_heal_restores_the_named_holder_and_not_a_candidate_without_the_head` | FAILED 63 s: the named holder holds `Ok(false)`, the candidate without the head `Ok(true)`, repairs 0 → 1 | ok 2 s | ok 3 of 3, 2 s |
| `a_heal_republishes_the_head_once_a_named_holder_has_left` | FAILED 452 s: the survivors' heads name the departed holder, `republished` 0 | ok 13 s | ok 3 of 3, 13–14 s |
| `a_heal_republishes_a_live_volumes_head_once_a_named_holder_has_left` | FAILED 446 s: the same | ok 13 s | ok 3 of 3, 13–14 s |
| `a_heal_that_cannot_place_ends_at_its_bound_and_the_volume_is_healed_later` | FAILED 66 s: `fleet.heal.unfinished` 0 | ok 12 s | ok 3 of 3, 12–13 s |
| `a_holder_that_lost_placed_content_is_repaired_by_the_healer` (quiet) | ok 11 s | ok 11 s | ok 3 of 3, 11 s |

The red late-repair run's own record: round 0 refilled the holder and collected only the owner's hold; the holder's
acknowledgement came 531 ms after the dispatch as a straggler and was folded with no repair; every later heal found the
holder whole.

The original test under contention, the test clamped to background QoS (`taskpolicy -c background`, so it runs on the
efficiency cores) with spinners clamped the same way, so the contention is the test's alone:
- no spinners, 4 pairs: red 4 of 4 ok, green2 4 of 4 ok;
- 24 steady spinners (three to each efficiency core), 4 pairs: red 4 of 4 ok, green2 4 of 4 ok; the seal measured its
  p95 under the same load (11–40 ms), and every refill fit the round;
- 24 spinners in bursts of 300 ms each second, 8 pairs: red 8 of 8 ok, green2 8 of 8 ok (p95 2–5 ms);
- 64 spinners in bursts of 300 ms each second, red alone until it failed: 23 ok, then **the original failure, naturally**:
  `the healer re-put the lost content and the holder holds it whole again (repairs Ok(0) → Ok(0); the holder holds it
  Ok(true) …)` after 500 s. Its DIAG lines name the cause: the seal measured a p95 of 8.1 ms; heal round 2 refilled
  the holder and collected only the owner's hold; the holder held the content again 7.39 s after the loss; its
  acknowledgement came 128 ms after the round's dispatch, as a straggler, folded with no repair; every later round
  found the holder whole (`refilled []`, placed).
- the same 64 bursting spinners, 30 pairs interleaved (load 22 to 49): red failed 1 of 30 the same way (482 s,
  `repairs Ok(0) → Ok(0)`, the holder holding the content, nine late acknowledgements in its DIAG lines), and green2
  passed 30 of 30, two of them taking the late path naturally and counting it (`fleet.heal.refill_outlasted_round` 1,
  `fleet.heal.repaired_late` 1, repairs 0 → 1). Over the hunt and the pairs, red failed 2 of 54 runs, every failure a
  refill acknowledged as a straggler; green2 failed none of 30. Every spinner was killed and counted gone after each run
  (`spinners: started 64, alive after kill: 0`).

The f = 0 rule: the unit test fails with live-fixes' rule put back (`assertion failed: f = 0: the owner's own copy is
the whole placement; nothing to heal`) and passes with `heal_decision`.

The merged tree (8c2b38739, origin/main 2b4c22d90 merged in), `cargo test --locked -p slates-server` on the Studio: the
library's 268 tests and every test binary ok, the fleet binary 137 of 138. Its one failure,
`a_survivor_holding_no_copy_of_the_works_catalog_learns_its_green_from_a_location_round`, is main's recorded flake (GAPS,
2026-11-17: it fails whenever the node the test deafens is the green's successor, and on main alone); run alone it failed
once (476 s, `OwnerUnreachable`) and passed once (4.1 s). It runs no seal job.

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
  - `content_work`, `heal_bound_ns`: a restore offers to every named holder at once; a complete job waits for its
    record; every heal is bounded by its content round's whole span and a period from its first round
    (`fleet.heal.unfinished`), so the first round's every acknowledgement is folded before it can end.
  - `put_seal_content`: the first round's dispatch is stamped on the owner shard's clock, the one `content_work` judges
    by (see root cause 4).
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
- The two clocks, swept 2026-11-17 over `crates/server/src` (110 runtime-driver readings, 133 host-clock readings). A
  runtime driver's clock counts from its own shard's start, the host clock from boot. Every other stored driver stamp is
  written and judged in one shard's own state on that shard: the death watch and death reports
  (`death_watch_started_ns`, `death_reports`), a dropped record session (`dead_session_dropped_at`), the learner
  fetch's ask (`learner_fetch.asked_at_ns`), a live volume's lanes, the FUSE and WinFSP told-waits, and the NFSv4
  recalls. A content round's late acknowledgement is timed from `LateReplies::Content::dispatched_ns` on the
  dispatching shard, where the round's dispatch settles. The database is handed the host clock throughout. Only
  `first_round_at_ns` crossed shards and clocks.
- The other straggler folds, read 2026-11-17. A late council, root or configuration-fetch reply folds through the same
  function as one collected in its round (`fold_late_replies`: `fold_reply`, `adopt_fetch`). A mirror shipment's late
  acknowledgement is folded and counted apart (`mirror::fold_late_acks`, `fleet.mirror.records_acked_late`), and
  `fleet.mirror.acks` is defined as the acknowledgements returned within their rounds. Only the healer's repair count
  depended on which path an acknowledgement took.
