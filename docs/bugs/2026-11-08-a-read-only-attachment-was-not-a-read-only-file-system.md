# A read-only attachment was not a read-only file system

**Found** 2026-10-24 by the Shards stream's read-only test (`a_read_only_stream_refuses_every_change`), recorded in
GAPS ("found by the new tests"). Fixed 2026-11-08 (slabs).

**Description.** Through a read-only attachment (one that may read but not write) or a snapshot's view, every change
was refused and nothing landed, but not as a read-only file system refuses it. On every FUSE transport (the shared
bridge: a guest's virtio-fs, the vhost route, a Shards stream) an open for writing was granted and each write through
it refused `EPERM`; NFS answered `PERM`; WinFsp answered `STATUS_ACCESS_DENIED` for what reached it. POSIX's read-only
file system answers `EROFS`, at the open for an open for writing. A client's message and its retry logic follow the
errno: `EPERM` reads as "you may not", `EROFS` as "nothing may".

**Root cause.** `VolumeBridge::authorize_write` (`crates/bridge-core/src/volume_bridge.rs`) answered `NotPermitted` for
a context without write rights and for a snapshot's view alike, and `VolumeBridge::open` ignored its flags, so a
write-mode open was granted and refused only at its first write.

**Impact.** No data was at risk: every change was refused before any effect. A client saw a permission error for a
read-only mount, and an application that opens read-write and falls back to read-only on `EROFS` failed instead.

**The edits.**
- `crates/vfs/src/error.rs`: `VfsError::ReadOnly` (`EROFS`).
- `crates/bridge-core/src/volume_bridge.rs`: `authorize_write` answers `ReadOnly` for a context that may read but not
  write and for a snapshot's view, and `NotPermitted` for one with no rights; `open` checks a write-mode open (`O_WRONLY`,
  `O_RDWR`, `O_TRUNC`, the FUSE protocol's flags) against it before any handle (`opens_for_writing`).
- The edges: `crates/bridge-fuse/src/bridge.rs` (`EROFS`, and the DAX refusals `Refused::ReadOnly`,
  `Failure::ReadOnly`), `crates/bridge-nfs/src/procedures.rs` (`NFS3ERR_ROFS`, and NFSv4's `NFS4ERR_ROFS` through
  `Nfsstat4::of_v3`), `crates/bridge-winfsp/src/lib.rs` (`STATUS_MEDIA_WRITE_PROTECTED`), `crates/server/src/error.rs`
  (the wire's `ReadOnlyVolume`).
- Tests, each red before the fix with the old answer (1, `EPERM`/`PERM`): `a_read_only_stream_refuses_every_change`
  and the Shards DAX writable ask (`crates/server/tests/shards.rs`), the snapshot device's CREATE
  (`crates/server/tests/virtiofs.rs`), the read-only attachment's writable SETUPMAPPING
  (`crates/bridge-virtiofs/tests/device.rs`), the read-only export's WRITE (`crates/bridge-nfs/tests/procedures.rs`),
  and the WinFsp status table (`crates/bridge-winfsp/src/lib.rs`); and a read-only WinFsp drive's changes answered
  `ERROR_WRITE_PROTECT` (`crates/cli/tests/winfsp.rs`): Windows' own read-only volume flag refused the overwrite, the
  create and the directory, but not the delete (below).

**Found by the WinFsp test on CI** (diag-slabs-35, run 42438454720, both runners): through a read-only drive the
overwrite, the create and the directory were refused `ERROR_WRITE_PROTECT`, but `DeleteFile` **succeeded**. Windows
deletes by a disposition and the cleanup that follows, and WinFsp's `CanDelete` (`op_can_delete`,
`crates/bridge-winfsp/src/host.rs`) checked only a directory's emptiness, never the attachment's rights; the cleanup's
unlink was then refused with nowhere to report it. The bridge now answers `authorize_change(cx)` (a new `Bridge`
method, `authorize_write` without a change) and `CanDelete` asks it first, so the delete is refused
`STATUS_MEDIA_WRITE_PROTECTED` before Windows marks the file.

**Unchanged.** An attachment with no rights at all is still refused for permission (`crates/server/tests/virtiofs.rs`'s
unenrolled guest, `EPERM`). Ownership refusals (`chmod`, `chown`, a sticky directory) stay `EPERM`/`NFS3ERR_PERM`.
