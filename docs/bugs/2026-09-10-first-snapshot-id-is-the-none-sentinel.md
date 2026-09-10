# A volume's first snapshot has the id the catalog uses for "no snapshot"

Date: 2026-09-10
Area: `crates/server/src/verbs.rs` (`placed_state`, `await_placed`), with the id scheme in
`wire_snapshot`/`to_db_snapshot` and `slates_db::catalog::SnapshotId`.
Severity: a wrong answer from `status` and `await placed(region)` for every volume's **first** snapshot —
they report the creation head's placement instead of the snapshot's. Found while proving §4.10 content
replication by use (the new `a_sealed_snapshots_content_replicates_to_the_holder_and_places` never saw
`placed: true` although the durable `SnapshotPlaced` had landed — confirmed by an instrumented trace of
both the record and the verb).

## Root cause

A volume's catalog record carries `head: SnapshotId` and is created with `SnapshotId::default()` (value
`0`) meaning "no snapshot yet"; `placed_state` and `await_placed` branched on `head == default()` to tell
"no snapshot" from "a snapshot". But a snapshot's wire id is its volume-core slab handle packed as
`index << 32 | generation`, and the first snapshot a fresh volume takes lands in slot `0` at generation
`0` — value `0`, exactly the sentinel. So for every volume's first snapshot both readers took the
"no snapshot" branch and reported the **creation head's** recorded placement (sequence 0), never the
snapshot record's `placed` field that the content plane sets.

The db itself was fine: `SnapshotTaken` inserted the record under `(volume, 0)`, and `SnapshotPlaced`
updated it; only the two readers mistook the id.

## Fix

"No snapshot yet" is the volume's **epoch** being zero (it advances with every snapshot), never the head
id being the default. `placed_state` now takes the volume record and branches on `record.epoch == 0`;
`await_placed` resolves its target as: an explicit snapshot → that record; no snapshot and epoch 0 → the
creation head's recorded placement; otherwise the head snapshot's record. The sentinel stays as the
catalog's "not yet" value at creation (nothing else compares against it).

## Sibling scan

Every other `SnapshotId::default()` comparison in the server: the `create` verb sets it; the snapshot
verb and the takeover materialization write real ids; no other reader branches on it. The wire id
scheme itself (slot and generation) is process-local and is not carried across nodes — the fleet
names a snapshot's content by its manifest identity — so the ambiguity is confined to the local
readers fixed here.

## Regression

`a_sealed_snapshots_content_replicates_to_the_holder_and_places` (`crates/server/tests/fleet.rs`):
the first snapshot of a fresh volume must answer `await placed(snapshot, region)` with `placed: true`
once the content plane records it — it answered `false` forever before the fix.
