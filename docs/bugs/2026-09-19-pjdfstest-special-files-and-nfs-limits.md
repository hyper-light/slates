# Pjdfstest: special-file omissions and two NFSv3 limits

Date: 2026-09-19 (execution records use UTC 2026-09-20).
Design: §4.5 namespace, §4.6 mounted equivalence, §4.8 recovery, §4.16 merge.
Status: reproduced and scoped; not fixed or allowlisted.

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

If supported, special-file identity belongs in the shared VFS, not an NFS-only side table.
Represent the kind and, for devices, major/minor numbers on the inode. Use the existing
bounded inode/name accounting, owner and mode checks, hard links, rename/unlink, snapshots
and clone ownership. Kernel clients own live pipe/socket behavior; the server must never
open a host device to service such an inode.

The same metadata must survive recovery, canonical archive/merge representations and
all bridge attribute encodings. A landing must apply a supported node type or refuse it
before writing; it must not silently drop it or request daemon privileges. This spans
§4.5, §4.6, §4.8, §4.15 and §4.16 and cannot be fixed correctly by making MKNOD return OK
for a regular file. Start with an NFS creation/lookup/link/rename oracle, a snapshot and
restart oracle, and malformed type/device input. Rerun the full unchanged mounted suite.

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
