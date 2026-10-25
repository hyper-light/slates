# A change of a clone reaches a guest's DAX map of its origin (A-163)

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record.
- **Subsystem:** the store's DAX backings, `crates/vfs/src/shards_backing.rs`, and the volume's write boundary that
  writes through to them (`crates/vfs/src/volume.rs`, `crates/vfs/src/base.rs`).
- **Severity:** correctness and isolation, silent wrong data. A guest that maps a file of one volume sees another
  volume's writes to its file of the same number, and the other volume's guest sees this one's.
- **Exposure:** the Shards route (A-163), in production since 2026-10-24, for a volume with a clone in the same shard.
  A snapshot a device presents is such a clone, so a snapshot's guest would see the head's writes. Slates' own
  virtio-fs device (goal G-vhost) is moving to the same backings, so the fix comes first.
- **Found by:** reading the registry while reusing it for slates' own virtio-fs device; confirmed by a failing test.

## Symptom

`crates/vfs/tests/shards_backing.rs`, `a_change_of_a_clone_never_reaches_a_backing_of_its_origin`, before the fix:

```
assertion `left == right` failed: a write of the clone's file reached the origin's map
```

The origin's backing held "the clone's own bytes" at offset 100, written through the clone alone.

## Root cause

- An inode number names a file only within a volume. A clone shares its origin's nodes, and their numbers with them
  (`Volume::clone_of`); only the files it creates get numbers of its own prefix. A snapshot view
  (`crates/server/src/snapshot_view.rs`) is such a clone.
- The registry kept backings by inode number alone (`by_inode: BTreeMap<InodeNo, …>`). So the write boundary of either
  volume wrote into every backing of that number, whichever volume's attachment made it.
- The same key made the registry's other answers cross volumes. These refused more than they should, but leaked
  nothing:
  - the size bound refused a clone's write past the object's size (`FileTooLarge`);
  - `rewitness` refused a file another volume's guest maps to write (`DaxMapped`);
  - `Volume::dax_map_write` refused a writable map of a file another volume's guest maps (`DaxMapped`);
  - an overlay's drift marked another volume's backings stale, which cost one refill from their own volume.

## Fix

- A backing records the volume whose file it backs: that volume's own prefix (`Volume::prefix`). Each volume and each
  view in a shard has a prefix of its own (`crates/server/src/verbs.rs`, `volume_config`), even where the numbers it
  inherited carry its origin's.
- The registry keeps backings by (volume prefix, inode). Every query and every write-through names both.
- The write boundary and the base plane pass their own volume's prefix. The backed session
  (`crates/bridge-core/src/dax_backing.rs`) passes its attachment's, which `DaxState` carries.
- Test: the failing test above, now green, with a write, a truncate and an extend of the clone's file, and the origin's
  own write still reaching its map (non-vacuity).

## Sibling sweep

The same assumption, "an inode number's prefix names its volume", stands elsewhere. Each instance is reported here and
not changed by this fix:

1. **Write-log replay.** `replay_one` (`crates/server/src/verbs.rs`) routes a logged FUSE write to the volume whose
   prefix the record's inode number carries. The record holds only the inode (`crates/server/src/write_log.rs`,
   `Record`). For a clone's inherited file that prefix is the origin's, so a replayed write would land in the origin.
   Suspected from reading; to be proven by a test.
2. **The in-process DAX gate's recall.** Three places key by an inode number's prefix:
   - `DaxRegistry::has_writable_of_prefix` and `writable_of_prefix` (`crates/vfs/src/dax.rs`);
   - `store.dax.recalling(no.prefix())` in `Volume::dax_map_write`;
   - the snapshot recall in `crates/server/src/verbs.rs`, which calls `has_writable_of_prefix`.

   A clone's writable window on an inherited file counts under the origin. A snapshot of the clone would then freeze
   while that window still reaches a shared block. Harness-only exposure today: in-process writable DAX needs
   `writable_dax`, and DAX is unadvertised.
3. **`DaxRegistry::has_writable_of(InodeNo)`.** The backed session uses it to refuse `EBUSY`, and it is keyed by
   number alone. It over-refuses across volumes and leaks nothing.
4. **Prefix allocation wraps.** `volume_config` takes `next_prefix.wrapping_add(1).max(1)` with no check against the
   prefixes of live volumes. Past 65,535 creations in one shard, two live volumes could share a prefix, and with it
   inode numbers. Suspected from reading; to be proven by a test.
