# A conformance session's macOS mount outlived its session, a hard mount of a daemon that was gone

**Date:** 2026-11-18. **Area:** `xtask/src/conformance/slates.rs` (`Mount`'s drop, `MountMethod::Slates`), the
gated test `a_live_shipped_volume_makes_a_writer_wait_for_stopped_candidates_rather_than_fail`
(`xtask/src/conformance/live_fleet.rs`), and two examples with the same guard (`crates/cli/examples/slates_mount.rs`,
`crates/cli/examples/vfs_tails.rs`); design §4.6 and A-229. **Found by:** g076a's rerun of the live-shipped
conformance pair on the Mac Studio d1323898 (kiln run r137, main 7993026), before adding the stopped-candidates test
to CI's conformance lane.

## Description

The gated test passed: with both candidates of a live-shipped volume stopped, a size change through the mount waited
149.3 s and succeeded. But after it the mount table still listed `slates:/livewait-vol` on the test's mount point,
with the volume's fleet killed. The mount had outlived its session and its daemon.

## Root cause

The test opens a file on the mount and drops its session (mount, then anchors) while the file is still open, as a
suite killed mid-run leaves its files open. The harness's `Mount` drop unmade a `slates mount` on macOS with a plain
`umount`, which macOS refuses (`EBUSY`) while a file on the mount is open. The harness then killed the anchors, so
the mount stayed, served by nothing.

That was harmless while the macOS mount was `soft,intr`: its client gave up on a dead server. A-229 (2026-11-06)
made the mount `hard,intr`, so a writer waits under backpressure, and changed the CLI tests' guards to force the
unmount when the plain one is refused (`MountPoint` in `crates/cli/tests/cli.rs`, `KernelMount` in
`crates/cli/tests/live_fleet.rs`). The conformance harness's guard, and the two examples' guards, were not changed.

## Impact

A hard mount whose server is gone blocks every process that touches it until it is signalled or the mount is forced
off. The harness makes its mount points in the build output (`target/slates-mount-<pid>-<n>`, A-50). On CI that
includes the cache's post-run walk of `target/`, so a leftover mount there could hang the job. On a developer's Mac
it could hang `find`, Spotlight, a backup, or the next `cargo clean`. Only the gated test left one in practice, since
every suite closes its files before its session drops. A suite killed mid-run, or a `vfs_tails` run that panics with
its spinners' files open, would have left one too.

## Exact edits

- `xtask/src/conformance/slates.rs`: `Mount`'s drop for `MountMethod::Slates` runs a plain `umount` and, if that is
  refused, `umount -f`, as the CLI tests' guards do. `is_mounted` is `pub(super)` so the test can check the mount
  table.
- `xtask/src/conformance/live_fleet.rs`: the gated test drops its session with the file still open, then requires
  the mount point to be gone from the mount table. It was red twice on the Studio at 196770c32 (kiln r170 and r202,
  "the session's mount … outlived its session"; the writer itself waited 149.3 s and succeeded both times). It was
  green at a06d3b375 (r213), with nothing left mounted.
- `crates/cli/examples/slates_mount.rs` (`Mounted`) and `crates/cli/examples/vfs_tails.rs` (`Mounted`, and the panic
  hook a release build's abort runs instead of the drop): the same plain-then-forced unmount. These examples have no
  tests. They were checked by building them, and their guard is the tested one.

Swept: every `umount` in the workspace's tests, examples and harness. The CLI tests' guards already force. The Linux
FUSE guards use `fusermount3 -u -z` (lazy), and the Linux NFS adapter uses `sudo -n umount -l` (lazy), so neither can
be refused busy.

One sibling of another kind is reported, not changed here: slates-api's `unmount` (`crates/api/src/mount.rs`) is a
plain `umount` on macOS. It answers a busy mount with a typed `MountFault::Unmount`, which is a refusal the caller
sees, not a mount silently left behind. But its `unmount_lazy` is refused `Unsupported` on macOS, so a host whose ended
agent left a process holding a file on the mount has no way through the library to remove it. macOS's forced unmount
(`umount -f`) is that way. This is reported to the library's owner (g262).
