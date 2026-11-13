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

**Found by conformance's read-only pjdfstest run** (CI run 42498995934, through the kernel's own NFS clients): over
NFSv3 and NFSv4 `open(O_WRONLY)` and `open(O_RDWR)` of an existing file were granted, and v4's `open(O_RDONLY|O_TRUNC)`
answered `EACCES`. The bridge's tests had driven the procedures and never met the kernel's open. ACCESS
(`granted_access`, `crates/bridge-nfs/src/procedures.rs`) granted the write bits from the mode alone; the kernel decides a
v3 open from that reply, and v4's OPEN asked the same ACCESS; the truncation's SETATTR under the open's read state id
was refused by the state id's check before the attachment's. Now ACCESS grants no MODIFY, EXTEND or DELETE where the
bridge refuses a change (`change_refusal`); a v4 OPEN for writing asks the owner extension `CHANGE_ALLOWED` first and
answers its `NFS4ERR_ROFS`; and a change under a state id is refused ROFS before its state id is judged (`state_io`).
Over NFSv3 the open is then `EACCES`, the most a v3 server can say (A-269). The test,
`a_read_only_attachment_refuses_an_open_for_writing_through_the_kernels_nfs_clients`
(`crates/server/tests/nfs_read_only_kernel.rs`, Linux CI as root): red on main (diag-slabs-42, run 42517407649: v3
`[granted, granted, EROFS]`, v4 `[granted, granted, EACCES]`), green with the fix (diag-slabs-43, run 42517769901). The
bridge's `a_write_through_a_read_only_export_is_refused` now holds ACCESS and `CHANGE_ALLOWED`, red with the mask taken
out.

**Unchanged.** An attachment with no rights at all is still refused for permission (`crates/server/tests/virtiofs.rs`'s
unenrolled guest, `EPERM`). Ownership refusals (`chmod`, `chown`, a sticky directory) stay `EPERM`/`NFS3ERR_PERM`.
