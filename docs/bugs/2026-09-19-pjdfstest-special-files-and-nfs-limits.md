# Pjdfstest: special-file omissions and two NFSv3 limits

Date: 2026-09-19 (execution records use UTC 2026-09-20).
Design: §4.5 namespace, §4.6 mounted equivalence, §4.8 recovery, §4.16 merge.
Status: FIFO/socket namespace, recovery and Linux bridge support implemented; full merge-service
integration and the remaining conformance review are open. No failures have been allowlisted.

## Reproduction

The authorized disposable Linux container reproduces CI's exact result at the pinned
`pjd/pjdfstest@85a8aea9e685999ef0540392fd80535f873d7ff7`: 238 files, 8,798 cases,
5,175 passes, 3,595 failures, 28 TODO cases. The daemon stays responsive. The local run
took 173,791 ms; the separate uninstrumented fsstress rerun passes all 2,000 operations
in 93,309 ms. These are separate defects.

Command: `/build/debug/xtask conformance run --suite pjdfstest --records
/scratch/slates-records --scratch /scratch/slates-scratch --keep`.
Environment: Rust 1.98.0, Debian 13, aarch64 Linux 6.12.76-linuxkit, four CPUs,
4 GiB, real kernel NFS adapter; ordinary-user daemon, root test driver for uid changes.
Logs and unchanged upstream sources:
`/private/tmp/slates-mounted-evidence/conformance-retry.tar.gz`;
`/private/tmp/slates-ci-conformance-retry.log`.

## Findings

`Export::mknod_unsupported` unconditionally returns `NFS3ERR_NOTSUPP`; the VFS stores
only regular files, directories and symbolic links. FIFO creation, pathname sockets and
block/character device creation therefore fail before their ownership, links, times,
permissions and rename cases can run. Linux exposes the refusal as errno 524. Subsequent
assertions often fail with ENOENT. Some also get a successful rename/rmdir because the
entry intended to make the destination nonempty never existed. Those successes do not
prove a rename or rmdir bug.

Examples reviewed against the pinned scripts:

- `open/06.t:65–111`: the FIFO permission section fails; the regular-file and directory
  permission sections pass. The first FIFO creation refuses; the remaining calls address
  that absent inode.
- `rename/20.t:9–25`: refused FIFO creation leaves an empty destination directory, so
  case 10 successfully replaces it. Later setup/cleanup then uses paths already moved or absent.
- `rmdir/06.t:8–23`: the same missing FIFO permits rmdir; later special-file and symlink
  setup fails because their parent was already removed.
