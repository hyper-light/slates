# 2026-09-06 — a recovered snapshot id drifted when it was not the first slot

## Description

`Volume::from_image` (§4.8 recovery) rebuilt each snapshot with `Slab::insert`, which assigns the
next free slot (0, 1, 2, … on a fresh slab). A `SnapshotId` *is* the snapshot's slab handle —
`snapshot_handle(id) = Handle::from_raw(id.index, id.generation)` (crates/vfs/src/volume.rs) — so a
snapshot that lived at slot one before the crash came back at slot zero. A client holding the
original id then failed to resolve it: `read_in` looks the id up with `snapshots.get(...)` and
returned `StaleHandle`. The `SnapshotRef` type doc states the opposite guarantee — "a snapshot id a
client holds still resolves after recovery" — so the code did not deliver what the design promised.

The single-snapshot case hid it: one snapshot is slot `(0, 0)` both before and after, so it happened
to round-trip. The drift appears with more than one snapshot, or after any destroy leaves a gap.

## Root cause

`Volume::rebuild_snapshot` (crates/vfs/src/recover.rs) called `self.snapshots.insert(...)`, ignoring
the `snap.id` (`SnapshotRef { index, generation }`) the image faithfully carried. `Slab` had no way
to place a value at a chosen slot: only `insert` (next free slot) existed. So the rebuilt slab packed
survivors densely from zero, and any snapshot whose original slot was not its dense position — a
second snapshot, or the survivor of a destroy — got a new id.

## Impact

Multi-snapshot and post-destroy volumes only. After a daemon restart, a client (or a `Clone` naming
an origin `SnapshotId`) that held a snapshot id issued before the restart would get `StaleHandle` or,
worse, resolve to a *different* snapshot that now occupied the old slot — a faithful-recovery and
identity violation for §4.8. Single-snapshot volumes were unaffected (their id is `(0, 0)` either
way). Content and the head were never corrupted; the defect was purely in which id addressed which
snapshot.

## Fix

`Slab` gains `insert_at(index, generation, value)` (crates/mem/src/slab.rs): it places a value at
exactly the given slot and generation, filling any gap below with reusable vacant slots threaded into
the free list, and returns the handle `(index, generation)`. The contract is append-extending —
`index` at or beyond the slots created so far — which is exactly how `from_image` replays snapshots
(it already sorts them by `(index, generation)` ascending), so no free-list surgery is needed; a
lower index (a duplicate or out-of-order entry, only from a corrupt image) is refused with
`OutOfRange` and an index past the bound with `SlabFull`. `rebuild_snapshot` now calls
`insert_at(snap.id.index, snap.id.generation, …)`, so a recovered snapshot lands at the exact id a
client still holds and a destroyed snapshot's slot becomes a reusable gap.

Exact edits:
- crates/mem/src/slab.rs: add `Slab::insert_at`.
- crates/vfs/src/recover.rs: `rebuild_snapshot` places the snapshot with `insert_at` at its `snap.id`.

## Sibling sweep

The snapshot id is the only client-facing identity that is a slab position. Everything else a rebuild
restores is addressed by a stable key, not by slot, so no sibling instance exists:
- **Inodes** are addressed by inode *number* through the trie (`table_set` maps number → handle), and
  `InodeImage`'s doc says a client's file handle "still resolves" by number — so the inode slab's
  positions may drift freely, and the rebuild rebuilds the trie by number.
- **Directory nodes, blocks, tries, chunks** are internal handles rebuilt structurally (dirs by their
  name→number entries), never held by a client as an id.
- **Volume ids** are recorded in the database (`VolumeCreated`), not derived from a slab slot, and are
  restored by the server's volume recovery, not by this slab replay.
- **Attachment ids** carry their owner in their high bits and attachment recovery is a separate unrun
  gate; nothing here regresses it.

## Test

crates/vfs/tests/recover.rs `a_recovered_snapshot_id_survives_when_it_is_not_the_first_slot`: two
snapshots are taken, the first destroyed, and the survivor (slot one) must still serve its frozen
content through the id the client holds after rebuild — it read `StaleHandle` before the fix and the
correct bytes after. crates/mem/src/slab.rs `insert_at_rebuilds_exact_slots_and_leaves_the_gaps_reusable`
drives the primitive: entries placed at slots 1 and 3 (generation two on slot 3) resolve to those
exact handles, the gaps 0 and 2 are reused by the next inserts, and an out-of-order or out-of-bound
index is refused.
