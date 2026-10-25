# A guest's DAX window onto slates' own device shows stale bytes, then zeros, after another path's write (§4.6, A-9)

- **Date:** 2026-10-24
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

## Fix (owed to G-vhost, failing test first)

- Wake the device on the volume's change, as the FUSE mount's coherence round is woken (A-141, A-142), not only on a
  guest request.
- Re-point each window of the changed inode onto the blocks the inode holds now: SHMEM_MAP at the same offset, which
  crosvm makes a `MAP_FIXED` replace with no gap. Unpin the old block after the front end's ack.
- Never SHMEM_UNMAP a range a guest still believes mapped. The exception, beyond a file's new end after a truncate,
  must be decided against what a real guest does there.
- Tests:
  - hermetic, with the stand-in front end in `crates/bridge-virtiofs/tests/device.rs`: another path's write reaches a
    live window with no guest request, and the window shows the new bytes, never zeros;
  - live: the crosvm test G-vhost requires.

## Sibling sweep (to do with the fix)

- The snapshot teardown of writable windows (`request_revoke_writable_inode`) and the revoke on advance
  (`revalidate_windows`) end in the same SHMEM_UNMAP, so a guest reading a revoked window would read zeros there too.
- The local-cache placer's PROT_NONE overlay faults rather than reading zeros; whether a guest recovers from that fault
  is the same question.
