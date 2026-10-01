# Virtqueues could alias each other (AUD-29-71)

**Date:** 2026-10-01. **Audit:** AUD-29-71 (P1). **Design:** §4.6 (virtio-fs device), virtio 1.2 §2.7.

## Description

`Device::configure` validated each queue's layout alone, and `Virtqueue::check_descriptor` excluded only that
queue's own three rings. A driver could give both required queues the same layout, or post a writable buffer over
the other queue's descriptor table; the device then scattered replies and published used elements over another
queue's protocol state.

## Root cause

Ring ownership was a per-queue property; nothing held the device's queues together.

## Exact edits

- `crates/bridge-virtiofs/src/device.rs`: `configure` checks every pair of queues' rings and refuses
  `DeviceError::QueuesOverlap`; it then hands each queue the other queues' rings.
- `crates/bridge-virtiofs/src/virtqueue.rs`: `Virtqueue::rings`, `guard_foreign_rings`; `check_descriptor` refuses
  `VirtqueueError::BufferOverlapsOtherQueue`.

## Proof

`crates/bridge-virtiofs/tests/device.rs`: identical and partially overlapping layouts refused at configuration; a
writable buffer over the other queue's descriptor table refused before access, the table unchanged. Both tests fail
with the checks mutated out.

## Carried

Concurrent mutation of the rings by a hostile guest during a pass is AUD-29-72's publication-order contract.
