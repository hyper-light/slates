# A node back at its anchor with fresh RAM had every pair key refused (2026-11-07)

Contracts: A-92 piece 4b (each owner shard delivers a pair key to each neighbour, named by stable anchor), piece 4c (a
head's sealing wraps the lineage key under each delivered pair key; a successor opens it through its own), A-200 (a live
write is answered once its last whole entry covers every candidate).

## Description

raft-chaos's kill-at-ack flow (CI, three-way tree) replaces a node with a fresh daemon from the same certificate files,
so the same machine anchor with fresh RAM and a new member id. When that node then owned a live volume, its writes were
never answered (40 s, `fleet.live.held_back.keys` 394): durable on both candidates, but the last whole entry carried no
key a candidate could use.

## Root cause (from counters)

CI diag-raft-chaos-three-25: on the replacement `fleet.seal.pair_undelivered` 1,580; on each survivor
`fleet.seal.pair_refused` 789 and 788 of 793 and 792 served. `seal_keys::accept_pair` recorded one key per (sender
anchor, partition) and refused any other ("another key for a recorded pair"). The replacement made a new pair key (its
RAM, and the old key, gone), and the survivors refused it for good, so its sealings never covered them
(`live_ship::coverable_now` never turned true). The reverse direction was broken too: an owner shard marks a pair
delivered by the receiver's anchor (`ShardState::pairs_delivered`), so the survivors never delivered their pairs to the
new incarnation, which therefore could not open their heads as a successor.

## Impact

Any node replaced at its anchor with fresh RAM (a crash with its segment lost, a pod rescheduled with its certificate)
could seal nothing its neighbours could open: its live volumes' writes waited for ever, and a takeover of its sealed
volumes could not open them; and it could not take over its neighbours' volumes.

## Exact edits

- `crates/db/src/catalog.rs`: `SealKeyOwner::PairRetired { host, partition, key }` (appended).
- `crates/db/src/partition.rs`: `Partition::seal_keys`, every recorded key.
- `crates/server/src/seal_keys.rs`: `accept_pair` takes another key from a recorded anchor and shard as the newer
  incarnation's: in one transaction the old key is kept as retired when something held is sealed under it
  (`sealed_under`: held heads' sealing and held live logs' whole entries, by the wrapping key their entry names), the
  current one is replaced, and retired keys nothing held names are erased. `successor_lineage` picks the pair key the
  head's entry names (`Wrapped.parent`), current or retired.
- `crates/server/src/fleet.rs`, `state.rs`, `live_ship.rs`: `pairs_delivered` maps each anchor to the member id it was
  delivered to; a pair is owed again to a new member id.

## Evidence

`a_node_replaced_with_fresh_ram_at_its_anchor_exchanges_pair_keys_with_its_survivors` (`crates/server/tests/fleet.rs`):
three nodes, one replaced with fresh RAM at its anchor. Before: the survivors refused 111 and 115 of the replacement's
deliveries within the wait (the replacement counted 236 undelivered). After: none refused, and the replacement served
by each survivor, in 3.6 s. With the redelivery removed alone: the replacement was served no pair (`served Ok(0)`).
