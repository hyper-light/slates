# A grant a landing found past its term was never ended, and no expiry was audited (2026-11-18)

Goal g247 (every reply list bounded and paged), closing A-266's open item. Found reading the expiry path for that item;
reproduced by use on the stack's branch (`crates/server/tests/grants_pages.rs`, macOS arm64, kiln runner d1323898,
2026-11-18).

## Description

A-266 made the grants table hold live grants: a grant leaves it at its terminal step, and the reap tick ends grants
past their term (`end_expired_grants`, once a liveness budget, a second). Two things were wrong at a grant's term.

- **No expiry was audited.** The sweep recorded `GrantStateChanged { Expired }` alone; the audit's kinds had no expiry.
  An expired grant's history was its `grant_issued` record, which names no term, so the audit could not say whether a
  grant was still usable or had lapsed. AC-2.10 asks for every grant in the audit.
- **A grant a landing found past its term was never ended.** A landing checks its grant in the engine
  (`Grants::check`, `crates/land/src/grant.rs`). The check marked a grant past its term `Expired` in the shard's own
  table and refused the landing. The sweep takes grants that are `Issued` and past their term (`expired_at`), so it
  passed that grant over for good: it was never recorded ended, the durable table kept it `Issued`, the listing showed
  it as issued, and it stayed in the shard's table until a restart, which restored it as issued and only then let the
  sweep end it.

## Root cause

The check changed the grant's state as a side effect of reading it, in the one table that is not durable. Every other
state change of a grant is recorded first (`GrantStateChanged` in the landing's completion atom, or the sweep) and
applied to the shard's table after the record commits (`end_grants_recorded_ended`). The check's change had no record,
and the sweep's selection assumed none could happen.

## Impact

- A grant presented past its term (a landing retried after its approval lapsed) was listed as issued, with no end in
  the audit, for the daemon's life. It could not be used (the check still refused it), so no write was ever made under
  it; the harm was a false listing and an audit with no end for it.
- Every unused grant's end was missing from the audit.

## The fix

- `Grants::check` only reads (`&self`): a grant past its term is refused `GrantExpired` and left as it is, for the
  sweep.
- `end_expired_grants` records each expiry with a `grant_expired` audit record in one atom (`record_expiry`): under the
  principal the grant was made for, since no one acted, with the manifest it bound. On the control shard the record is
  appended in the atom; on another it is owed to the control partition, and the sweep sends the committed atoms'
  records as one batch (a refused atom's records are dropped with it, so each committed atom's are set aside first).
- `AuditKind::GrantExpired` is appended to the record enum and to the wire's typed audit kind, each published
  (`crates/db/tests/wire_order.txt`, `crates/ipc/tests/wire_order.txt`, D-15), and the consumer contract types it
  (`slates_api::AuditKind::GrantExpired`, `CONTRACT` 30).

## Tests

- `a_grant_a_landing_found_past_its_term_is_still_due_for_the_sweep` (`crates/land/src/grant.rs`): a grant checked past
  its term is still returned by the sweep's `expired_at`.
- `every_grant_ended_at_its_term_leaves_its_expiry_in_the_audit` (`crates/server/tests/grants_pages.rs`): three grants
  on each of two shards end at a one-millisecond term; each one's `grant_expired` follows its `grant_issued`, and the
  expiries are counted on both shards, so the control shard's in-atom record and the other shard's owed batch are both
  read.
- `a_grant_a_landing_found_past_its_term_still_ends_and_is_audited` (same file): four landings each present their
  grant once its term has passed; each is refused `GrantInvalid`, and every grant leaves the listing with its expiry
  audited.

Red, each guard undone alone in a copy of the fixed tree (Mac Studio, 2026-11-18): with the check's mark put back, the
unit test's sweep found no grant due (`[]` where `[GrantId(1)]` was due) and the four grants the late landings found
past their term were still listed 30 s later (`grants [1, 1, 2, 2]`, two shards); with the expiry recorded without its
audit record, all six expiries over the two shards had no `grant_expired` record (`[1, 2, 3, 1, 2, 3]`).

## Siblings

- **Fixed the same day (its own commit):** `issue_grant` issues in the shard's table before the durable `GrantIssued`,
  and nothing took the grant back when that record or its atom's commit was refused. The approval was answered
  `Unpublished`, yet the shard's table held the grant, bound to the presented landing: a landing naming the id the
  refused issue took ran under it, and the sweep's record of its end was refused `NotFound` at every tick. A refused
  record now ends the grant in the shard's table, and a rolled-back atom's grants leave it
  (`reconcile_unpublished_effects` with `Grants::retain`). Test: `a_grant_whose_issue_was_refused_covers_no_landing`;
  red with both takeouts undone: the landing naming the refused grant's id ran, and its completion was refused
  `NotFound` (the grant it consumed was in no durable table), where it is refused `GrantInvalid` before anything runs.
