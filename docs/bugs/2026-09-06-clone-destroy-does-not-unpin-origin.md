# Clone destroy does not release its origin snapshot's clone pin

Status: **reported, not yet fixed** (found while wiring the partial-volume discard). Date: 2026-09-06.

## Description

When a clone is created, `Volume::clone_of` (crates/vfs/src/volume.rs) increments `clone_refs` on the
origin snapshot it shares (`snap.clone_refs += 1`), so the origin cannot destroy that snapshot while a
clone still references its root. The counterpart — `Volume::unpin`, which decrements that count and is
documented as "the owner of both volumes (the shard) calls this when a clone's destroy has completed"
— is **never called by the server**. `grep -rn '\.unpin(' crates/` finds it only in vfs tests and a
bench, never in `crates/server`.

## Root cause

The server's destroy verb tears down the clone's own volume (`Volume::destroy` + `destroy_step`) but
never looks up the clone's lineage edge (`Partition::lineage(child)` gives `origin_volume` and
`origin_snapshot`) to call `unpin` on the origin. So a destroyed clone leaves its origin's snapshot
pinned forever.

## Impact

The origin can never `destroy_snapshot` the snapshot a since-destroyed clone was made from — it always
refuses `Pinned`. This is a semantic leak (a snapshot that cannot be reclaimed), not a resource leak of
its own, and it is bounded by the number of clones ever made from that snapshot. It is pre-existing and
independent of the create-failure discard work; the partial-clone-failure path leaves the same residue
as a normal clone destroy, so the discard work does not make it worse.

## Fix (proposed)

In the destroy verb, when the volume being destroyed has a lineage edge, call
`origin.unpin(origin_snapshot)` on the origin's slot once the clone's destroy completes (guarding for
an already-destroyed origin). Drive it from a test: clone a snapshot, destroy the clone, then the
origin can `destroy_snapshot` that snapshot (it refuses `Pinned` without the fix). The same call closes
the partial-clone-failure residue.
