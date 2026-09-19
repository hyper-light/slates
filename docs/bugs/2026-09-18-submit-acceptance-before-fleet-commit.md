# A submit's acceptance was published before its merge record committed at the quorum (AUD-11)

Date: 2026-09-18. Contracts: §4.16 "Commit" ("committed at f+1 acknowledgements … issued only when
every identity the version references is placed. The reply is `Accepted{version}` or
`Conflict{windows}`"), §4.9 exactly-once; AUD-11 in `docs/bugs/2026-09-14_AUDIT.md`; GAP-A9-14.

## Symptom

`verbs::submit` advanced the engine, appended the local `GreenAdvanced` record, consumed the work's
journal, queued the version's merge record for the record plane (`merge_service::enqueue_record`,
which at `f > 0` leaves the placement pending) and returned `Submitted { version: Some(v) }` at once.
The enclosing `run_recorded` committed the local transaction — and the request's completion record —
without awaiting the fleet placement. With `f = 1`, remote acceptance stopped and the owner lost, the
reply named a version absent from any surviving commit quorum, and a retry met a completion record
saying so. `await placed` could tell a client later, but that is not what §4.16 calls an accepted
commit. Source-confirmed by the September 14 audit.

## Root cause

The reply and the completion record were produced by the synchronous verb, on the local append, with
the record plane's acknowledgements arriving in the background and nothing joining the two.

## Fix

At `f > 0` the acceptance **waits for the commit** (`merge_service`):

- `submit` still commits the version locally (the chain append is the durable input the record
  names) but, when `placed_version` has not reached the version, registers the request as
  **awaiting** (`defer_acceptance`: the reply route — shard, client slot, request word — and the
  completion key; `MergeShardState::awaiting` by green and version, indexed by completion key;
  bounded by the clients' credit, one entry per request in flight) and marks the verb deferred.
  `run_recorded` then commits the verb's effects but records **no completion** and returns no reply
  (`Option<ReplyBody>`; the serve path treats it as forwarded — the reply comes through `deliver`).
  At `f = 0` the append is the commit and the reply goes now, as before (R8: the same code path,
  `placed_version` decides).
- When the version's record is placed at the quorum (`record_merge_acks`), `resolve_accepted`
  answers every waiting request: the acceptance is recorded as its completion on the owner
  partition in one durable step (a rollback there — AUD-06 — is counted and leaves the retry to
  re-execute into the engine's idempotent accept), then the reply is delivered by a task spawned on
  the request's shard (`state::deliver`; a spawn the target's admission refuses is counted and the
  completion record answers the retry). Counted: `merge.acceptance_deferred`, `.acceptance_resolved`,
  `.acceptance_unrecorded`, `.acceptance_undelivered`.
- A **retry while waiting** joins the waiting entry (`join_awaiting`, from both the local serve path
  and `run_forwarded`): it is answered with the committed result when the version places, never a
  success from memory; and a resubmit the engine answers from its idempotent accept appends nothing
  to the chain twice.
- A request **forwarded from another node** has no ring route: its fleet exchange polls the owner
  shard's completion window each period within the liveness budget (`await_deferred_completion`),
  then answers from the record; past the budget it is refused retryable (`Overloaded` naming the
  owner shard) with the acceptance still waiting, so the origin's retry meets the record once the
  version commits.
- Test support: `MergeFault::refuse_records` (a holder withholds every merge record's
  acknowledgement; counted `merge.record_withheld`) beside the existing `refuse_content_puts`, and
  `Daemon::merge_awaiting(green)`.

What this does **not** yet cover: a retry after the **owner's loss** must be answered by the
successor from a servable green — the recomputed chain's idempotent accept — which is AUD-14's
takeover recovery; that regression lands with it.

## Failing test first, and regression

`crates/server/tests/fleet.rs::a_submit_is_answered_only_once_its_record_commits_at_the_quorum` —
two-node `f = 1`: with the holder refusing content puts the submit's bounded wait times out, one
acceptance waits on the owner and the version is unplaced; with the inputs placing but the
acknowledgements withheld, over a whole hold window the version stays unplaced and the acceptance
keeps waiting while the holder counts what it withheld; with both lifted the record commits at the
quorum, the wait resolves (counted), the holder recomputes version 1, and a retry of the same request
meets the completion record: `Submitted { version: Some(1) }`. Before the fix the first call returned
`Submitted { version: Some(1) }` at once under both faults.

Two existing fleet histories asserted the defect and were corrected with it:
`a_merge_record_is_issued_only_once_its_inputs_are_placed` and
`a_holder_whose_recomputation_mismatches_refuses_the_version_loudly` both began with
`submit_one_version` expecting `Submitted { version: Some(1) }` at once while the holder could not
acknowledge — the reply before the commit. The helper now expects the bounded wait to return
without a reply (the acceptance waits for the commit the fault withholds), and the inputs history
drains the late reply the lift produces before its `await placed` call.

## Validation (this box, 18 cores, 2026-09-18)

This history alone: 7.77 s (8.74 s on its first run). The three merge fleet histories together
(this one and the two corrected ones): 23.25 s. The server's daemon suite 9 (14.43 s) and recovery
4 (the `f = 0` submit flows answer at once as before); the client suite 4. The in-process fleet
suite, run alone: **46 passed, 0 failed, 300.74 s**. Clippy `-D warnings` for the server crate
clean; fmt clean; `cargo xtask check` ok.

## Siblings reviewed

- `rebase` commits nothing to the green and needs no commit gate.
- `await placed` keeps its separate meaning (whether every committed version's record placed); it is
  no longer the only way a client learns a version is committed.
- The head-record path of plain volumes already places before it replies (`await_placed` on the
  creation head) — not a sibling.
