# A transaction whose publication failed stayed applied in memory, so a retry was served a success that a restart then lost (AUD-06)

Date: 2026-09-18. Contracts: §4.8 transactions ("a verb's effects and its completion record are one
durable step"), AC-2.3 (crash-recovery: after `kill -9` at any instruction every acknowledged effect
is present or the request is explicitly incomplete), §4.9 exactly-once; AUD-06 in
`docs/bugs/2026-09-14_AUDIT.md`; GAP-A9-6.

## Symptom

Inside a recorded verb (`verbs::run_recorded`), `Db::begin` opens a transaction, `dispatch` applies
each operation to the partition **immediately** (in-transaction `Db::mutate` applies now and queues
the operation for the record), `record_completion` applies the completion record the same way, and
`Db::commit` appends the one record. When that append failed for a reason a snapshot could not cure
— or the log was full and the snapshot that would have carried the effects was not published — the
verb's reply became a refusal, but the applied effects **and the completion record** stayed in the
partition. A retry of the same request id then met `Seen::Completed` and was answered the original
success from memory, without another durable publication; the next restart replayed the segment,
which held none of it. Source-confirmed by the September 14 audit. A related mis-typing: a
post-append maintenance snapshot that failed also turned the reply into a refusal although the
record was already durable, so the client's first reply denied a committed effect.

## Root cause

`Db::commit` treated every failure the same way — return the error, leave the partition as it was —
and `Db::mutate`'s in-transaction path applies before the commit for intra-transaction visibility
(a later operation must see an earlier one, and the completion record must see the effect). Nothing
undid the applied operations when the record could not be written, and nothing distinguished "not
durable" from "durable, maintenance deferred".

## Fix

The two failure classes are kept apart in `Db::commit` (`crates/db/src/replay.rs`):

- **Before anything is durable** (the append refused other than for space; or `LogFull` and the
  fallback snapshot not published): the transaction is **rolled back** — the partition is re-derived
  from the segment's durable state (`derive_partition`: the newest published snapshot plus the
  records after it, the same derivation `recover` runs at boot; the lease wheel is the partition's
  own, so nothing dangles), the sequence stays where the durable state is (on the full-log path it
  now advances only once the snapshot is published), the rollback is counted, and the typed
  `DbError::Unpublished { seq, cause }` names what stopped the publication. The effects and the
  completion record are gone together, exactly as a restart would leave them.
- **After a durable append**: the maintenance snapshot the policy asks for can no longer fail the
  commit. It is deferred (`since_snapshot_bytes` stays past the policy so the next commit asks
  again), counted (`maintenance_failures`), and the commit returns the sequence it wrote. A log that
  fills before a deferred snapshot succeeds takes the full-log path on the next append, whose own
  snapshot failure is a rollback: durability degrades into typed refusals, never into a record the
  segment does not hold. The non-transaction `mutate` path gets the same maintenance treatment.

On the daemon (`verbs::run_recorded`): a commit refusal is counted under its typed name
(`unpublished` — it is deliberately not recorded as a completion, so a retry re-executes), and
`reconcile_unpublished_effects` releases what the verb built outside the partition for a record the
re-derived catalog no longer holds — a volume slot whose `VolumeCreated` was never published (its
content returned to the store and its credits to the shard's ledgers exactly as a completed destroy
returns them; at most one verb old, so its destroy completes within the destroy slice) and any green
or work state keyed by a volume the catalog does not hold — so a client retrying into a segment that
cannot publish leaks nothing per attempt.

On the wire: a new typed refusal, `Refusal::Unpublished { reason }`, replaces the `BadRequest` a
durability failure used to be reported as; `refusal_of_db` maps `Unpublished`, a `LogFull` that
reaches a verb, and a segment refusal (`Anchor`) to it, so a client retries under the same id rather
than treating its request as malformed.

Test support: `Db::inject_publication_fault(Some(PublicationFault::BeforeAppend | AfterAppend))`
fails the next publication once, as named; `Daemon::inject_publication_fault` installs it on every
shard (a test need not know which shard a verb routes to) and `Daemon::db_publication_counters`
reports each shard's rollbacks, deferred maintenance and snapshots.

## Failing test first, and regression

- `crates/db/tests/publication.rs::a_publication_refused_before_the_append_rolls_the_transaction_back_and_a_retry_re_executes`
  — a recorded create (effect + completion in one transaction) whose append is refused: the commit
  returns `Unpublished` at the sequence it would have taken; the live partition holds neither the
  effect nor the completion (`Seen::New`: a retry re-executes) while the durable volume before it is
  untouched; the sequence did not move; the rollback is counted; a fresh `recover` from the segment
  agrees; the same transaction run again is durable in memory and in the segment with its
  completion. Before the fix the completion stayed `Completed` in memory and the recovery disagreed.
- `…::a_maintenance_snapshot_that_fails_after_the_append_is_deferred_and_the_commit_stands` — with
  a snapshot due on every commit and the after-append fault, the commit stands, the effect and its
  completion are servable, the failure is counted, no snapshot was published, recovery replays the
  record from the log, and the next commit publishes the deferred snapshot (recovery then replays
  nothing: the log was trimmed).
- `crates/server/tests/recovery.rs::an_unpublished_verb_is_refused_typed_a_retry_re_executes_and_a_restart_agrees`
  — by use, over one anchor segment held across two daemons: a create is refused `Unpublished`
  (the shard's rollback counter reads 1; the volume is not listed), the retry under the same id
  re-executes and creates the volume, the restart over the same segment lists it once and answers
  the retried id from the completion record the retry published (the same volume, no second create);
  1.20 s.

## Validation (this box, 18 cores, 2026-09-18)

`cargo test -p slates-db --test publication --test model`: 2 and 10 passed. The server's recovery
suite 8 passed (13.82 s, this history 1.20 s alone) and daemon suite 4 passed; the client suite
(the restart/retry histories) 4 passed. The in-process fleet suite, run alone: 45 passed in
295.17 s (every recorded verb the fleet histories drive goes through the changed commit path).
Clippy `-D warnings` for the database, IPC and server crates clean (the by-use history split into
`names_listed` / `retry_create` / `rollbacks_of` for the complexity gate; the database histories
into `holds_recorded` / `holds_nothing_of`, the one predicate asked of the live and the recovered
partition). fmt clean; `cargo xtask check` ok.

## Siblings reviewed

- `Db::mutate`'s in-transaction path applies an operation before pushing it; an `apply` that fails
  after `check` passed leaves earlier operations applied and queued — the committed record then
  holds exactly what applied, which replay reproduces. Not a durability hole; noted as the invariant
  `check` must keep (a decision inside `apply` that `check` did not make is a recovery bug, as the
  model test's first run found).
- Verbs also mutate `deferred` (cooperative work) and the client table; neither holds an object whose
  existence the catalog asserts, so a rollback leaves nothing there to reconcile. Attachments and
  landings live in the partition and roll back with it.
