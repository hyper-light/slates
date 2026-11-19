# A kept FUSE mount of a volume recovery could not rebuild answered ENOTCONN until unmounted by hand

Date: 2026-11-18. Area: `crates/server/src/verbs.rs` (recovery's reconciliation), `crates/server/src/fuse.rs` (a mount's
turn), `crates/bridge-fuse/src/channel.rs`. Design: §4.6 "Linux", §4.8 `NotRecovered`; A-61 (the held device), A-102
(slates never unmounts a mount its user did not release); amendment A-509. Goal 85. Found by reading (the GAPS entry
of 2026-11-08 on volumes recovery could not rebuild), proven here by its unit test.

## Description

A Linux FUSE mount survives a daemon restart: the anchor holds its device, and the successor serves it again (A-61).
When the successor's recovery cannot rebuild the mounted volume (no image, a base gone, the resources not there, a
takeover incomplete), the volume is listed with its cause and every verb on it answers `NotRecovered`. Its mount did
not: the kernel answered every call on it `ENOTCONN` ("Transport endpoint is not connected"), nothing named why, and
the mount stayed so until someone ran `fusermount3 -u` by hand. A later restart whose recovery did rebuild the volume
came back to no mount.

## Root cause

Recovery reconciles each rebuilt volume's attachments (`verbs::reconcile_lost`): a FUSE mount whose device the anchor
held is kept for `fuse::adopt_held`, any other is left for its user and its record ended. A volume recovery could not
rebuild returns early from that path (`rebuild_record` answers false, "which leaves it out of the reconciliation"), so
its mount was neither kept nor recorded as dead. The anchor's device stayed among the inherited ones nobody took and
was let go at the end of recovery, and the kernel's connection ended with the mount still in its table.

The serving side had the same gap. A mount's turn that finds no slot for its volume (`fuse::turn_with`) ends the mount,
as for a destroyed volume, so a kept mount of a not-recovered volume, had it been adopted, would have ended at its first
request.

## Impact

Every Linux FUSE mount (bare metal kiln agents' workspaces among them) whose volume a restart could not rebuild: the
agent's tools met `ENOTCONN` with no cause, the mount had to be unmounted by hand, and a transient cause (resources, a
base directory offline) cost the mount for good. Nothing was written to the disk beneath: the mount stayed in place.

## Exact edits (A-509)

- `crates/server/src/verbs.rs`: `keep_unrecovered_mounts`, run for each volume recovery could not rebuild. A FUSE mount
  whose device the anchor held, on a kernel that can resend, goes to the kept mounts (`adopt_fuse`), as a rebuilt
  volume's does. Any other is left in place for its user (`stale_fuse_mounts`, counted by `leave_stale`) and its record
  ended. The volume's other attachments stay with its record.
- `crates/server/src/fuse.rs`: a turn that finds no slot for a volume the shard records unrecovered serves the mount
  on (`refuse_unrecovered`): a request turn answers the waiting request `EIO` (`NOT_RECOVERED_ERRNO`, counted
  `fuse.unrecovered_refused`), a change turn has nothing to tell. The mount ends when the kernel disconnects.
- `crates/bridge-fuse/src/channel.rs`: `refuse_unserved` reads one request and answers it the errno without reaching any
  volume, a `FORGET` by nothing with its lookups taken off what the kernel holds, and marks the restored mount's takeover
  round told, since nothing of the volume is served.
- Docs: §4.6 "Linux" (a status), the amendment log (A-509), GAPS.

## Tests

`a_kept_mount_of_a_volume_recovery_could_not_rebuild_answers_each_request_eio` (`crates/server/src/fuse.rs`, the
server's unit tests, Linux): a mount over a socket pair standing in for `/dev/fuse`, of a volume the audit shard
records unrecovered; a `FORGET`, then a `LOOKUP`. It expects both turns served, one reply (the `LOOKUP`'s `EIO` under its
own `unique`) and two refusals counted. Red without the turn's not-recovered branch, green with it (adacachy, Linux,
debug). Command: `cargo test -p slates-server --lib a_kept_mount_of_a_volume`.

The flow over a real mount is main's to run: a volume left unrecovered across a restart, a read through its kept mount
answering `EIO`, and the mount served again once a restart rebuilds the volume.
