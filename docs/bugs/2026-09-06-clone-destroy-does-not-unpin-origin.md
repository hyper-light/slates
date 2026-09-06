# Clone destroy does not release its origin snapshot's clone pin

Status: **fixed** — the server now releases the origin's pin when a clone's destroy completes (and
when a partial clone is abandoned). Wired together with the `DestroySnapshot` verb that makes it
observable. Date: 2026-09-06.

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

## Fix (applied)

`unpin_origin` (crates/server/src/verbs.rs) looks up the clone's lineage edge and calls
`Volume::unpin(origin_snapshot)` on the origin's slot when the clone's destroy completes in
`step_destroys`, and on the partial-clone-abandon paths in `clone`. A `DestroySnapshot` wire verb was
added (the catalog already had `Op::SnapshotDestroyed`) so the effect is observable: gated in
`crates/server/tests/daemon.rs`, a snapshot a clone was made from is refused destruction while the
clone lives and can be destroyed once the clone's destroy completes. Non-vacuous — neutering
`unpin_origin` leaves the snapshot pinned and the final destroy refused.
