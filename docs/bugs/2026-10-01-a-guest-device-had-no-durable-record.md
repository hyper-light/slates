# A guest device had no durable record (AUD-29-68, the record leg)

**Date:** 2026-10-01. **Audit:** AUD-29-68 ("Complete the owned in-process or inherited-descriptor binding and its
durable authority/lifetime records"); also closes AUD-29-76's last owed item (a guest's view could not advance).
**Design:** §4.4 attachment lifecycle, §4.6, §4.13; `docs/wip/virtiofs.md` §8 item 2.

## Description

A guest device existed only in its shard's memory:
- `status` did not count it;
- `detach` could not name it;
- recovery could not account for it;
- a guest presenting a snapshot could not `advance`, because `advance` works on attachment records.

The harness never learned an id for its device.

## Root cause

The guest path admitted the device into the registry and the loop table only (`docs/wip/virtiofs.md` §8 item 2
recorded this as the next leg). Its snapshot view lived in a separate per-shard map keyed by a private counter.

## Impact

A guest's authority had no durable trace, and an operator could not see or end it through the verbs every other
attachment answers to. A daemon that died with a device attached left nothing to reconcile, which was harmless
only because there was no record.

## Exact edits

- `crates/db/src/catalog.rs`: `Consumer::Guest` and `AttachForm::GuestTag { tag, scope }`, both appended.
- `crates/server/src/virtiofs.rs`:
  - the device's record id is allocated with its view, before admission, and a snapshot view is kept in the
    shard's `snapshot_views` under it (the per-shard `guest_views` map is gone);
  - after admission the record is committed: consumer `Guest`, the authenticated principal, rights (never write
    for a snapshot), the snapshot, the tag and scope, and a zero token, which every capability check refuses;
  - a refused commit reclaims the device (`GuestDeviceOutcome::RecordRefused`);
  - the harness learns the id (`GuestHarness::on_admitted`);
  - at the loop's end the view closes and the record is removed (a refused removal is counted
    `virtiofs.record_end_refused`).
- `crates/server/src/verbs.rs`:
  - `end_attachment` asks the record's device to revoke. While a device lives, its terminal step closes its view
    after sweeping through it; otherwise the view closes at once.
  - Recovery ends `Guest` records, since their devices died with the process.
  - `next_attachment_id` is the one allocator for the attach verb, the green pin and the owner's mount capability.
    Each of the three had `next_attachment += 1`, an increment that could overflow; it saturates now.

## Proof

All in `crates/server/tests/virtiofs.rs`. Green on macOS and on Linux arm64; the live QEMU guest still passes with
the record.

- `a_guest_device_is_recorded_its_snapshot_view_advances_and_its_detach_ends_it`:
  - the harness learns the id, and `status` counts the device;
  - the guest reads `f` at 3 bytes, `advance` to the second snapshot names `/f`, and the guest then reads 5;
  - `detach` ends the device (`Revoked`, references swept) and its next request goes unanswered;
  - `status` no longer counts it, and both snapshots are destroyable.
  - With the detach's revocation mutated out, the view closed under the live device and it ended
    `VolumeGone(NotFound)`. The first run of this test also found the order bug the `end_attachment` change fixes:
    the sweep was refused because the view closed first.
- `a_dead_daemons_guest_record_is_ended_by_recovery`: a daemon stopped with a device attached leaves its record;
  the next daemon's `status` no longer counts it. With `Guest` mutated out of recovery's filter, it counted 1.
