# A green's chain kept every version it ever took (2026-11-06, §4.16 "Deltas older than the derived retention fold into checkpoint deltas", §4.8 recovery, CLAUDE.md banned item 8)

Found by agents/loadtest-scale attributing the load test's memory per version (GAPS, "Memory per client,
attributed"). W3 of the load test's walls, assigned by the supervisor on 2026-11-06. Amendment: A-238.

## Description

The partition kept every accepted increment of every green in `green_chains`, in commit order, so recovery could
replay them. Nothing ever folded them: the cap's own comment said "until checkpointing folds old chain entries
(owed)". So:

- each accepted version cost its increment in the partition for good, even when no reader could ever name a version
  that old;
- every snapshot, taken each time the log filled, re-encoded the whole chain;
- recovery replayed every version since the green was created;
- a green whose readers all kept up was refused `Capacity { table: "green_chains" }` once its history passed the
  partition's cap. The client saw that as `BadRequest { reason: "green_chains at capacity" }`.

Fixing that met a second wall, the in-memory twin of the first. Before a verdict the submit folds the green's
histories "only as far as the retention budget needs" (`fold_history_to_budget(floor, budget)`), which stops once
history is at the cap. The submit then secures its own claim (the bytes its overwrite retains), and that claim never
fits: it was refused `BudgetExceeded { available: 0 }` although every version below the head was foldable.

## Measured

`crates/server/tests/green_checkpoint.rs`, on a one-shard debug daemon on adas-macbook-pro-2 (the loaded arm):

- `a_green_whose_readers_keep_up_holds_versions_past_its_chain_cap`. The setup: a 256 KiB green-chain cap, and 200
  versions each overwriting one file's 8 KiB from one work, which is reset at every accept, so the reachable floor is
  the head.
  - Before: refused `green_chains at capacity` at version 31 (the CI run 42332874286, and locally at version 30 with
    the checkpoint disabled).
  - With the chain checkpointed and the fold unchanged: refused `BudgetExceeded { available: 0 }` at version 34.
  - After both fixes: all 200 accepted.
- `a_green_recovered_after_its_chain_was_checkpointed_is_the_green_it_was`: the same 200 under a played anchor, then a
  second daemon over it.
  - Before: refused at version 30.
  - After: recovered at head 200 with its bytes, and version 201 accepted.
- `a_partitions_snapshot_holds_what_its_green_is_not_what_it_has_been`: the partition's snapshot bytes at version
  40 and at version 200, under the derived caps.
  - Before: it grew 1,329,920 bytes over the 160 versions (338,071 to 1,667,991), each version's 8 KiB.
  - After: growth is within its bound of 39,168 bytes (four edits and 40 bytes an accepted increment).

## Root cause

The design asks for old deltas to fold into checkpoint deltas (§4.16). The engine folded its in-memory histories
(AUD-16), but nothing folded the durable chain. The retention settle folded to the cap, not to the cap less the claim
it was settling for.

## Fix

- **The engine** (`crates/merge/src/checkpoint.rs`, `Green::checkpoint`, `Green::restore`; commit e5a5ebb9).
  - A checkpoint is the green's state at a version `F` beyond its origin: for each key whose value at `F` was set
    after version 0, that version and value.
  - It also keeps the identities of the increments accepted up to `F`, so a retry is still answered with its version.
  - Restored over the origin and given the increments after `F`, the green answers exactly as the original for every
    version from `F` on. A proptest over generated origins and chains holds this as an oracle.
  - A base below `F` is refused `BaseUnavailable`, never judged.
- **The database** (`crates/db`).
  - `Op::GreenCheckpointed { green, version, checkpoint }` is appended to the op set. Applying it drops the chain's
    increments up to `version` and replaces the checkpoint held.
  - It is checked guard-then-apply. A version not past the checkpoint held, or past the head, is refused with the new
    `DbError::CheckpointOutOfRange`.
  - A checkpoint no larger than what it releases is admitted at any fill, so a partition at its cap can always fold
    back under it.
  - The snapshot carries the checkpoints as an appended table. `green_head` is the checkpoint's version plus the
    chain after it.
  - The model test gains a checkpoint step, and `a_green_checkpoint_folds_its_chain_and_recovery_holds_it` holds the
    rest: the fold, the budget it returns, the refusals, and recovery from the log and from a snapshot.
