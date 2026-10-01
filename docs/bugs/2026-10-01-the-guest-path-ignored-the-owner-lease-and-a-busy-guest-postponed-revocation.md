# The guest path ignored the owner lease, and a busy guest postponed revocation (AUD-29-83, AUD-29-87)

**Date:** 2026-10-01. **Audit:** AUD-29-83 (P1), AUD-29-87 (P1). **Design:** §4.8 "Leases and reads" (AUD-08),
§4.6 (virtio-fs, FUSE), R8, StaleNeverCommits / ReadSafety.

## Description

- **Lease (83).** The daemon's guest device (`crates/server/src/virtiofs.rs` `ShardBridge`) checked only that the
  volume's slot existed before each pass. An owner whose lease had lapsed — cut off, paused past the bound, or
  superseded — kept serving its guest reads and writes from its live tree while the NFS mount of the same volume
  answered `NFS3ERR_JUKEBOX`. The daemon's FUSE owner turn (`crates/server/src/fuse.rs`) had the same hole: a
  sibling found while fixing this.
- **Revocation (87).** `serve_until_idle` yielded between passes but checked the revoke flag only before its
  doorbell wait, so a guest that kept its rings full kept being served after a revoke was asked for.

## Root cause

The live-tree gate lived inline in the NFS serve path, and the guest and FUSE paths were written later against the
slot alone. The serve loop's revoke check sat in the outer round, not at the pass boundary.

## Exact edits

- `crates/server/src/verbs.rs`: `live_tree_fenced(state, volume)` — consensus ready and `lease_refusal` — the one
  gate the mounted transports share.
- `crates/bridge-virtiofs/src/serve.rs`: `BridgeAccess::fenced`; `serve_until_idle` checks the revoke request and
  the fence before every pass; while fenced it sleeps the owner's interval and drains the doorbell (a hangup ends
  the loop); `EndReason::WaitLost`; `ServeEnd::fenced_waits`.
- `crates/server/src/virtiofs.rs`: `ShardBridge::fenced` answers with the shared gate and `HEARTBEAT_NS`.
- `crates/server/src/fuse.rs`: the turn reads no request while fenced (`Turned::Fenced`), paced one heartbeat.
- `crates/server/tests/common/guest.rs`: the guest harness moved out of `tests/virtiofs.rs`, with `start_guest`.

## Proof

- `crates/server/tests/fleet.rs` `an_isolated_owner_holds_its_guests_requests_rather_than_serve_them`: f = 1, three
  nodes; the guest's read answered while A's lease held; A isolated on the probe plane until its lease lapsed; a fresh
  read and a CREATE unanswered for five heartbeats; `fenced_waits > 0`; the lease refusals counted; the hangup ended
  the loop. With the fence mutated out: `answered = Ok(true)`.
- `crates/bridge-virtiofs/tests/serve.rs` `a_fenced_owner_holds_the_guests_requests_until_its_lease_holds` and
  `a_revoke_requested_mid_service_ends_the_loop_at_the_next_pass` (one pass and 8 answers, where the unfixed loop ran
  two passes and answered 16).

## Carried

- The FUSE turn's fence has no test of its own yet: it needs a fleet with a Linux kernel mount. It is the same
  predicate the guest test exercises.
- A FUSE mount the kernel unmounts while its owner is fenced is seen when the fence lifts or at the daemon's stop.
