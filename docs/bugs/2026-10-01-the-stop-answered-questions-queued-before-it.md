# The stop answered questions queued before it (AUD-29-64 regression)

**Date:** 2026-10-01. **Audit:** AUD-29-64 (the daemon serves Linux FUSE). **Design:** §4.6 "Linux", §4.14
(observation delivery).

## Description

On Linux, `Daemon::stop` sent every shard a question, `fuse::unmount_all`, before shutting the runtime down. A
question waits in the shard's control queue behind whatever the shard is running and behind every question sent
before it. So a stop issued while a shard was busy did not terminate the questions already waiting there: it
waited for the shard, the waiting questions ran and answered, and only then did the shutdown run.
`crates/server/tests/observe.rs`
`a_stopped_target_terminates_its_pending_question_and_its_reused_slot_never_answers_a_stranger` failed on the
GitHub Ubuntu lane (run 36890745801): the pending question answered `Ok(11)` after 2.0016 s (the hold's span)
instead of `Terminated` or `ShardGone`. macOS was unaffected: the question is `#[cfg(target_os = "linux")]`.

## Root cause

`153ca63` (the daemon serves Linux FUSE) put the stop's unmount in the shard's work queue. That ordered the stop
behind the shard's work, against the contract the observe test states: a stop ends a pending question typed and
never leaves it waiting. A wedged shard would also have held the stop for the whole observe budget, shard by shard.

## Exact edits

- `crates/server/src/daemon.rs`: `stop_parts` no longer sends the unmount question. A drop guard, `EndMounts`, is
  held by each shard's serve loop. The loop is perpetual, so it ends only when the shutdown cancels every task on
  the shard; the guard's drop then runs `fuse::unmount_all` on the shard's own thread with its state still
  installed.
- `crates/server/src/fuse.rs`: `unmount_all`'s doc names its caller.

## Proof

Reproduced in `rust:1.98.0` as an ordinary user with io_uring allowed (`--security-opt seccomp=unconfined`): the
observe test failed with `Ok(11)` after 2.067 s; after the change the observe suite passed 6/6, and so did
`crates/server/tests/fuse_mount.rs` on a real kernel FUSE mount (`--device /dev/fuse --cap-add SYS_ADMIN`). With
the guard's unmount mutated out, `fuse_mount.rs:222` fails ("the daemon's stop unmounted"). The guard is what
unmounts.

## Siblings found (reported, not changed here)

- A shard fenced by a consensus-recovery failure refuses every state borrow (`StateAccess::Fenced`), so its FUSE
  mounts are not unmounted at the stop. The removed question had the same gap.
- A FUSE attachment's record survives a crash. Recovery reconciles out only SDK consumers' attachments
  (`verbs::reconcile_lost`), and a FUSE mount is a bridge consumer at a chosen path, the same record shape as an
  NFS host mount. The record cannot tell the restarted daemon that the device died with the process. Owed in the
  AUD-29-64 row.
