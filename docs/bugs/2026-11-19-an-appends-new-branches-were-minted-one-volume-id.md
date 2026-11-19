# An append's new branches were minted one volume id (A-694)

Found 2026-11-19 by g300 landing A-687's import (the `states` repository's three bookmarks, each a new green, in one
entry); fixed in the same piece's own landing, before A-687, as main ruled.

## Description

A-635's append (`crates/server/src/repository/push.rs`) takes every branch a push changes as one entry, all or none. A
new branch (`BranchChange::Create`) is planned with a fresh volume id for its green (`plan_create`), then made in memory
(`make`, `Lineage::branch`), judged, and recorded with the entry. With two new branches or more in one entry, the second
was refused `ContentUnavailable` and the whole entry with it:

```
push_imported -> Err(Changes([ChangeRefused { index: 1, why: Refused(ContentUnavailable) }]))
```

(kiln, macOS arm64, Mac Studio, 2026-11-19 ~23:10Z, a diagnostic test on branch `agents/g300-a687-diag` at 3528cc682:
the import's entry making `bookmark:one:`, `bookmark:secret-one:` and `bookmark:two:` from the root at version 0.)

## Root cause

`verbs::fresh_volume_id` minted its counter from the database's next log sequence (`Db::next_seq`), which moves only when
a record commits. A plan mints every new branch's id before anything is recorded, so every `Create` of one entry got the
same id; `make` then met the second in the lineage map already (`Greens::branch` -> `LineageRefused::Stale`) and refused
it `ContentUnavailable`. A verb that recorded one volume and minted another inside the same transaction would have met
the same id too, since a transaction's records commit as one.

## Impact

Every append that makes two new branches or more was refused whole:
- a git push under `atomic` creating two refs or more (`git_push` builds a `Create` for each new ref, all in one entry);
- a Mercurial push making two lines or bookmarks, or a line and a bookmark (`hg_push`'s plan: a new head on a branch, a
  new branch, a `bookmarks` part or pushkey making a bookmark), as `hg push -B a -B b` does;
- A-687's import of a repository with two bookmarks or more.

Each was answered as a refusal by the store being unavailable (`error:abort` for Mercurial), never as the push's own
fault, and nothing was applied: no data was lost or corrupted.

## Exact edits

- `crates/db/src/catalog.rs`: `VolumeId::counter`, the counter an id was minted at (bytes 10 to 15).
- `crates/db/src/partition.rs`: the partition's `volume_counter`, one past the highest counter of every volume created
  here, advanced in `apply` by each `VolumeCreated` (unconditionally, so a replay advances it alike) and carried by the
  snapshot (`PartitionSnapshot::volume_counter`, appended); `Partition::volume_counter`.
- `crates/db/src/replay.rs`: `Db::volume_counter_at(offset)`, the counter the `offset`-th id minted now takes:
  `max(next_seq, volume_counter) + offset`, none past `register::MAX_OBJECT_COUNTER`.
- `crates/server/src/verbs.rs`: `fresh_volume_id` mints at offset 0 and `fresh_volume_id_at` at any, both from
  `volume_counter_at`.
- `crates/server/src/repository/push.rs`: `plan` mints each `Create` at the count of creates planned before it.

So the ids of one record's new volumes are consecutive and distinct; each creation's apply moves the counter past its
own, so no later mint repeats one, within the transaction or after it; and a recovery, which applies the same records or
restores the snapshot, mints what the live partition would.

## Tests

- `t_a694_an_append_makes_each_new_branch_its_own_green` (`crates/server/src/repository/push/tests.rs`): three new
  branches in one entry, each its own green holding its source's file and bound to its name, and a fourth made after
  by the branch verb, every counter past the one before it.
- `t_a694_a_push_making_two_bookmarks_makes_two_greens` (`crates/server/src/repository/hg_push/tests.rs`): a
  `bookmarks` part making two bookmarks at a held changeset, each its own green at it.
- `t_a694_every_mint_is_new_and_a_recovery_mints_alike` (`crates/db/tests/model.rs`): generated histories of volumes
  created alone and several to one record, the database dropped and recovered at random points; every id new, every
  recovery equal to the live partition and minting the next id the live database would.

Red first (macOS arm64, Mac Studio, kiln r398, 2026-11-19, over main b288b412e; branch
`agents/g300-a694-mint-ids-red` at 8dd016acd, every new branch minted at offset 0): `cargo test --locked -p
slates-server --lib -- t_a694` 0 passed, 2 failed, the append refused and the Mercurial push answered `error:abort`
naming change 1 `ContentUnavailable`. Green (the same run, d53177e26): `-p slates-db --test model -- t_a694
every_recovery` 3 of 3, `-p slates-db --lib -- snapshot` 8 of 8, `-p slates-server --lib -- t_a694 repository::push
repository::hg_push repository::tests git_push vcs_store::` 55 of 55, `-p slates-server --test green_checkpoint` 6 of 6,
fmt clean once its one rewrap was applied.
