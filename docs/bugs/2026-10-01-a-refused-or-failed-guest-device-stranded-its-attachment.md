# A refused or failed guest device stranded its attachment (AUD-29-69, AUD-29-70)

**Date:** 2026-10-01. **Audit:** AUD-29-69 (P1), AUD-29-70 (P1). **Design:** §4.6 (virtio-fs admission and terminal
step), §4.13.

## Description

- **Refused admission (69).** `admit` admitted the registry attachment before reading the queues, mapping memory and
  publishing the configuration; a refusal at any of those released only the seam. The attachment stayed live: the
  owner's next barrier closed it, and repeated refusals consumed the registry's fixed slots.
- **Terminal step (70).** `AdmittedDevice::reclaim` returned early when no context could be minted or the sweep
  refused — before the registry attachment was drained and the seam released; the serve loop then unregistered and
  dropped the device. A device whose volume was gone never reached the terminal step at all (`with_bridge` refused),
  the daemon's refused-loop branch discarded the reclamation's result, and a dropped device released nothing.

## Root cause

Cleanup was written as a sequence that stopped at the first refusal, and the registry was reachable only through a
bridge to a live volume.

## Exact edits

- `crates/bridge-virtiofs/src/admission.rs`: `withdraw` (revoke and drain) on every refusal after the attachment is
  taken; `reclaim` names a refused sweep (`Reclaimed::sweep_refused`) and releases everything else regardless
  (`release_all`); `abandon` for a volume that is gone (`ReclaimError::VolumeGone`); `Drop` releases the seam of a
  device never reclaimed.
- `crates/bridge-virtiofs/src/serve.rs`: `BridgeAccess::with_registry`; the loop's terminal step falls back to
  `abandon` through it.
- `crates/server/src/virtiofs.rs`: `ShardBridge::with_registry`; the refused-loop branch runs the same fallback and
  counts an incomplete sweep (`virtiofs.reclaim_incomplete`).

## Proof

`crates/bridge-virtiofs/tests/admission.rs`: `a_refused_admission_leaves_no_attachment_behind` (queues, memory and
publication each refused; the barrier closes nothing afterwards and a later admission's closes one; red with the
withdrawal mutated out) and `the_terminal_step_releases_everything_whatever_the_sweep_did` (a sweep refused by a bridge
over another volume, an abandoned device, a dropped device).

## Carried

A daemon-level test that destroys a volume under a live device and checks the registry afterwards; AUD-29-68 (a real
VMM binding).
