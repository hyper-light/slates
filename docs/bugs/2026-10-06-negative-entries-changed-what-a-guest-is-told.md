# Negative entries changed what a virtio-fs guest is told

**Found:** 2026-10-06, by CI (run 37448849001, the live-guest lane):
`a_guest_device_presents_a_subtree_or_a_snapshot_and_nothing_else` (`crates/server/tests/virtiofs.rs:581`) expected
`ENOENT` for a name outside a scoped guest view and got success (`left: (0, 0, -2)`, `right: (0, -2, -2)`).

## Description

`c3c0024` made a FUSE lookup miss a negative entry (success, node id 0) cached for the directory's lifetime. For a
transport that cannot be sent invalidations (`CacheCoherence::Revalidated`: a virtio-fs guest) the lifetime is zero,
so the miss became a negative entry cached for nothing. The kernel treats that the same as `ENOENT`, but the reply
changed for no gain, and the guest-view test, which states the guest's contract as `ENOENT`, failed.

## Root cause

The negative entry was chosen by the outcome (a miss) alone, not by whether the kernel may keep it. My sibling sweep
for that change ran the bridge-fuse, bridge-virtiofs and server FUSE suites, but not the server's virtio-fs suite,
which runs only in the live-guest lane.

## Fix

`serve_lookup` (`crates/bridge-fuse/src/bridge.rs`) answers a miss with a negative entry only when its lifetime is
non-zero, and with `ENOENT` otherwise. So a guest is told exactly what it was before `c3c0024`, and a kernel mount
that receives invalidations keeps its negative entries.

## Tests

- **Restored to their pre-`c3c0024` contract:** `crates/bridge-virtiofs/tests/device.rs` (the guest's miss is
  `ENOENT`) and `crates/bridge-fuse/tests/volume_bridge.rs` (a revalidating context's miss is `ENOENT`).
- **The negative entry keeps its coverage:**
  - `lookup_dispatches_and_a_miss_is_a_negative_entry_cached_for_its_directorys_lifetime` (`dispatch.rs`, a bridge
    with an unlimited lifetime).
  - `a_name_cached_absent_in_the_root_appears_when_another_attachment_makes_it` (`coherence_mount.rs`, a real
    Linux kernel).
- **On Linux 6.12:** `slates-server --test virtiofs` passes 18 of 18, and the bridge's real-kernel suites pass 22
  of 22.

## Lesson

A change to `bridge::dispatch` reaches every transport that calls it: FUSE mounts, virtio-fs devices, and the
server's guest views. The sweep runs `slates-server --test virtiofs` as well as `--test fuse_mount`.
