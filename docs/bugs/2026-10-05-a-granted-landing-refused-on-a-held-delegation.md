# A granted landing was refused while an NFSv4 client held a delegation in its volume (2026-10-05)

## Description

CI's Linux conformance lane failed hermeticity over the NFSv4.2 transport: `slates land --grant` exited 1 with
`Unpublished { reason: "the file is delegated to an NFSv4 client; its recall is under way" }`. It passed in a
local run, where the client had already returned its delegation. A server test reproduces it every time:
`a_granted_landing_of_a_delegated_file_waits_for_the_return_and_lands` failed with that refusal, and its client was
never called back.

## Root cause

- A landing's finish advances the volume's overlay past what reached the disk, which changes the landed files, so
  the recall gate refuses it while a client holds a delegation of one of them (RFC 8881 §10.2; A-79). The refusal is
  retryable under the same id, but `slates land` does not retry, and a granted landing records its reply at the
  finish.
- The recall the refusal asked for was never sent: verbs drain the gate's recall queue after they run, and a granted
  landing runs as a task after its verb returned, so nothing drained after its slices.

## Impact

A user's granted landing failed whenever an NFSv4 client (the Linux kernel's, with A-78/A-80's delegations) still
held a delegation of a file in the volume.

## Exact edits

- `crates/vfs/src/recall_gate.rs`: `recall_volume(prefix)` asks for the recall of every delegation held on a file of
  one volume, without counting a refused change.
- `crates/server/src/landing.rs`: the granted landing drains after every slice, and before its finish recalls the
  volume's delegations and waits for their return on the gate (`recall_before_finish`), keeping its target lease
  alive; a delegation not returned within a lease is revoked by the drain at the deadline (§10.4.5), and the finish
  runs. Counted `landing.held_for_recall`.
- Tests (`crates/server/tests/nfs_mount.rs`): the return releases the landing (red first: `Unpublished`, no recall),
  and an unreturned delegation is revoked after one lease and the landing then lands.
