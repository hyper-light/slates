# Partial-volume slab-slot leak on a late create/clone failure

Status: **partially fixed** — the reservation-refused path is closed by reserving before publishing
(the common path the §4.2 inode reservation made reachable); the db-mutate and slot-insert failure
paths still leak (pre-existing, rarer) and want the fix below. Date: 2026-09-06.

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

So any create/clone path that fails *after* the volume object exists, dropping it, leaks those slots:

- `create`: **fixed** — the version-slab reservation now runs *before* `Volume::create` (reserve all
  credits or none before publishing, §4.2), so a reservation refusal allocates nothing. Still leaking:
  the `admit_dimensions` failure, the `VolumeCreated` db-mutate failure, and the slot-insert failure.
- `clone`: **fixed** — the reservation likewise runs before `clone_of`, so a refusal leaks nothing and
  does not leave the origin's `clone_refs` bumped. Still leaking: the db-mutate and slot-insert failures.

The byte and version reservations are given back correctly on all these paths via `give_back` — the
remaining leak is the *physical slab slots*, orthogonal to the (consistent) budget accounting.

## Impact

Bounded per failure (one volume's root inode + trie path + root dir); unbounded over repeated
failures. The §4.2 inode reservation had made this materially more reachable — a client retrying
`create` against a full version slab was refused on each attempt and leaked each partial volume — but
**that path is now closed**: the reservation is taken before any allocation, so a refusal allocates
nothing (proven in `crates/server/tests/daemon.rs`: 24 refused probes stay `BudgetExceeded`, where
the pre-reorder code leaked into `SlabFull` by the fourth probe). The remaining reachable paths are
the rarer db-mutate (journal full) and slot-insert (`SlabFull` on the volume registry) failures.

## Fix (remaining paths — proposed, not yet applied)

Free a partial volume's slots on the db-mutate and slot-insert failure paths — a bounded "discard a
fresh volume" step that returns its root inode, trie nodes and root dir to the slabs (a fresh volume
has only a handful, so no cooperative slicing is needed), and for a clone also decrements the origin
snapshot's `clone_refs`. Prove it with a non-vacuity counter: a loop of late-failing creates on those
paths must leave the store's live slab-slot count flat (it grows without the fix). These paths are
harder to drive from a test (they need a journal-full or registry-full daemon), so they are left as a
focused follow-up rather than bundled here.
