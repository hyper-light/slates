# A keeper took a newer tracker as its own, and one of the two was never woken (2026-10-31)

**Status:** fixed before it shipped, failing test first, in A-192's change.

## Description

A-192 keeps a tracker per volume while NFS connections hold byte states there, and a consumer task (its keeper) parked
on the volume's change generation feeds each change into the states. On fedora-2 (Linux 6.19.10, 32 cores, load 61,
two CI jobs, the test under `systemd-run --user` at `CPUWeight=1`), arm B's run 4 of five of the hostile NFS suite
failed its task check after 18,711 broken connections and no mismatch:

```
hostile: 18711 breaks, 0 mismatches, live tasks 9 -> 16
every broken connection's task ended: 9 live before, 16 after
```

Arm A, run interleaved with it on the same box, passed that check in all five runs.

## Root cause

A keeper found its tracker by the volume alone (`park`: `trackers.get_mut(&volume)`). A tracker closes when its last
connection ends, and a keeper first runs only after the turn that spawned it. So when a connection opened a tracker and
ended in one turn and another connection opened a new tracker for the volume before the first keeper ran, the first
keeper found the new tracker and took it as its own. Both keepers then parked: each stored its waker in the tracker,
the second over the first, and both registered on the volume's change waiters. At the tracker's close only the stored
waker was woken; the other keeper stayed parked on the volume until its next change, which a test ending in reads, or
a volume left idle, never makes. A loaded shard makes the window wide: its run queue is long, so a spawned keeper waits.

## Impact

None shipped: found before A-192 landed. Had it shipped, each such race would have left one parked task per volume
until the volume next changed, a task slot of the shard's bounded arena held meanwhile, and a duplicate consumer doing
the same work twice while both were parked on an active volume.

## Reproduction

`crates/server/src/nfs_writeback.rs` `tests::a_keeper_ends_with_the_tracker_it_was_spawned_for`: on an audit daemon's
volume, eight rounds of the race laid out on the shard in order (one observation opens a tracker, ends its connection,
and opens another; a second observation lets the shard run what the first spawned; a third ends the second connection).
Before the fix it failed with every round's keeper left (macOS, 1 run of 1):

```
every keeper ends with its tracker: 15 tasks live after, 7 before
```

## Exact edits

- `Tracker` gains `id`, taken from `WritebackStates::next_tracker` when `tracking` opens it, and its keeper is spawned
  as `keep(volume, id)`.
- `park` answers `None`, which ends the keeper, when the volume's tracker is not the one with its `id`, counting
  `nfs.writeback_keepers_stale` when a newer one stands in its place: each tracker has exactly the keeper spawned for it.
- The unit test above, which also asserts that counter at one race a round, so the test cannot pass with the race
  unbuilt. After the fix: 6 runs of 6 on macOS.

## Siblings swept

- The two other tasks that park on a volume's changes find their state by their own identity, not by the volume: a FUSE
  mount by its attachment id (`fuse.rs` `volume_changed`: `s.fuse_mounts.get(&attachment)`; ids come from a counter
  and are not reused), and a WinFsp mount by the mount it serves (`winfsp.rs`, its `mount` passed in).
- `close` wakes the stored keeper and removes the tracker in one borrow, so a keeper woken by it finds its tracker gone
  or replaced and ends either way.
