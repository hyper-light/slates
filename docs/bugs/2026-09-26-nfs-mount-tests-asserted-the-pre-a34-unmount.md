# Two daemon NFS tests asserted the pre-A-34 unmount rule

**Date:** 2026-09-26. **Found:** running `cargo test -p slates-server --test nfs_mount` locally while
adding the daemon's NFSv4 routing. The CI test lane for 97c6b94 and 16ea027 stopped at an earlier
failing binary (the fleet suite), so it never reached this one.

## Description

`a_snapshot_over_a_mounted_volume_reports_the_barrier_it_closed` and
`a_consumer_private_volume_is_served_over_nfs_only_through_its_attachment_capability` failed:

- the snapshot after `UMNT` still closed one attachment: `left: (ServerVisible, 1)`, expected
  `(Complete, 0)`;
- the read after `UMNT` still succeeded: `left: 0`, expected `13` (`NFS3ERR_ACCES`).

## Root cause

Both tests sent a `UMNT` whose path carried the capability (`/<name>@<attachment>.<token>`) and
expected it to end the mount's attachment. That was the rule before A-34. A-34 (97c6b94) made the
mount source `slates:/<name>`, with no capability, so the kernel's `UMNT` names only `/<name>`. It
proves nothing by itself. The daemon now ends a host mount's attachment only once the kernel's mount
table no longer lists the mount at the attachment's bound mount point (macOS). Off macOS a `UMNT` is
never proof. A `UMNT` presenting a capability is ignored. The daemon behaved as designed; the tests
asserted a superseded design.

## Impact

None on the product. Two tests that could not pass guarded nothing.

## Exact edits

`crates/server/tests/nfs_mount.rs`:

- `end_host_mount` binds the attachment to a mount point this test never creates, then sends the
  kernel's `UMNT /<name>`.
  - On macOS it waits (bounded by `CREDIT_WAIT`) for the attachment to end.
  - Elsewhere it asserts the attachment still stands, then detaches it.
- The lifetime test also asserts that a `UMNT` presenting the capability ends nothing.
- The snapshot test ends its mount through `end_host_mount`.
- Both docs state the A-34 rule.

No product code changed.
