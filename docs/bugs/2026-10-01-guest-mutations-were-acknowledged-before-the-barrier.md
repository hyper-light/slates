# Guest mutations were acknowledged before the barrier (AUD-29-82)

**Date:** 2026-10-01. **Audit:** AUD-29-82 (P1). **Design:** D-18 (the local daemon-restart boundary), §4.6
(virtio-fs), §4.8 (the barrier before a mutation's reply).

## Description

The daemon's virtio-fs path (`crates/server/src/virtiofs.rs` `ShardBridge`, `crates/bridge-virtiofs/src/device.rs`
`service_queue`) dispatched a guest request against the owner's live volume, scattered the reply and published the
used element at once. Nothing on that path published the shard's recovery image, so a guest's CREATE, RENAME,
SETATTR or close (`FLUSH`) was acknowledged while it lived only in the daemon's arena: a daemon restart lost it.
NFS's stable procedures and the FUSE owner turn both publish before replying. `VolumeBridge::flush` also said its
bytes were "already in the anchor segment", which is not true until a publication captures the volume.

## Root cause

The barrier lived in each transport's server code, and the guest transport's serve loop had no seam to call it: its
service ran inside `BridgeAccess::with_bridge`, which borrows parts of the shard state, while the publication needs
all of it.

## Exact edits

- `crates/bridge-fuse/src/bridge.rs`: `needs_barrier(opcode, error)` — the one rule both transports apply (the FUSE
  channel's `Dispatched::needs_barrier` delegates to it).
- `crates/bridge-virtiofs/src/device.rs`: a chain that needs the barrier is held (`AwaitingBarrier`); the pass stops
  with `Serviced::barrier_owed`; `complete_awaiting(captured)` publishes the used element, or writes `EIO` over the
  reply and gives back its grants (`reclaim_unreported`); counters `barriers_awaited`, `barriers_refused`.
- `crates/bridge-virtiofs/src/admission.rs`: the pass ends at a held chain; `AdmittedDevice::complete_barrier`.
- `crates/bridge-virtiofs/src/serve.rs`: `BridgeAccess::barrier`, run by the loop between the pass and the completion.
- `crates/server/src/virtiofs.rs`: `ShardBridge::barrier` — `publish_shard` and `captured(volume)`; a refusal counted
  `virtiofs.barrier_refused`.
- `crates/bridge-core/src/volume_bridge.rs`: the `flush` comment corrected.
- `crates/server/tests/common/anchor.rs`: the held-segment fixture, shared by `recovery.rs` and `virtiofs.rs`.

## Proof

- `crates/server/tests/virtiofs.rs` `a_guests_acknowledged_close_survives_a_daemon_restart`: the test holds the anchor
  segment; a guest creates, writes, flushes and releases a file, each answered success; the daemon stops (a stop
  publishes nothing) and a second one starts over the segment; the file's bytes read back over NFS. With the barrier
  mutated to publish nothing the test fails: `LOOKUP kept.txt: status 2, expected 0`.
- `crates/bridge-virtiofs/tests/device.rs` `a_mutations_used_element_waits_for_the_owners_barrier`: no used element
  before the barrier; a refused barrier answers `EIO` and gives back one reference and one handle; a write waits for
  none.

## Carried

- AUD-29-83: the guest path still checks only that the slot exists, not the owner's lease (the next item).
- One publication per held chain; group commit over a pass is a measured refinement, not built.
- A revocation or a volume's disappearance while a chain is held ends the loop through the terminal step, whose sweep
  releases every reference the attachment holds and whose ledger restores the charge.
