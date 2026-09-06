# Partial-volume slab-slot leak on a late create/clone failure

Status: **reported, not yet fixed** (sibling found while building the §4.2 inode reservation).
Date: 2026-09-06.

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

So on every create/clone path that fails *after* the volume object exists, dropping it leaks those
slots:

- `create`: the `admit_dimensions` failure, the `VolumeCreated` db-mutate failure, the version-slab
  reservation refusal, and the slot-insert failure.
- `clone`: the db-mutate failure, the version-slab reservation refusal, and the slot-insert failure.

The byte reservation and (now) the version reservation are given back correctly on these paths via
`give_back` — this leak is the *physical slab slots*, orthogonal to the budget accounting, which is
consistent.

## Impact

Bounded per failure (one volume's root inode + trie path + root dir); unbounded over repeated
failures. The §4.2 inode-version **reservation** makes this materially more reachable: a client that
retries `create` against a shard whose version slab is fully reserved is refused on each attempt, and
each refusal leaks its partial volume's slots — so a retry loop can fill the slab with orphaned
partial volumes and eventually refuse creates that should fit. Before the reservation, the reachable
paths were the rarer db-mutate and slot-insert failures.

## Fix (proposed, not yet applied)

Free a partial volume's slots on every post-creation failure path — a bounded "discard a fresh
volume" step that returns its root inode, trie nodes and root dir to the slabs (a fresh volume has
only a handful, so no cooperative slicing is needed), routed through the same failure handling as
`give_back`. Prove it with a non-vacuity counter: a loop of late-failing creates must leave the
store's live slab-slot count flat (it grows without the fix).

This is a focused change of its own, touching the create/clone failure handling uniformly rather
than only the reservation's path; it is deliberately **not** bundled into the reservation commit,
whose budget accounting is already correct.
