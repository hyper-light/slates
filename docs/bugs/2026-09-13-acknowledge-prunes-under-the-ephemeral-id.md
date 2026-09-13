# A client's acknowledgement pruned completions under the ephemeral member id, not the anchor they are recorded under

Date: 2026-09-13
Area: `crates/server/src/verbs.rs` — `acknowledge` and the deferred-reply completion record.
Severity: exactly-once and bounded growth (§4.9 RIFL; banned item 8): a local client's completion window
was never pruned, and a retry after acknowledgement met its record again instead of `DuplicateRequest`.
Found while validating `docs/bugs/2026-09-13-durability-refusal.md` (the crate's daemon test file had
not been run since the change that introduced this).

## Description

`the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals` failed at its RIFL
scenario (`crates/server/tests/daemon.rs:423`): after `Acknowledge { up_to }` the retry of the acknowledged
request must be refused `DuplicateRequest`, but returned the recorded `Created` again — 2/2 runs, and 2/2
with the durability gate disabled, so independent of that change.

## Root cause

`cefb159` (2026-09-12, task #22's two-id model) keyed a local client's completion records on the node's
**stable cert-anchor** (`serve`: `let origin = state.origin_anchor.0`), so a retry meets its record across a
daemon restart. Two sites kept keying on the **ephemeral member id** (`state.fleet.host().0`):

1. `acknowledge` — the `CompletionsAcknowledged` op pruned the window `(member_id, client)`, which holds
   nothing; the window `(anchor, client)` that holds the records was never pruned (unbounded growth), and
   its lookup still answered `Seen::Completed` after the acknowledgement.
2. The deferred-reply path (a reply held because the client's ring was full) recorded its completion
   under `(member_id, client)`, where `serve` never looks — a retry of a deferred request found
   `Seen::New` and ran the verb again.

On a laptop the member id is `member_id(anchor, generation)`, a hash of the anchor and the generation —
never equal to the anchor even at generation 0 — so the mismatch is unconditional, not fleet-only.

## Fix

Both sites key on `state.origin_anchor.0`, with their comments corrected (the forwarded-write relay
already read the watermark under the anchor, `verbs.rs:556`, and the forwarded prune keys on the
authenticated origin — unchanged). No other completion-origin site uses the member id (`fleet.host().0`
remains where it belongs: `ObjectId::creator` and the fleet status report).

## Validation (2026-09-13)

`cargo test -p slates-server --test daemon`: the lifecycle test FAILED 2/2 before (2.08 s, 2.09 s; the
retry returned `Created`), passes after (2/2 with the durability test, 15.04 s). `cargo test -p
slates-server --lib` 21/21; `slates-client` green.

## Siblings

- The client restart oracle (`crates/client/tests/client.rs`,
  `a_session_outlives_a_daemon_restart_and_its_retry_meets_the_completion_record`) exercises the record
  and lookup keys, not the acknowledge prune — which is why it stayed green while this failed. A retry
  after acknowledgement across a restart is not covered by any test; reported.
- The fork working on task #22 (ephemeral member id) touches the same file; the integrator should apply
  this two-line key change before or with that branch.
