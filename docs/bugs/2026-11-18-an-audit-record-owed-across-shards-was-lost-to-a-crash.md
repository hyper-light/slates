# An audit record owed across shards was lost to a crash, and a batch sent twice would have been appended twice (2026-11-18)

Goal g247 (every reply list bounded and paged), closing A-265's open item; AC-2.10 ("the audit log replays after
`kill -9` with every grant, manifest and outcome present"). Reproduced by use (`crates/server/tests/audit_crossing.rs`,
macOS arm64, kiln runner d1323898, 2026-11-18).

## Description

A-265 holds every audit record on the control shard's partition, so one sequence numbers the daemon's audit. A record
made on another shard (a presentation's `landing_planned`, a granted landing's entries and finish, a grant's issue or
expiry) was owed to the control partition: kept in the owning shard's memory (`LandingState::audit_owed`) while its
atom committed, then sent as one batch. A daemon that died between the two lost the batch, while the landing, the grant
and the request's completion it recorded stood. A batch that failed at the control shard was counted and dropped too.

## Root cause

The record was durable only at the control partition, and the crossing between the two partitions was a message with
no record on the sending side: nothing could send it again, and nothing at the receiving side could tell a batch sent
again from a new one.

## The fix

- The owning atom records the record in its own partition (`Op::AuditOwed`, the partition's outbox, each record numbered
  by a crossing sequence of the partition's own), so it is durable with the effect it records.
- A batch carries the outbox from its oldest record, so a batch that was lost is carried by the next; the control
  partition appends it in one record (`Op::AuditBatchAppended`), whose apply passes over every record the sending
  partition already delivered (a mark per partition, snapshotted), so a batch sent twice appends nothing twice.
- The control shard acknowledges, and the owner records it (`Op::AuditDelivered`), which empties the outbox; the reap
  tick sends again an outbox whose front has not moved over a whole tick (`landing.audit_resent`), which is how a
  restarted daemon's owed records reach the audit.
- The outbox is bounded by the audit window (`caps.audit_records`), the oldest dropped and counted past it, as the
  window would drop them; the outbox and the marks are in the partition's snapshot, so the anchor's layout is version 10.

## Tests

- `crates/db/tests/audit.rs`: an owed record waits in the outbox until its delivery is recorded, across a replay and a
  snapshot; a batch sent again appends only what is new (an overlap and a gap); the outbox keeps the newest owed records
  and counts what it dropped.
- `a_record_owed_across_shards_survives_a_restart_before_its_batch` (`crates/server/tests/audit_crossing.rs`): with the
  crossing withheld (`Daemon::withhold_audit_crossing`), a presentation on the shard that is not the control shard is
  not in the audit; a daemon started over the same anchor reads it once.
- `a_batch_sent_again_after_a_lost_acknowledgement_appends_nothing_twice` (same file): with acknowledgements withheld
  (`Daemon::withhold_audit_acknowledgement`), the reap tick sends the record again and again; it is in the audit once.

Red, on the Mac Studio (2026-11-18): over the old crossing, with only the withhold stand-in added, the record owed before
the restart was in no audit after it (0 records where 1 was owed); with both of the batch's guards undone (the control
shard's trim and the apply's mark), the resent record was appended 7 times; with the apply's mark alone undone, the db
test's partition appended every batch whole (`[0, 1, 2, 3, 4, 0, 1, 2, 3, 4, 3, 4, 5, 6, 10, 0, 1]` where
`[0, 1, 2, 3, 4, 5, 6, 10, 0, 1]` was owed).
