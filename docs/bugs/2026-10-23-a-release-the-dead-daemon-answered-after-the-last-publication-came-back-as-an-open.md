# A release the dead daemon answered after the last publication came back as an open

**Status:** fixed 2026-10-23 (A-154, approved by Ada in plan mode). Root cause confirmed by logs on the box (Linux
7.2.9). Found by A-152's own end-to-end test, under load.

**Severity:** a leak bounded by the mount's lifetime, the case A-152 left. The kernel never resends a `RELEASE` its
daemon answered. A daemon that answered one, and died before its next publication, leaves the open counted in the image.
The successor restores that open, and nothing ever gives it back. So the file's open reference stays until the mount
ends: an unlinked file is never reclaimed, and its bytes stay charged. Nothing is reclaimed early, and no bytes are
wrong.

## Reproduction

`crates/cli/tests/cli.rs` `the_handles_a_dead_daemon_issued_give_back_their_references_at_release` failed once in about
five full CLI-suite runs on the box, at load average 74 to 93, and never alone (12 of 12 passed at load 100). The
failure was `referenced_bytes` still 1,048,576 after the 10 s wait.

With temporary `RELDBG` lines in `VolumeBridge::release` and `Volume::restore_references`, and the test printing its two
daemons' pids (run 3 of the full suite, load average 93):

```
RELDBG pid=2115171 release ino=3659174697238530 fh=0x0 known=true
RELDBG pid=2116277 restore owner=281474976710657 ino=3659174697238530 refs=4 opens=3
RELDBG pid=2116277 release ino=3659174697238530 fh=0x100000000 known=false
RELDBG pid=2116277 restored open given back for ino=3659174697238530: true
RELDBG pid=2116277 release ino=3659174697238530 fh=0x200000000 known=false
RELDBG pid=2116277 restored open given back for ino=3659174697238530: true
```

- **The dead daemon (2115171)** released the `CREATE` handle `0x0` (`std::fs::write`'s, closed before the unlink).
- **The successor (2116277)** restored three opens. So the unlink's publication had counted that handle as open: its
  `RELEASE` reached the dead daemon after the barrier. The kernel sends `RELEASE` in the background, and under load it
  queued behind the 1 MiB file's write-back while the `UNLINK` went first.
- **The two releases** of the handles still open gave back two of the three restored opens. The third outlived every
  handle.

**Deterministic, at the seam:** `crates/bridge-core/tests/takeover_release.rs`
`a_handle_released_after_the_last_publication_leaves_no_open_behind` releases the first handle through the dead daemon
after the image, takes over from the image, and releases the other two. The file is never reclaimed (red).

## Root cause

A-152 restores the opens an image counted. Its analysis covered a handle opened after the last publication: nothing in
the image, so nothing to give back. It missed the other edge: a handle released after the last publication. The image
still counts its open, and the release that would give it back was answered by the dead daemon, so the kernel never
sends it again. That release's effect was durable only at the next publication. D-18 asks for an effect a client
relies on across a restart to be durable before the reply, and the kernel relies on a `RELEASE` having taken effect.

## Siblings

- **A `FORGET` processed after the last publication** leaves its lookup references in the image the same way, and the
  kernel never resends a `FORGET` either. `FORGET` has no reply, so no barrier can precede one. It is the same bounded
  leak, recorded with A-61's stated limits in GAPS, not fixed here.

## The fix (A-154)

The invariant: after every reply, each recorded attachment's live opens of a file are at least the opens the committed
publication counted. A successor restores the committed count, and the kernel's later releases consume restored opens
while any are left, so the opens nothing will give back number max(0, committed − live).
- **`Volume::committed_opens`** holds the counts the last committed publication, or the recovered image, carries. It
  is updated at the commit (`mark_published` → `commit_opens`: the changed reference records after a delta, every
  count after a volume's first, whole publication), at recovery, and at an attachment's sweep.
- **`close_for`** marks the volume `releases_unpublished` when it leaves fewer live opens than committed.
- **`Volume::awaits_publication`** (A-150's `unreserved`, renamed) answers true for that too. So a transport's reply
  waits for the owner's barrier (FUSE, NFS, virtio-fs; `Bridge::awaits_publication`).
- **A crash before that reply** leaves the `RELEASE` unanswered: the kernel resends it, and it consumes the restored
  open.

For a regular file the kernel sends `RELEASE` after `close()` has returned, so the wait costs the daemon a publication
and the application nothing. An open and release both since the last publication leave the count where it was, and
wait for nothing.

## Exact edits

- `crates/vfs/src/volume.rs`:
  - `committed_opens` and `releases_unpublished`;
  - `close_for` sets the flag;
  - `restore_references` and `sweep_attachment` keep the committed counts;
  - `unreserved` became `awaits_publication`.
- `crates/vfs/src/delta.rs`: `mark_published` calls `commit_opens`.
- The renamed hook:
  - `crates/bridge-core/src/lib.rs`, `volume_bridge.rs` and `scoped.rs` (`Bridge::awaits_publication`);
  - `crates/bridge-virtiofs/src/device.rs`;
  - `crates/server/src/verbs.rs`, `fuse.rs` and `nfs.rs`;
  - `crates/bridge-fuse/tests/dispatch.rs` and `tests/common/mod.rs`;
  - `crates/vfs/tests/counter_reserve.rs`.
- Tests: `crates/bridge-core/tests/takeover_release.rs`:
  - `a_handle_released_after_the_last_publication_leaves_no_open_behind` (red, then green);
  - `a_release_of_an_open_no_publication_counted_waits_for_nothing`.
- Docs: `SLATES_DESIGN.md` A-61 and A-154, GAPS.

## Verification

| | before | after |
|---|---|---|
| the seam test | red: the volume awaited no publication after the release | green; the file reclaimed at the forget |
| the A-152 end-to-end test, full CLI suite on the box | about 1 in 5 runs failed (load average 74 to 93) | 5 of 5 passed (load average 107 to 128) |
| an open and release both since the last publication | (no wait) | no wait |

- **macOS:**
  - the vfs, bridge, land and base suites: 945 of 945;
  - the server's `recovery` (39), `nfs_mount` (40), `virtiofs` (8) and `attach_forms` (9);
  - workspace clippy `-D warnings`, the doc gate, `cargo xtask check` and fmt.
- **The box (Linux 7.2.9, load average 107 to 128):**
  - workspace clippy `-D warnings`;
  - the vfs and bridge suites (697);
  - the server's `fuse_mount` (3), `recovery` (41), `virtiofs` (18) and `nfs_mount` (40);
  - the guest tests (3);
  - five full CLI-suite runs, 279 of 280. The one failure was the busy unmount already queued for diagnosis
    (`a_fuse_mount_survives_its_daemons_kill_with_its_open_files_usable`).
