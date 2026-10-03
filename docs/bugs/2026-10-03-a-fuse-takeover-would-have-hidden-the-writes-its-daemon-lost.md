# fuse: a mount taken over across a daemon's death would have hidden the writes that daemon acknowledged and lost

**Date:** 2026-10-03. **Design:** §4.6 "Linux"; A-61; D-18; T-3.5. **Found by:** the in-flight takeover test
(`crates/cli/tests/cli.rs` `a_write_in_flight_at_the_daemons_kill_is_resent_and_no_loss_is_silent`), before the
takeover landed. Nothing shipped with it.

## Description

With the anchor holding a FUSE mount's device across a daemon's death (A-61), the writer runs on. A writer
appending pages through the kill had every write acknowledged, and the file's length was right. But page 99 came
back wrong, and in later runs pages 551, 558, 584 and 615. A later `fsync` would have succeeded over the hole.

## Root cause

A plain FUSE write is acknowledged before the shard's recovery image is published. It is made stable by the
`flush` or `fsync` that follows, like an NFS `UNSTABLE` write (D-18, `slates_bridge_fuse::bridge::needs_barrier`).
That contract was sound while a daemon's death ended its mounts: the writer saw the loss at once
(`ECONNABORTED`). A takeover removes that signal. NFS clients recover the same loss by resending uncommitted
writes when the server's write verifier changes; FUSE has no such protocol.

## Impact

None shipped. Had the takeover landed alone, a FUSE writer's acknowledged-but-unpublished writes would have
vanished at a daemon restart, and its next `fsync` would have reported success: silent data loss.

## Exact edits

- **The log.** `crates/server/src/dirty_log.rs` is a page of the shard's slice of the anchor content object,
  which outlives the daemon. The first write to a file after a publication names its inode there (one
  shared-memory store). A publication that captured every volume empties it. The log is bounded at a page, past
  which it reports every file.
- **The recovery read.** `crates/server/src/daemon.rs` `content_slice` carves the page from the slice and reads
  the previous daemon's log before anything publishes.
- **Marking.** `crates/server/src/fuse.rs` `log_write` marks a successful write before its reply. A write
  applied and never answered is resent, not counted lost.
- **Reporting.** `crates/bridge-core/src/volume_bridge.rs` `LostWrites`, with the trait's new `fsync`. For a
  handle the dead daemon issued on a named file, every `flush` answers `EIO` without consuming it, and the first
  `fsync` answers it and consumes it. A flush comes with every close of any copy of a descriptor, so a spawned
  child's exec-time close must not take the writer's report. Measured: in the first version, it did.
- **The contract.** The design's T-3.5 is restated to match D-18: barrier-stable writes are present, and a lost
  write is never silent.

## Proof

- The in-flight test failed before the log, with "page 615 is wrong and no fsync reported a loss". After the
  report was made non-consuming on flush, it passes 3 of 3, each run with a resent request served and the loss
  reported.
- `crates/bridge-core/tests/volume_bridge.rs` `a_lost_write_is_reported_once_to_each_handle_that_predates_the_takeover`.
- `crates/server/src/dirty_log.rs` tests cover the marks and clear, the overflow, and an uncounted entry or an
  impossible count.

## Sibling sweep

Other transports:
- **NFS:** survives restarts by its write verifier (the client resends uncommitted writes).
- **virtio-fs guest devices and WinFsp:** end with the daemon, so their writers see the loss.

Only a held FUSE mount needed the log. Making every acknowledged write survive, rather than reporting its loss,
needs the owed incremental publication.