- **The server.**
  - `merge_service::checkpoint_green` runs after every accepted submit. It checkpoints at the floor: the reachable
    floor, kept below the oldest merge record still waiting for placement, whose inputs are read from the chain.
  - It checkpoints only when the increments it would fold reach the size of the checkpoint it replaces (the doubling
    rule). So the work of checkpointing is at most the bytes appended, and the chain past the checkpoint stays under
    the checkpoint's own size.
  - `replay_green` restores from the checkpoint and replays only the increments after it. A checkpoint that does not
    decode fences the green `CheckpointCorrupt`.
  - A submit or rebase whose base is below the checkpoint answers `UnknownBase`.
  - The idempotent-append check, a takeover's "already held", `inputs_of` and `merge_chain_identities` count versions
    from the checkpoint.
- **The fold.** `settle_green_retention_leaving` folds until the claim about to be secured fits, never past a live
  reader's version.

## A read below the fold, fixed with it

A read of a green at a named version was refused only past the head. Below the engine's fold, its histories keep
only the value in effect at the floor, so an earlier version was answered as the floor's state or as a file missing.
After a restart from a checkpoint, `f.txt` at version 0 of a base-seeded green read `NotFound` though the origin held
it (`a_base_seeded_greens_origin_survives_a_daemon_restart`, failing on this Mac). In the checkpoint recovery test,
version 1 read `NotFound` (red).
- The fix: `merge_service::green_version` refuses a version below `Green::folded_below` `UnknownBase`, as it does one
  past the head.
- The base-seeded test now keeps a second work at version 0, which holds the floor there: its subject is the origin's
  durability.

## A full chain, typed, and folded once its floor is released (fixed with it, at the supervisor's request)

- The db's `Capacity` refusal reached the client as `BadRequest { reason: "green_chains at capacity" }`, as if the
  request were malformed. It is now `Refusal::TableAtCapacity { table }`, appended.
- A chain held at its floor by a reader fills to its cap and is refused. When the reader goes (a work destroyed or
  rebased), nothing folded the chain: the checkpoint was taken only after an accept, and the full chain refused every
  submit. A submit refused at the cap now checkpoints at the floor first, the doubling rule waived, and checks again.
- `a_chain_held_by_a_reader_is_refused_typed_at_its_cap_and_folds_once_released` holds both: refused typed after
  about 32 versions with a work held at version 0, then accepted once that work is destroyed. Its red and green
  runs are CI's (diag-loadtest-scale-w3-3).

## Impact

Before the fix, a green's durable cost and its snapshot grew with every version it took, and its submits were refused
once the history filled the cap, whatever its readers needed. At 100k clients, three rounds is about 300k increments,
about 48 GB of chain at the attributed 0.16 MB a version, each snapshot re-encoding it. After the fix, the partition
holds a green's changed state once plus a short tail. The one per-version cost left is 40 bytes an accepted identity
(below).

## Siblings, reported, not fixed here

- At `f > 0` each holder's replica of a green (`merge.replicas`) and its persisted holder records still grow a version
  at a time. So does a takeover: it adopts the whole chain from them into the successor's partition, which can meet
  the cap.
- A checkpoint keeps every accepted identity (40 bytes a version) so a retry of an old increment is answered with its
  version. The engine's own `accepted` map keeps them in memory too. Bounding both against the client's completion
  records is owed.
- The checkpoint walk is one synchronous call on the green's shard, O(the keys changed since the origin), not sliced.
- A checkpoint larger than the log ring is refused and counted (`merge.checkpoint.refused`), and the chain stays
  whole. The cap then refuses as before.

