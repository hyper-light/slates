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
- After the fix, the same rung (adacachy, release, primary killed at 5 s): five runs, 0 oracle failures, harness exit
  0, 13.1 to 13.9 s. `merge.acceptance_rejoined` counted the resends joining the wait (95 in one run).

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

## What a takeover does

`a_submit_resent_after_its_owner_was_taken_over_is_not_applied_again`: the owner dies for good after the version
committed. Nothing runs twice (the head stays 1). The resend is refused `OwnerUnreachable`, retryable: the work lived on
the dead owner and is not taken over, and completion records stay where their calls were served (A-127), so nothing
can name the version. The client's outcome is unknown until it reads the green, which the load harness's oracle does.
Naming the version would need the merge record to carry the request's key and a work's requests to route to the
green's successor: its own amendment, not done here.

## A note on the first rung

The first four rungs on this fix fenced the green at recovery (`NotAccepted`, at versions 260 to 448) and stalled. Their
build's target directory had been seeded from another tree's, the hazard of cargo reusing a crate built from a
different worktree (its hash leaves out the workspace's path). The next build, in a target directory of its own, ran
the rung clean five times of five. The cause was not proven beyond that, so the landing hash is built from an empty
target directory and run again.
