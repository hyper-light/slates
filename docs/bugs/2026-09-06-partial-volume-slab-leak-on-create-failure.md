# Partial-volume slab-slot leak on a late create/clone failure

Status: **fixed** (slab slots). The reservation-refused path is closed by reserving before
publishing; the db-mutate and slot-insert failure paths now discard the partial volume, returning
its slab slots. A separate residue remains — a clone's origin `clone_refs` pin — tracked in its own
note (docs/bugs/2026-09-06-clone-destroy-does-not-unpin-origin.md), because it is a pre-existing gap
in normal clone destroy, not specific to a failed create. Date: 2026-09-06.

## Description

When `create` or `clone` fails *after* `Volume::create`/`Volume::clone_of` has already succeeded, the
partial volume is dropped without being destroyed, and the store slab slots it allocated are never
freed. Each such failure leaks a small, bounded number of slots; over many failures the growth is
unbounded (banned category #8).

## Root cause

`Volume` and `VolumeSlot` have **no `Drop` impl** that returns their handles to the store's slabs —
slots are freed only by `Volume::destroy`/`destroy_step` (the cooperative teardown). But
`Volume::create` (crates/vfs/src/volume.rs) allocates, up front: a root inode
(`store.inodes.insert`), a trie root and its inserts (`store.tries`), and a root `DirNode`
(`store.dirs.insert`). `Volume::clone_of` shares the origin's versions but still holds handles.

Every create/clone path that could fail *after* the volume object exists is now handled:

- The version-slab reservation runs *before* `Volume::create`/`clone_of` (reserve all credits or none
  before publishing, §4.2), so a reservation refusal — the common path — allocates nothing.
- `admit_dimensions` cannot fail after the reservation moved earlier (it only sets caps to the already
  reserved allowance).
- The `VolumeCreated` db-mutate failure now calls `Volume::discard_partial`, returning the volume's
  slab slots.
- The slot-insert path checks `Slab::has_room` first (a full registry's `insert` consumes and drops
  the slot), discarding the volume when the registry is full instead of leaking it.

The byte and version reservations are given back on all these paths via `give_back`; the slab slots
are now returned via `discard_partial`. A fresh clone shares its origin's versions, so `discard_partial`
frees nothing for it (correct) — but the origin's `clone_refs` pin is not released, the same as a
normal clone destroy (the server never calls `Volume::unpin`); that is the separate note above.

## Impact

Bounded per failure (one volume's root inode + trie path + root dir); unbounded over repeated
failures. The §4.2 inode reservation had made this materially more reachable — a client retrying
`create` against a full version slab was refused on each attempt and leaked each partial volume — but
**that path is now closed**: the reservation is taken before any allocation, so a refusal allocates
nothing (proven in `crates/server/tests/daemon.rs`: 24 refused probes stay `BudgetExceeded`, where
the pre-reorder code leaked into `SlabFull` by the fourth probe). The remaining reachable paths are
the rarer db-mutate (journal full) and slot-insert (`SlabFull` on the volume registry) failures.

## Fix (applied)

`Volume::discard_partial` (crates/vfs/src/volume.rs) runs the volume's own destroy machinery to
completion in one call — `destroy` populates the queue with the objects the volume *made* (born after
a clone's origin epoch, so a clone frees only its own), and `destroy_step` releases them — returning
its root inode, trie nodes and root dir to the slabs. The create and clone verbs call it on the
db-mutate failure, and check `Slab::has_room` before the slot insert (discarding rather than moving
the volume into an `insert` that would consume it on a full registry).

Proven behaviourally in `crates/vfs/tests/discard.rs`: on a 32-slot slab, create-then-discard runs
200 times where a fresh volume takes a dozen-odd slots; non-vacuous — dropping the volume instead of
discarding it exhausts the slab by the second iteration (`SlabFull { capacity: 32 }`).
