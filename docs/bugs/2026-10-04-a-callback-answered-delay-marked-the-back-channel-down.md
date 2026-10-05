# A callback answered NFS4ERR_DELAY marked the back channel down

Date: 2026-10-04. Scope: the NFSv4.1 back channel (§4.6 A-77) and read delegations (A-78, A-79).

## Symptom

In the cross-protocol consistency run (`e2e-deleg-consistency.sh`, scratch: a Docker container reads a settled file
over NFSv4.2 while the macOS host writes it over NFSv3), three of three runs reported `nfs4.callback.down: 1` and no
`nfs4.delegation.granted`. With no delegation there was nothing to recall, so the run proved nothing about recalls.
An earlier run of the same script had granted and recalled.

## Root cause

A temporary diagnostic on the probe's answer (removed afterwards) printed the reply's results. The two outcomes:

- up: `CB_COMPOUND` status `0`, `CB_SEQUENCE` status `0`, the full `CB_SEQUENCE4resok`;
- down: `[0, 0, 39, 24, …, 0, 0, 0, 11, 0, 0, 39, 24]`. That is status `0x2718` = 10008 = `NFS4ERR_DELAY`, for both
  the compound and its one `CB_SEQUENCE`.

The Linux client answers `CB_SEQUENCE` with `NFS4ERR_DELAY` while it is still setting up its session; in a fresh run
the probe races that. RFC 8881 §15.1.1.3 defines `NFS4ERR_DELAY` as "try again later", and §20.9.3 says a failed
`CB_SEQUENCE` leaves the slot unchanged. The daemon read any status but `NFS4_OK` as a dead back channel, so the
session was never offered delegations. The recall path had the same rule: a recall answered `NFS4ERR_DELAY` was
counted unanswered and lapsed into revocation a lease later.

## Impact

Read delegations were withheld from about half of new Linux sessions (two of four runs). On the recall side, a
delegation could be revoked that the client would have returned.

## Fix

- Failing test first: `a_probe_answered_delay_is_retried_on_the_same_slot_sequence` answers the probe
  `NFS4ERR_DELAY` and expects it sent again with the same sequence id. Without the fix it fails: no second probe
  arrives.
- `crates/server/src/callback.rs`: `call` sends the same arguments again (same slot sequence, new xid) after
  `DELAY_RETRY_NS`, the daemon's poll interval, while the caller's deadline allows. The probe and `CB_RECALL` both go
  through it. The retries are counted as `nfs4.callback.delayed`.
- After the fix: four of four runs grant, recall and answer (BENCHMARKS, "Delegations against the kernels").

## Siblings

The only callback senders are the probe and `CB_RECALL`, and both use `call`. No other caller reads a callback
status.
