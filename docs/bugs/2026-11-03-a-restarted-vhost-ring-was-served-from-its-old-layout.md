# A restarted vhost-user ring was served from its old layout

Found 2026-11-03 by the audit of `2dc9f3eb..98000d56` (kiln plan 01a1a34bf7e973069e94f773afdea44a, step 2). Design:
A-169, §4.6; the vhost-user specification's "Ring states".

## Description

Since `663e4df8` (2026-11-02) a `GET_VRING_BASE` stops a ring without ending the device, and a `SET_VRING_KICK`
starts it again. But the device kept serving the queue layout and position it was admitted with:

- After admission, `SET_VRING_NUM`, `SET_VRING_ADDR` and `SET_VRING_BASE` only updated the seam's own ring record
  (`crates/bridge-virtiofs/src/vhost_user.rs`). The admitted `Device` never saw them, and the base was checked only at
  admission. This held for a running ring as well as a stopped one.
- QEMU's `vhost_virtqueue_start` sends the ring's size, its base and its addresses before every kick. The base is the
  last available index, which is zero after a guest reset. A guest reboot or a virtio-fs driver rebind therefore
  starts the rings over at new addresses from index zero. The device would then read its old ring at its old index.
  That memory is no longer a ring to the guest, and the guest's new requests would never be served.
- The only restart test (`stopping_a_vhost_ring_drains_its_taken_request_and_leaves_new_work_until_restart`)
  restarted with the kick alone, over an unchanged layout.

## Root cause

`663e4df8` turned a ring stop from the device's end into a suspension, and kept a restart's kick. It did not decide
what a restart over a changed ring means, and nothing compared the front end's new settings with the device's queues.

## Impact

Linux, slates' own virtio-fs device behind `slates vhost-user` (QEMU, crosvm). A guest whose driver restarts its rings
over the same device would be served from stale ring memory, and its new requests would hang. No run is known to have
done so: the live QEMU suite stops rings only at power-off.

## Fix

- A drained stop's reported index becomes the ring's base. A restart without a `SET_VRING_BASE` (the existing test's
  form) resumes there.
- After admission, a ring takes a new size, addresses or base only while it is stopped and its stop has been answered.
  A running ring moved is a protocol refusal.
- A kick restarts a stopped ring only over the layout it was admitted with, at the index it stopped at. That is what
  QEMU sends after a pause or a stop.
- Any other restart is refused and ends the device. A driver that started over opens a new FUSE session. The device
  cannot yet take that over in place, because the old session's references and DAX windows would be left behind, so
  a reset is a new device.
- Owed: an in-place reset that sweeps the old session as the terminal step does and re-admits every queue, so a
  rebooted guest keeps its device.

## Failing test first

`crates/server/tests/virtiofs.rs`, on adacachy (Linux x86_64, load 86), tree `14290a8a` plus the tests:

```
cargo +1.98.1 test --locked -p slates-server --test virtiofs -- a_vhost_ring_restarted_after_a_guest_reset \
  a_running_vhost_ring_cannot_be_readdressed stopping_a_vhost_ring_drains a_vhost_user_front_end_drives_a_guest \
  --test-threads=1
test a_running_vhost_ring_cannot_be_readdressed ... FAILED          (the acknowledgement was 0: readdressed)
test a_vhost_ring_restarted_after_a_guest_reset_is_refused_not_served_from_its_old_ring ... FAILED
                                                                    (the kick's acknowledgement was 0: restarted)
test a_vhost_user_front_end_drives_a_guest_whose_file_the_host_reads_back ... ok
test vhost_form::stopping_a_vhost_ring_drains_its_taken_request_and_leaves_new_work_until_restart ... ok
```

## Siblings

- The in-process seam (`VmmSeam`'s default `queue_running`) has no ring stops, so it has nothing to resume.
- `SET_MEM_TABLE` already refuses a table that moves or drops a region a configured ring lies in.
