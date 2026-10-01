# The guest-memory seam had no ordering contract (AUD-29-72)

**Date:** 2026-10-01. **Audit:** AUD-29-72 (P1). **Design:** §4.6 (virtio-fs), virtio 1.2 §2.7.

## Description

`GuestMemory` promised checked byte copies and nothing about ordering. `push_used` wrote the used element and then
the used index through two ordinary calls, and the available index and the notification flags were read the same
way. VIRTIO requires the reply and the used element to be visible to the driver before the used index, the
device's reads of a published entry to follow the index that published it, and a barrier between publishing the
used index and reading the driver's notification suppression. Source order says none of that to a guest's CPUs.

## Root cause

The seam was modelled on a sequential simulator, where every order holds by construction.

## Exact edits

- `crates/bridge-virtiofs/src/memory.rs`: `Edge` (`Acquire`, `Release`, `Full`) and the required
  `GuestMemory::order`; the trait states that a chain is copied whole and each request byte read once.
- `crates/bridge-virtiofs/src/virtqueue.rs`: `peek` asks `Acquire` after a non-zero available index; `push_used`
  asks `Release` between the element and the index; `interrupts_wanted` asks `Full` before the flags.
- `crates/bridge-virtiofs/src/sim.rs`: the simulated memory fences (`Acquire`, `Release`, `SeqCst`) and records
  each edge with its position among the accesses (`edges`).
- Test seams (`tests/device.rs`, `tests/serve.rs`, `crates/server/tests/common/guest.rs`) delegate `order`.

## Proof

`the_ring_protocol_asks_for_its_ordering_edges_where_virtio_puts_them`: in one served GETATTR the acquire falls
after the index read and at or before the first descriptor read, the release right before the used-index write
and after the element, the full edge after the used index and before the flags read.

## Carried

The native seam (AUD-29-68) must implement each edge with the hardware fence its shared mapping needs; a weak-memory
run of it is owed with it.