- `unlink/14.t:4`: open/unlink/fstat reports one link. Linux's NFS client renames an open
  file to a hidden name and unlinks it only after close. This is in the client's
  [nfs_sillyrename implementation](https://github.com/torvalds/linux/blob/v6.12/fs/nfs/unlink.c).
- `utimensat/09.t:5`: 4,294,967,296 seconds comes back as 4,294,967,295. NFSv3 has an
  unsigned 32-bit seconds field, so the requested instant is outside its wire range.
  [RFC 1813, nfstime3](https://www.rfc-editor.org/rfc/rfc1813#page-21).

The latter two need a narrow transport-specific conformance expectation, not invented
server state. The special-file failures require either implementing the missing namespace
semantics or explicitly reviewing the deliberately limited contract. A bulk-generated
expected-failure list cannot establish that every dependent assertion is understood.
No list has been expanded in this repair.

## Correct implementation boundary for special files

Accepted scope (A-26, 2026-09-19): FIFO/socket namespace metadata and local transient
communication; devices remain refused. Landing these names refuses before its first
write. Host special files remain excluded from the live base. This supersedes the broader
initial device proposal: device metadata needs its own decision.

Special-file identity belongs in the shared VFS, not an NFS-only side table.
Represent the FIFO/socket kind on the inode, with no device numbers or content. Use the existing
bounded inode/name accounting, owner and mode checks, hard links, rename/unlink, snapshots
and clone ownership. Kernel clients own live pipe/socket behavior; the server must never
open a host device to service such an inode.

The same metadata must survive recovery, canonical archive/merge representations and
all bridge attribute encodings. A landing must apply a supported node type or refuse it
before writing; it must not silently drop it or request daemon privileges. This spans
§4.5, §4.6, §4.8, §4.15 and §4.16 and cannot be fixed correctly by making MKNOD return OK
for a regular file. Start with an NFS creation/lookup/link/rename oracle, a snapshot and
restart oracle, and malformed type/device input. Rerun the full unchanged mounted suite.

## Implemented piece and verification (2026-09-20)

The shared inode and directory-entry models now carry FIFO/socket kinds. Namespace operations,
copy-on-write snapshots, clones and recovery retain kind, attributes and hard-link identity.
Regular-file reads, writes (including empty writes), truncate, edit and open refuse IPC names.
Recovery image format 4 refuses a special inode carrying content or a nonzero size. The VFS
ops document version 2 carries a typed IPC kind, attributes and canonical hard-link references,
with no content hunks. Its existing deterministic golden vector was updated for the format change.

NFS MKNOD and Linux FUSE MKNOD create these kinds. Attributes and directory listings report the
actual type. Device MKNOD remains refused; NFS uses BADTYPE, as RFC 1813 §3.3.11 requires when a
server supports some special types but not the requested one. Truncated-request tests exercise
every cut of each new parser and verify that no name appears. FSKit and WinFsp return explicit
unsupported-operation errors; they do not project these nodes as ordinary files.

Archives carry IPC metadata without reading a stream. The server's archive reconstruction also
needed a sibling fix: it previously created a distinct inode for every path. The new regression
failed with inode 281474976710658 versus 281474976710659 for two names of one FIFO. Reconstruction
now groups names by the archived inode, validates repeated attributes and bytes, and applies
owners/times after all links exist. Conflicting groups or IPC payload bytes refuse before the
replacement tree is mutated. Archive metadata still has its existing timestamp contract (mtime
and ctime; atime/birth time are not fields of this archive format).

A landing containing an IPC name refuses during manifest construction before any host write,
including when an ordinary file precedes the IPC name. Existing host special files are still
excluded from the live base. No device activation, daemon privilege, live-stream replication or
host-endpoint import was added.

Commands and evidence:

- `cargo test -p slates-vfs --test special`: five tests pass on macOS, including canonical
  metadata derivation, hard links, snapshot/clone/recovery, archive export, malformed recovery
  and regular-file I/O refusal. `/private/tmp/slates-special-vfs-current.log`.
- Disposable Linux: `cargo test --offline -p slates-server --lib
  archive_restore_preserves_ipc_hardlinks`, followed by `cargo test --offline -p slates-vfs
  -p slates-bridge-core -p slates-bridge-nfs -p slates-bridge-fuse -p slates-bridge-fskit
  -p slates-bridge-winfsp -p slates-land`: 391 passes, zero failures, two ignored, before the
  final parser tests and stronger active-endpoint clone test were added.
  `/private/tmp/slates-special-linux-current.log`.
- The real FUSE mount test runs as the ordinary `tester` user. It queues bytes in both a FIFO
  and a connected UNIX socket, keeps the listener alive, then snapshots and mounts the clone.
  The clone has neither queued pipe bytes nor a listener; the original still reads both
  payloads. `crates/bridge-fuse/tests/ipc_mount.rs`.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo xtask check` pass on macOS.
  `/private/tmp/slates-special-clippy-host.log`, `/private/tmp/slates-special-xtask.log`.
- Windows cross-Clippy was attempted with `--target x86_64-pc-windows-msvc -p
  slates-bridge-winfsp --all-targets --no-default-features --features slates-machine/pure-hash`.
  It stopped in zstd's existing C dependency: the host lacks target `stdlib.h`/`string.h`.
  This is not evidence that the changed Windows-only host code compiles.
  `/private/tmp/slates-special-clippy-windows.log`.

The unchanged full pinned pjdfstest rerun took **172,343 ms**: **6,970 passed, 1,800 failed,
28 TODO**, across the same 238 files and 8,798 cases. That is **1,795 additional passes**.
Artifacts: `/private/tmp/slates-mounted-evidence/conformance-special.tar.gz` and
`/private/tmp/slates-special-pjd.log`. The container and command match the baseline above.
The remaining device failures cascade into later assertions, sometimes involving a supported
kind: for example `rename/13.t:12` cannot create a block device, case 13 consequently moves the
source directory, and later socket/symlink cases inherit the damaged setup. These are still
unlisted; a passing FIFO test does not justify accepting an arbitrary dependent failure.

### Explicit remaining work

- The VFS deriver now retains IPC deltas, but the separate `slates-merge` Origin/engine/work
  protocol still lacks this dimension. `walk_origin` explicitly refuses snapshots containing
  IPC names. It must gain canonical metadata, histories, type-conflict decisions and replay
  together; the VFS tests do not prove that service integration.
- Review the remaining mounted failures against the accepted no-device contract, including
  dependency cascades, and the two NFS-specific limits above. The expected-failure list is empty.
- Native FSKit/WinFsp IPC support remains unavailable; the new refusals make that boundary explicit.

### Final local verification (2026-09-20)

`SLATES_TEST_DRIVER=io_uring cargo test --offline --workspace` in the approved disposable
Debian 13 / aarch64 Linux 6.12.76 container (Rust 1.98.0, four CPUs, 4 GiB):
**1,524 passed, zero failed, 14 ignored**. All 49 fleet histories passed in 248.00 s.
The ordinary-user mounted FIFO/socket clone test passed; the container had `/dev/fuse`,
its approved mount capability and the approved io_uring seccomp profile. Strict workspace
Clippy and `cargo xtask check` passed in the same serial run.
Log: `/private/tmp/slates-special-workspace-collector-fixed.log`.
Final macOS strict workspace Clippy also passed (6.22 s):
`cargo clippy --offline --workspace --all-targets -- -D warnings`,
`/private/tmp/slates-special-clippy-host-complete.log`. Formatting and diff whitespace
checks passed. The Windows C-header limitation above remains.

This workspace result is separate from the unchanged pjdfstest result above (1,800 failures)
and does not close the explicitly listed merge-service work or native-platform limitations.
Validation also found and corrected the telemetry drain oracle, the hedge fixture's candidate
selection, collection of queued replies at expiration, and late-offer session recovery:
[telemetry](2026-09-20-telemetry-drain-counts-its-own-spans.md),
[hedge target](2026-09-20-hedge-test-reconstructs-the-wrong-candidate.md),
[collector](2026-09-20-collectors-expire-before-reading-queued-replies.md).

## Failing files (the exact local baseline)

| Pinned test file | Failed cases |
| --- | ---: |
| `tests/chmod/00.t` | 60 |
| `tests/chmod/01.t` | 12 |
| `tests/chmod/11.t` | 56 |
| `tests/chown/00.t` | 732 |
| `tests/chown/01.t` | 16 |
| `tests/chown/07.t` | 72 |
| `tests/link/00.t` | 128 |
| `tests/link/01.t` | 24 |
| `tests/link/10.t` | 8 |
| `tests/mkdir/01.t` | 12 |
| `tests/mkdir/10.t` | 11 |
| `tests/mkfifo/00.t` | 31 |
| `tests/mkfifo/01.t` | 12 |
| `tests/mkfifo/02.t` | 3 |
| `tests/mkfifo/03.t` | 3 |
| `tests/mkfifo/05.t` | 4 |
| `tests/mkfifo/06.t` | 5 |
| `tests/mkfifo/09.t` | 12 |
| `tests/mknod/00.t` | 31 |
| `tests/mknod/01.t` | 20 |
| `tests/mknod/02.t` | 9 |
| `tests/mknod/03.t` | 9 |
| `tests/mknod/05.t` | 4 |
| `tests/mknod/06.t` | 5 |
| `tests/mknod/08.t` | 20 |
| `tests/mknod/11.t` | 24 |
| `tests/open/01.t` | 16 |
| `tests/open/06.t` | 47 |
| `tests/open/17.t` | 3 |
| `tests/open/22.t` | 8 |
| `tests/open/24.t` | 5 |
| `tests/rename/00.t` | 68 |
| `tests/rename/09.t` | 928 |
| `tests/rename/10.t` | 844 |
| `tests/rename/12.t` | 24 |
| `tests/rename/13.t` | 26 |
| `tests/rename/14.t` | 16 |
| `tests/rename/20.t` | 14 |
| `tests/rename/23.t` | 32 |
| `tests/rmdir/01.t` | 3 |
| `tests/rmdir/06.t` | 16 |
| `tests/symlink/08.t` | 8 |
| `tests/unlink/00.t` | 64 |
| `tests/unlink/11.t` | 128 |
| `tests/unlink/14.t` | 1 |
| `tests/utimensat/00.t` | 20 |
| `tests/utimensat/09.t` | 1 |
