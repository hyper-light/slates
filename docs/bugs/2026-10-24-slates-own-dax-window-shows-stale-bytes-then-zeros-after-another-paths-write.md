# A guest's DAX window onto slates' own device shows stale bytes, then zeros, after another path's write (§4.6, A-9)

- **Date:** 2026-10-24
- **Status:** fixed in the device 2026-10-25 (A-168), proven by hermetic tests against a stand-in front end and the
  same day through a real crosvm guest (`crates/cli/tests/guest_vhost.rs`: another path's write in the map as its
  writer is answered, 3 of 3 on the box; red-checked without write-through).
- **Subsystem:** the DAX revoke of slates' own virtio-fs device over vhost-user:
  - `crates/vfs/src/dax.rs`, the registry's step-5 queue (`take_pending_revokes`);
  - `crates/bridge-virtiofs/src/admission.rs`, the drain at the end of a service pass;
  - `crates/bridge-virtiofs/src/device.rs`, `revoke_inode_windows` and `teardown_dax_window`, which send SHMEM_UNMAP.
- **Severity:** correctness, silent wrong data. A guest that maps a file through slates' own DAX window reads stale
  bytes after another path writes the file. Once anything makes the device drain its revokes, it reads zeros: its map
  and its reads both, while the file holds other bytes.
- **Exposure today:** none in production. DAX on this device is unadvertised, and no guest device attaches over the
  ring (`SeamNotOnWire`). The users are the live A/B harness (`crates/client/examples/crosvm_dax_ab`) and the in-repo
  tests. Goal G-vhost (a `slates vhost-user` verb) puts this path in production, so the fix belongs to it.
- **Found by:** a real run on the box, while preparing G-vhost.
  - Setup: crosvm 1c8d17a85, the box's 7.2.9 kernel as the guest's, the A/B harness with DAX on (an 8 MiB window).
  - The guest wrote a 32 KiB file of `A` through its `dax=always` mount, mapped it, and polled bytes 0 and 16384 every
    100 ms. The harness rewrote the file with 32 KiB of `B` through the SDK (`fs_write`) about 12 s after attach.

## Symptom

The guest's console, first run, with no guest request after the write:

```
GUEST-MAPPED b0=65 b16384=65
GUEST-MAP-FINAL b0=65 b16384=65
GUEST-PREAD n=1 b0=65
```

`A` (65) for all 40 s, in the map and through `pread` (a DAX read through the same window).

Second run, the guest `stat`s the file at 20 s:

```
GUEST-STAT t_ms=20000 rc=0 size=32768
GUEST-SAW t_ms=20000 b0=0 b16384=0
GUEST-MAP-FINAL b0=0 b16384=0
GUEST-PREAD n=1 b0=0
```

Zeros, while the volume holds `B`.

## Root cause

1. **Stale.** A write through another path enqueues the inode on the shard's DAX registry (§4.6 step 5), and the device
   drains that queue only at the end of a service pass (`admission.rs`, "honour any daemon-initiated DAX revokes").
   A service pass runs when the guest rings the doorbell. A guest that only reads its map, or reads through it, rings
   nothing, so its window keeps the old, pinned block indefinitely.
2. **Zeros.** When a request finally drives a pass, the drain tears the window with SHMEM_UNMAP. crosvm's frontend
   (`shmem_unmap` → `remove_mapping` → `MemoryMappingArena::remove`) maps anonymous read-only pages over the range, so
   the range reads zeros. The guest's FUSE DAX code keeps its own record of mapped ranges and is never told this one
   went away, so it never re-maps. `crates/vfs/src/dax.rs`'s "the guest re-maps after the fault" does not hold: a read
   of an anonymous page never faults.

## Fix (A-168, 2026-10-25)

The fix first proposed here, re-pointing each window onto the inode's new blocks, was rejected on reading the sources
(kiln research 01a171bff4bd73c6bea625793fb5f821):
- the vhost-user specification forbids a SHMEM_MAP over a live mapping, and unmapping first opens the zeros gap;
- the guest's mapped ranges are 2 MiB and permanent until it reclaims them, and it never asks again for one it holds;
- slates' content moves (an open block that grows is copied to a larger one, an idle one is sealed), and no single block
  covers a 2 MiB range.

What was built instead: the message placer's window maps the file's backing (A-163's object, one per attachment and
file, sized once and never moved), which the volume's write boundary keeps equal to the file before any change returns
(`crates/bridge-virtiofs/src/backed.rs`, `crates/bridge-virtiofs/src/device.rs`).
- A change through any path reaches the guest's map with no wake and no message; nothing revokes such a window.
- Only the guest's REMOVEMAPPING, its FORGET or unmount, and the device's end unmap one.
- The guest's stores reach the volume at its sync points (FSYNC, FLUSH, RELEASE, READ, FORGET, DESTROY) and at the
  device's end, a write-back the volume refuses failing the request that asked for it.
- Beneath an overlay, a reply that could reveal the disk's change waits until the map shows it.

Tests, in `crates/bridge-virtiofs/tests/device.rs` against a stand-in front end that maps what SHMEM_MAP sends:
- `a_window_over_a_backing_shows_every_other_paths_change_with_no_message`: an in-place write, a write past the old
  block, and a cut, each in the map at once; the daemon's revoke and a GETATTR pass send nothing.
- `a_guests_store_through_a_writable_window_reaches_the_volume_before_its_fsync_is_answered`,
  `a_refused_write_back_fails_the_fsync_that_asked_for_it`, `a_writable_upgrade_of_a_live_window_is_answered_with_no_message`,
  `a_guests_unsynced_store_reaches_the_volume_when_the_device_ends`, `a_window_hands_the_vmm_one_files_bytes_and_nothing_else`,
  `hostile_setupmappings_are_refused_with_no_window_and_no_message`, `a_forget_lets_the_nodes_backing_go_after_its_mapping_is_removed`,
  `an_advance_refills_the_backings_and_tears_no_window`, `a_reply_that_could_reveal_the_disks_change_waits_until_the_map_shows_it`,
  `a_reply_held_for_the_owners_barrier_waits_for_the_refill_too`.

Red-checked: the pre-fix code (411c243d, on adacachy) failed the same checks in the old API: "STALE: a write after
the file outgrew its block is not in the map: "AAAAAAAAAAAA"", then "TORN: SHMEM_UNMAP sent for a range the guest still
holds". Eight mutations of the fix each fail their own test (A-168).

The live proof, a real crosvm guest seeing another path's write in its map, is G-vhost's step 3.

## Sibling sweep

- The snapshot teardown of writable windows (`request_revoke_writable_inode`) and the revoke on advance
  (`revalidate_windows`) ended in the same SHMEM_UNMAP. Fixed with the rest: neither touches a window over a backing.
  An advance marks the device's backings stale, so each is refilled from the new view before the guest sees more.
- The fd-sharing placer's `PROT_NONE` overlay faults rather than reading zeros. Whether a guest recovers from that
  fault is the same question, and by the same sources it would not. That placer has no production user (no in-process
  VMM is wired), and the L1/L2 DAX defenses are built on its pinned blocks. Decided in A-168: it keeps them for now, and
  before an in-process VMM ships it takes the backed route or is retired. Recorded in GAPS.
