# A FUSE mount outlived its daemon's crash, or a fenced shard's stop (AUD-29-64 siblings)

**Date:** 2026-10-01. **Area:** `slates-server` (`fuse.rs`, `verbs.rs` recovery, `daemon.rs` `EndMounts`),
`slates-db` (`AttachForm`). **Found by:** the sibling sweep of
`2026-10-01-the-stop-answered-questions-queued-before-it.md`. **Design:** §4.6 "Linux", §4.8 recovery.

## Description

1. **A crash.** A FUSE mount's device is the daemon process's own, so it dies with the process. Afterwards:
   - the attachment's record stayed in the catalog, so `status` counted a dead attachment after the restart;
   - the mount stayed in the kernel's table and answered `ENOTCONN` until someone unmounted it.

   Recovery reconciled out only SDK attachments. A FUSE record had the same shape as an NFS host mount's (a
   bridge consumer at a chosen path), and an NFS mount does reconnect to the restarted daemon.
2. **A fenced stop.** A shard fenced by a consensus-recovery failure refuses every ordinary state borrow. The
   stop's unmount (`EndMounts`) borrowed that way, so a fenced shard's mounts outlived the daemon.

## Root cause

1. The record did not say which transport made it.
2. A shutdown release of kernel resources was gated by the same fence that guards record writes.

## Exact edits

- `crates/db/src/catalog.rs`: `AttachForm::FuseMount { path }` is appended (the enum is append-only), with
  `AttachForm::mount_point()` for every reader of a mounted form.
- `crates/server/src/verbs.rs`: a FUSE attach records `FuseMount`; `mounts_of` reads `mount_point()`.
  `reconcile_lost` ends a `FuseMount` record at recovery and queues its mount point in
  `ShardState::stale_fuse_mounts`.
- `crates/server/src/fuse.rs`:
  - `unmount_stale` runs once the shard is installed. It unmounts each queued point only if the kernel's table
    shows `fuse.slates` with the record's attachment as its source, so a later mount at the path is never
    touched.
  - `unmount_all` is split into `unmount_points` (no records) and the record ends.
- `crates/server/src/oci.rs`: the container bind's parent is matched by `mount_point()`.
- `crates/server/src/state.rs`: `with_state_at_shutdown`, a borrow that ignores the fence and through which
  nothing writes a record.
- `crates/server/src/daemon.rs`:
  - `EndMounts` falls back to that borrow and `unmount_points`;
  - `Daemon::inject_consensus_failure` is test support.

## Proof

Both tests ran in `rust:1.98.0` as an ordinary user, with `/dev/fuse`, io_uring allowed.

- `crates/cli/tests/cli.rs` `a_fuse_mount_whose_daemon_was_killed_is_ended_by_the_restarted_daemon`:
  `slates mount` over FUSE, `kill -9` the daemon, and the anchor restarts it. Expect the attachment count to
  reach 0 and the mount to be gone. It passes. With the recovery step mutated out it fails at `cli.rs:3181`
  ("the restarted daemon ended the dead mount's attachment").
- `crates/server/tests/fuse_mount.rs` `a_fenced_shards_fuse_mount_ends_with_the_daemon`: a one-shard daemon,
  FUSE attached, the control shard fenced, then stopped. Expect the mount gone. It passes. With the fallback
  mutated out it fails at `fuse_mount.rs:263`.
- `slates-db` passes; `slates-server --lib` passes 149/149; attach_forms passes 5/5.
- Observed once, not explained: one `--lib` run under load average 37 missed a 10 s observe deadline in
  `a_takeover_keeps_the_volumes_grants_and_its_locked_policy`. That test passed 3/3 alone, and the full suite
  passed 149/149 on the next run. It is not attributed to this change; a code path linking them was not
  found.
