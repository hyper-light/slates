# anchor: the CLI anchor sized the content object at half of what the daemon's recovery layout assumes

**Date:** 2026-10-03. **Design:** §4.8; `docs/wip/recovery.md` slice 7. **Found by:** adding the write log
(A-63), which needed the anchor and the daemon to agree on each shard's slice.

## Description

The content object holds each shard's double-buffered recovery image, two slots per shard. A standalone daemon sized
it `reserve_per_shard × 2 × partitions` (`daemon.rs` `content_bytes`, documented as "two reserve-sized slots per
shard"). The CLI anchor, which creates the object for every supervised daemon, sized it
`reserve_per_shard × partitions`. So under a real anchor each image slot was half a reserve. A shard whose image
outgrew half its reserve would have its publication refused (`NoSpace`) while the daemon's own layout promised room
for a whole reserve.

## Root cause

The size was defined twice, in two crates, and the two definitions drifted.

## Impact

A shard holding more than about half its reserve in recovery-image bytes would fail every barrier under an anchor:
a refused publish answers `EIO` (NFS `NFS3ERR_IO`, a FUSE barrier) rather than a stable acknowledgement. No reported
failure: the test volumes are far smaller than half a reserve.

## Exact edits

- `crates/server/src/config.rs` `DaemonConfig::content_bytes`: the one definition (two reserve slots and the write
  log per shard, times the partitions).
- `crates/cli/src/anchor.rs` and `crates/server/src/daemon.rs` both use it.
- The duplicate `content_bytes` and `PUBLISH_SLOTS` in `daemon.rs` are removed.

## Proof

There is one definition now, so the two cannot drift again. Every daemon restart test runs under the object it
sizes: recovery 14, daemon 18, and the CLI's anchored restarts.

## Sibling sweep

No other size is shared between the anchor and the daemon. The segment geometry is passed whole (`Geometry`).
