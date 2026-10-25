# A clone's copy-up sealed its origin's open extent behind the origin's image

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record (A-170).
- **Subsystem:** the volume's copy-up, `Volume::make_current_inode` in `crates/vfs/src/volume.rs`, and content in place
  (A-64).
- **Severity:** data loss. After a daemon crash the successor refused every volume of the shard: none was rebuilt, and
  each answered `NotFound`.
- **Exposure:** any shard holding a clone whose origin's snapshot still had a file's last window open (a file written
  within about two idle ticks before the snapshot, A-99). The clone changes that file, and the daemon dies before the
  origin publishes for a reason of its own. Neither DAX nor a VM is needed; the guest test only made the timing likely.
- **Found by:** the daemon-kill chaos test of G-vhost step 4, `crates/cli/tests/guest_vhost.rs`
  (`a_daemon_killed_under_a_guests_live_map_ends_its_device_and_loses_nothing_the_guest_synced`). It failed 2 rounds of 3
  on the box, then 3 of 4 under instrumentation, always in the fastest rounds.

## Symptom

The successor's recovery, on adacachy (2026-10-25):

```
slates-server: partition 1: the recovery images' blocks were not claimed, no volume is rebuilt: EIO
slates-server: partition 1: volume origin not rebuilt: RecoveryIncomplete: the shard's recovery images name blocks that could not be claimed
slates-server: partition 1: volume boot not rebuilt: RecoveryIncomplete: ...
slates-server: partition 1: volume session not rebuilt: RecoveryIncomplete: ...
slates-server: shard 1: recovered 0 volumes from their images (3 refused), ...
```

Instrumenting each refusal path of the claim (`Claims::prepare_with`, a scratch copy on the box) named the block:

```
DEBUG-CLAIM: open block (64, 0) held as open OpenImage { offset: 0, len: 24476, block: (64, 0, 32768), born: 0 };
             image names open OpenImage { offset: 0, len: 24476, block: (64, 0, 32768), born: 1 }
DEBUG-CLAIM: image [8c, 90, ae, 95, ...] refused at inode 3659174697238530
```

The refused image was the session's (the clone's). The first claim was the origin's: both named one block as the open
extent of the file `f`, with different births.

## Root cause

1. A snapshot shares the head's inode records, open extents included ("A snapshot seals the open windows on the next
   write"), and a clone of the snapshot shares them in turn.
2. Every mutation passes through `make_current_inode`, whose copy-up sealed an open extent born in an earlier epoch in
   place. It also rewrote the shared old record to the sealed body (`old.body = copy.body.clone()`), so that every view
   of that record saw the seal.
3. When the mutating volume was a clone, that record was its origin's: the origin's head and its snapshot both use it.
   The clone marked only itself dirty (`note_shared_divergence`), so the origin stayed clean (`Volume::is_clean`). A
   barrier records only a volume that is not clean, so the origin's last committed image stood, still naming the open
   extent.
4. The seal frees what the extent's block no longer needs: the trimmed tail always (A-118), and the whole block where
   a cipher moves the bytes to be sealed (A-99). A later allocation takes the freed block. On the box the clone's own
   next open extent took the whole block, so the box's daemon sealed through its cipher: a trim keeps the block's
   head, which the sealed chunk then holds.
5. Recovery claims every image's blocks before it rebuilds anything (A-64), and requires every image naming a block to
   name it identically. Two records for one block refuse the claim, and the claim is all or nothing: the shard rebuilt
   no volume.

Shown hermetically on macOS by the new test with the debug prints the fix removed: after the clone's write the origin
reported clean (`true`). Its last image named `f` as `Open { block: (0, 0, 65536), born: 0 }`, while its live record
was a sealed, trimmed chunk at `(0, 0, 32768)`.

## Fix (A-170)

A clone never changes a version its origin owns. In `make_current_inode`, an open extent this volume owns (born after
its clone origin) is sealed in place as before. An open extent the origin owns is copied into an extent of the clone's
own, born at the clone's epoch (`ChunkStore::copy_open`), and the origin's record, and so its image, is left exactly as
it was. A copy that cannot be made, or whose record cannot be installed, gives its block back, and the retention charge
too.

The price is a copy of at most one chunk per file the clone changes whose last window its origin's snapshot held
open. The copy is the clone's, so the clone's own snapshot retains it (the retention test now says so). Alternatives
rejected:
- Sealing every open extent at snapshot time shares the window with no copy. But it needs a complete list of open
  extents (a recovered one is not in the idle sweep's sets) and an answer for a pinned DAX block, and it puts
  O(open files) of seals in the snapshot verb.
- Keeping the in-place seal and making the origin publish needs a store-wide register of records sealed by another
  volume, lineage-aware across clones of clones, with a lifecycle of its own: more cross-volume coupling, which is what
  failed here.

## Verification

- **Failing test first:** `crates/vfs/tests/content_in_place.rs`,
  `a_clones_first_write_to_a_file_it_shares_with_its_origin_leaves_the_origins_image_true` (no cipher: the seal trims in
  place and a second file takes the freed tail) and
  `a_clones_first_write_to_a_sealed_volumes_shared_file_leaves_the_origins_image_true` (a cipher: the seal frees the
  whole block, which the clone's next extent takes). Both failed before the fix with the production refusal
  (`RecoveryIncomplete` at the claim) and pass after.
- **The whole vfs suite** passes (43 test binaries), after one test was changed to state the new rule:
  `retained_bytes.rs`, `a_clones_snapshot_charges_nothing_for_the_windows_its_origin_owns`. Its sealed inherited window
  still costs the clone nothing, and the window the origin held open is now the clone's copy.
- The server's recovery suite, and the daemon-kill chaos test on the box: see GAPS (2026-10-25, G-vhost step 4) for
  the runs.

## Siblings

- **The seal's refusal after a retention charge (changed in passing, untested).** In the same copy-up, a version this
  volume's own snapshot retains is charged a retention slot before its open extent is sealed. A seal that refused
  returned without crediting the slot back, unlike the install's refusal. The fix moved the seal and the copy into one
  method (`Volume::copy_up_open_extent`, as clippy's complexity bound asked), whose refusal the caller credits, so
  the seal's refusal is credited now too. Owed: a test that injects the seal's refusal and holds the charge balanced.
- **The cold reseal changes shared chunk records (reported, not changed).** `Volume::reseal_inode` (A-99) re-encrypts a chunk's record in place
  (`ChunkStore::reseal`: a new block and seal). It checks neither that this volume owns the chunk nor marks the inode
  dirty, so another volume's image, or this volume's own last one, may still record the chunk unsealed. Reached today
  only after an in-process DAX map decrypted a chunk (`Volume::note_dax_reopen`), a path no VMM is wired to. Not
  reproduced; owed a recovery test before anything is changed.
