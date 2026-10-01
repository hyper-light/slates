# A destroy left its guest devices to find the volume gone (AUD-29-68–70)

**Date:** 2026-10-01. **Audit:** AUD-29-68–70 (the open "daemon-level volume-destroy-under-a-device test of the
`abandon` path"). **Design:** §4.4 lifecycle ("Revocation stops admission before any later device, queue or VFS
effect"), §4.6.

## Description

`destroy` on a volume a guest device served neither stopped the device nor waited for it. It closed the volume's
snapshot views and began freeing the volume, and the device discovered the loss on its next request. It ended
`VolumeGone(NotFound)` and its terminal step fell back to the registry-only path, so its references were never swept
through the volume. A device presenting a snapshot had its view closed under it.

## Root cause

`destroy` predated guest devices and had no knowledge of them. The device's terminal step needs its volume, or its
view, to exist to sweep through it, but the destroy freed both in the same turn.

## Impact

A destroy under a live guest skipped the device's terminal step. The device was ended by failure, not revoked, and
its sweep did not run. No data was exposed, since the volume was being destroyed.

## Exact edits

- `crates/server/src/verbs.rs`:
  - `destroy` records `Destroying` and asks every guest device serving the volume to revoke
    (`virtiofs::revoke_volume_devices`).
  - With none serving, the teardown starts at once (`start_teardown`: the snapshot views close, then the volume's
    destroy begins).
  - With devices serving, the teardown waits. `step_destroys` begins it (`teardown_started`) once no device serves
    the volume. Each device ends at its next pass boundary, having swept through its volume or view and closed the
    view itself.
- `crates/vfs/src/volume.rs`: `Volume::is_destroying`.
- `crates/server/src/virtiofs.rs`: `revoke_volume_devices`.

## Proof

`crates/server/tests/virtiofs.rs` `a_volume_destroyed_under_its_guest_devices_ends_them_cleanly`, for a device
presenting the head and one presenting a snapshot:
- each answers one request, then the volume is destroyed;
- each device ends `Revoked` with its references swept and no sweep refused, and its next request goes unanswered;
- no reclaim is counted incomplete, and the volume's destroy then completes (`status` refuses it).

Before the fix it failed: the head device ended `VolumeGone(NotFound)`.

Green on macOS and on Linux arm64, where 12/12 virtio-fs tests pass, the live QEMU guest included.

During the full server library run one unrelated test,
`verbs::tests::a_takeover_keeps_the_volumes_grants_and_its_locked_policy`, missed its 10 s execution deadline with
the machine at load average 35 (other sessions). It passed alone twice (5.6 s, 4.0 s) and in a full rerun (150/150).
It is recorded here, not fixed.
