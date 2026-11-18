# A restarted successor of a taken-over green wrote its records below its takeover's fence (2026-11-18, §4.16 owner-loss recovery, §4.8 "Promotion and takeover", A-333)

Found by A-333's test, `a_successor_adopts_a_greens_ledger_past_its_merge_budget_and_keeps_it_across_its_restart`
(`crates/server/tests/fleet.rs`), whose second half restarts a green's takeover successor warm and asks it to commit the
green's next version. Goal g232 (one primary and two replicas editing the Linux kernel tree) restarts every node under
its chaos, so a successor's restart is an expected event there.

## Description

A takeover re-commits the departed owner's newest records at a promotion epoch, and every holder raises its fence for
the green to that epoch (§4.8 "Promotion and takeover"); a record written below the fence is refused `StaleEpoch`. The
successor wrote the green's records at that epoch only while `placed_heads` named it (`merge_service::record_epoch`;
`fleet::owed_record` for the green's catalog register), and `placed_heads` lives in memory: a warm restart forgets it.
The green's record kept no takeover (`VolumeRecord::taken_over: None`, in `materialize_taken_over_green` before A-333
and in A-333's first form), so after the restart the successor wrote the green's merge and catalog records at its own
host's epoch, every holder refused them, and the green never committed another version. A-344 had fixed the same hole
for a live volume (`fleet::promotion_epoch`, read from `VolumeRecord::taken_over`, A-325), which a green's takeover never
wrote.

## Measured

Kiln r2200 and r2370 on the Mac Studio `d1323898` (2026-11-18, debug build, logs `~/.cache/g232-logs/a333-2/` and
`a333-4/` there):
- **Red** (A-333 with the green's record keeping no takeover): after the restart the successor re-adopted the green
  (`merge.recovery.awaiting_adoption` 1, `merge.replica.rebuilt` 1, `merge.takeover.readopted` 1) and read version 0 and
  the head back, but `placed` stayed false for version 3, which it had committed before the restart, and version 4's
  submit was unanswered at the test's 60 s deadline; its green's catalog ship came back short 4,580 times. Failed after
  531 s (r2370; r2200 the same before the fix).
- **Green**: the whole test, takeover and restart, passed in 4.92 s (r2370).

## Root cause

The promotion epoch is durable state of the takeover, and the green's only copy of it was in memory. Every other piece
of a successor's ownership survives its restart: the adoption (`Op::GreenAdopted`), its ledger (the holder records and
the hold) and its own chain after it.

## Fix

- `crates/server/src/verbs.rs`, `record_adopted_green`: the green's record keeps the takeover that made this node its
  owner, `taken_over: Some(TakeoverRecord { epoch, head_sequence, from_live_log: false })`, with the promotion epoch and
  the adopted version, recorded in the same operation as the adoption.
- `crates/server/src/fleet.rs`, `materialize_green`: the epoch is the greater of the two adopted registers' placed epochs
  (`AdoptedGreen::epoch`).
- `crates/server/src/merge_service.rs`, `record_epoch`: the greatest of the host's epoch, `placed_heads`' and the
  takeover the green's record keeps (`fleet::promotion_epoch`), as `owed_record` already did for the catalog register.

## Siblings

- A volume's takeover (`verbs::materialize_taken_over`) records its takeover since A-325, and A-344 reads it: not this.
- A green that a restarted owner never lost (no takeover) writes at its host's epoch, as its holders fenced it: not this.
