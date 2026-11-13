# A submit resent after its owner restarted was applied twice (2026-11-08, §4.16 "Commit", §4.9 "Exactly-once", A-252)

Found by agents/fixtures on its 1,000-client `f = 1` primary-kill rung, on 2ae4e832 merged with b38a9d66 (adacachy,
`~/.cache/slates-agents-scratch/fixtures/rungs10/r1k-kill0.*`). fixtures saved a first cut and its test
(agents/fixtures-a252-wip, 08448ecf) when it reached its weekly limit; this record is the finished fix.

## Description

At `f > 0` a submit's acceptance waits for its version's merge record to commit at the quorum (AUD-11), and its
completion is recorded only then. The wait lived in memory alone (`merge.awaiting`). The version's append and the work's
reset were already durable.

So a restart between the append and the commit lost the wait. The client resent under its own id, the owner found no
completion (`Seen::New`), and ran the verb again against the work's reset state, as a new version. "A retry under its
own id meets its original answer" (§4.9) was broken.

## Measured

- fixtures' rung: no stall (15.1 s, 3,000 accepted, 32 reconnects), but the oracle found 4,261 failures of one shape.
  The accepted versions ran 1 to 1,124 and then jumped to 1,208, versions 1,125 to 1,207 were on the green with no
  submit to explain them, and 83 clients were told acceptance at 1,208 and up.
- Before the fix, on 2ae4e832 merged with main 9a996608 (the rung, two runs): 109 and 174 oracle failures.
- `crates/server/tests/fleet.rs`:
  - `a_submit_resent_under_its_id_after_its_owner_restarted_is_answered_its_version_once`: red on both Linux runners
    (CI run 42484881444), answered `Submitted { version: Some(2) }`; green with the fix (42484884876).
  - `a_submit_resent_while_its_version_still_waits_after_the_restart_joins_the_wait`: red (the head went to 2); green
    with the fix: no answer and no second version while the record is withheld, then version 1.
- After the fix, the same rung (adacachy, release, primary killed at 5 s): every run that finished had 0 oracle failures.
  Five such runs took 13.1 to 13.9 s, harness exit 0, and `merge.acceptance_rejoined` counted the resends joining the
  wait (95 in one run). Across all the builds of this fix, 7 of 18 runs did not finish: the green fenced at recovery and
  they stalled. That is a different bug, below.

## Fix

The wait is a typed durable state, not reply bytes (the first cut stored a marker inside `CompletionRecord.result`, so
a reply encoding could be mistaken for it):

- `Seen::Awaiting { object, version }` in the completion window (`slates_wire::request::ClientWindow`), between a
  completion and nothing: a request that ran and waits is never `New`.
- `Op::CompletionAwaiting { origin, client, sequence, green, version }`, appended; the snapshot's `ClientCompletions`
  carries `awaiting` (`AwaitingCompletion`), appended. Recovery and snapshots carry it by type.
- `merge_service::stage_awaiting` stages it first in the `Accepted` arm of the submit, ahead of the version's append and
  in the same transaction: the two are one durable step, and a failure fails the accept (the first cut counted the
  failure and went on, which silently lost exactly-once). At `f = 0` the completion recorded in that transaction replaces
  it.
- A request found waiting (`merge_service::serve_awaiting`) is answered its version once it has placed, the completion
  recorded; else it joins the wait. It is never run again. A completion that cannot be made durable is refused typed.

Bounds: a wait counts toward the client's retained entries (`ClientWindow::retained`, the `AcknowledgementOwed` bound),
and leaves when its completion replaces it or the client acknowledges it.

Tests: the window's unit tests; `a_wait_and_its_transaction_are_one_durable_step` (`crates/db/tests/model.rs`: dropped
before the commit neither is there, after it the wait survives recovery and its completion replaces it); the owed
model gains a wait step, so random acknowledgement orders with recoveries hold that a waiting request is never `New`.

## What a takeover does (open and owed, A-255)

`a_submit_resent_after_its_owner_was_taken_over_is_not_applied_again`: the owner dies for good after the version
committed. Nothing runs twice (the head stays 1), and the resend is refused `OwnerUnreachable` without end, so the client
never learns its version. That is not what the design promises: §4.16's failure matrix has a green's owner loss Masked
after takeover, and §4.8's walk-through has retries return their original completion records from the new owner. It is
owed as A-255: the submit's completion carried in the merge record, and the resend reaching the green's successor. RIFL
states the rule (Lee et al., SOSP 2015, §3, read 2026-11-08): "the completion record is stored wherever that object is
stored. If the object migrates, then the completion record must move with it."

Why nothing runs twice meanwhile, by test (`a_works_head_is_never_placed_so_a_takeover_cannot_bring_it_back_to_run_a_resend`):
a work's head can never reach a holder. `Snapshot` refuses a work (`Unsupported`), `LiveShip` finds no store slot for one
(`NotFound`), and the seal schedule places only a head those make (`sealable_head` wants an epoch above 0, and only
`Snapshot` and a takeover's own materialization advance one). So a takeover has no work to bring back, and a submit of
it sent to the successor is refused, not run.

## The fence the rung met

7 of 18 rungs on this fix fenced the green at recovery and stalled: `NotAccepted`, at versions 260 to 448 (4 of 4 runs,
then 0 of 5, then 2 of 3 on a build from an empty target directory, so not a mixed build, as first suspected, then 1 of
6). The fence is W3's and independent of this fix. A build printing the refusal showed `version 251 held 250: increment
base 0: BaseUnavailable { base: 0, floor: 250 }`: a checkpoint's floor and an increment's base, neither of which this fix
reads or writes. The W3 checkpoint folds to the reachable floor without looking at the increments above it, which can
have bases below it, accepted before their works moved on. Recovery restores at the checkpoint and cannot replay them.

liveness's reproduction needs no primary kill and no part of this fix:
`a_checkpoint_leaves_every_later_entry_a_base_recovery_can_replay` (`crates/server/tests/green_checkpoint.rs`,
agents/liveness-checkpoint-floor at e35fc17e, on main 9a996608, which has no `Op::CompletionAwaiting`). The fix is
liveness's, A-257: replay stops deciding again what was accepted, and applies it. A floor rule was tried on paper first
and is wrong: under steady concurrency no version qualifies, so a checkpoint would never fold.

Until A-257 lands this rung can stall, and its count cannot be read as this fix's.
