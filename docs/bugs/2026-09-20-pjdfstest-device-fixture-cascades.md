# First root pjdfstest review: refused device fixtures and their dependent cases

Date: 2026-09-20. Design: §4.4/§4.6 A-26; AC-9.7/T-9.1. This is the first root
review left pending in `docs/wip/conformance.md` §3.4. It does not claim full POSIX
conformance or native FUSE coverage.

## Evidence

The unchanged pinned upstream suite, `pjd/pjdfstest@85a8aea9e685999ef0540392fd80535f873d7ff7`,
ran through Linux's root-mounted NFSv3 adapter with an ordinary-user slates daemon:
238 files, 8,798 cases, **6,970 passed, 1,800 failed, 28 TODO**. Job
[106138990733](https://github.com/hyper-light/slates/actions/runs/35533762695/job/106138990733)
and the local run agree on these counts. The local raw output is retained in
`/private/tmp/slates-mounted-evidence/conformance-special.tar.gz`.

A diagnostic copy added TAP comments before assertions naming the current `type`,
`type2` and `type3` loop variables. It changed no assertion. It failed the **same exact
1,800 case identifiers**, in 172,774 ms. A second diagnostic removed `block char` from
66 loop headers, leaving assertions and explicit device sections alone: **5,548 passed,
70 failed, 22 TODO**, in 130,332 ms. Of those 70, 68 are explicit device sections and two
are NFS client/wire limitations. This isolation run has deliberately stale upstream TAP
plans and is diagnostic evidence only, never a passing conformance record.

Commands: `xtask conformance run --suite pjdfstest --records /scratch/conformance-records
--scratch /scratch/slates-scratch --keep`, with the diagnostic copies prepared by
`/private/tmp/slates-run-pjd-device-context.sh` and
`/private/tmp/slates-run-pjd-supported-context.sh`. Archives:
`conformance-device-context.tar.gz` and `conformance-supported-context.tar.gz` in the
same evidence directory. The wrapper logs are `/private/tmp/slates-pjd-device-context.log`
and `/private/tmp/slates-pjd-supported-context.log`.

## Cause and exact classification

A-26 admits FIFO/socket metadata and local endpoint semantics. It deliberately refuses
block and character devices. A failed device creation therefore invalidates later tests
that assume that inode exists. Some loops also move or remove a surviving directory or
regular file, contaminating subsequent supported-kind iterations. Labelling only the
device creation assertion would miss those dependencies; labelling a whole file would
hide independent defects.

| Reviewed cause | Cases | Dependency |
|---|---:|---|
| DEVICE-FIXTURE | 772 | Direct device iterations: creation, lookup, metadata and namespace effects require the missing device. |
| DEVICE-EXPLICIT | 44 | `mknod/02.t`, `mknod/03.t`, and explicit device sections of `unlink/00.t`; each reason names its creation case. |
| DEVICE-SOURCE | 464 | `rename/09.t`: a missing device source/destination changes inverse renames and invalidates the saved inode comparison. |
| DEVICE-STICKY | 466 | `rename/10.t`: an absent device destination removes the sticky-directory denial precondition; a rename can consume its surviving source. Forty later supported-kind cases inherit this absence. |
| DEVICE-DIR | 9 | `mkdir/10.t`: refused device at case 10; case 11 creates a directory that later `unlink` cannot remove. |
| DEVICE-MOVE | 21 | `rename/13.t`: refused device at case 12; case 13 moves the source directory into the absent destination. |
| DEVICE-EMPTY | 9 | `rename/20.t`: missing device child at case 12; case 13 replaces an empty destination and consumes the source. |
| DEVICE-PARENT | 13 | `rmdir/06.t`: missing device child at case 11; case 12 removes the parent needed later. |
| NFS-OPEN | 1 | `unlink/14.t:4`: Linux's NFS client keeps an open unlinked inode through a temporary name until close, so link count stays one. |
| NFS-TIME | 1 | `utimensat/09.t:5`: NFSv3's unsigned 32-bit seconds cannot carry 2³²; Linux clamps it to 2³²−1. |

The NFS limitations follow the [Linux v6.12 NFS unlink implementation](https://github.com/torvalds/linux/blob/v6.12/fs/nfs/unlink.c)
and [RFC 1813's `nfstime3`](https://www.rfc-editor.org/rfc/rfc1813#page-21).
They are transport limitations, not exclusions from the shared VFS contract.

The classified failures consist of 1,689 cases running in a device iteration, 44 explicit
device cases, 65 dependent cases outside a device iteration, and the two transport limits.
The isolated supported-kind run leaves only `mknod/02.t` (6), `mknod/03.t` (6),
`mknod/11.t` (24), `unlink/00.t` (32), `unlink/14.t` (1), and `utimensat/09.t` (1).
Supported FIFO/socket loops, permissions and sticky rename histories pass after the
unsupported device fixtures are isolated.

## Change and guard

Initialize `docs/wip/conformance/expected-failures/native-linux-fuse.pjdfstest.txt`
with the 1,800 exact reviewed identifiers and an individual causal explanation. The
historical slug names Linux FUSE, but these records explicitly remain **LIMITED: NFS
adapter**. No wildcard, assertion substitution, upstream patch or supported-kind skip
is added to CI. Unlisted failures, listed cases that pass, and listed cases that disappear
remain gate failures. After this first root review the list can only shrink.

The classification script `/private/tmp/slates-review-pjd-cases.py` verifies equality of
the original and annotated failing identifiers, the isolated remainder, and the manually
reviewed dependency groups. It is not a failure-to-expectation generator used by CI.

The complete unchanged suite passes this reviewed gate in **174,353 ms**: 6,970 passes,
1,800 expected failures, 28 TODO, zero unexpected failures, zero listed cases passing or
absent. The same full run passes fsx (10,000 operations), fsstress (500 × 4), all nine
workloads and hermeticity: 199 write calls, 22 inside the grant (six matched written
entries), 89 RAM, 88 stdio, zero unresolved/outside. Seven conformance harness regressions
also pass. Command wrapper: `/private/tmp/slates-run-conformance-reviewed.sh`; log:
`/private/tmp/slates-conformance-reviewed.log`; raw archive:
`/private/tmp/slates-mounted-evidence/conformance-reviewed.tar.gz`. The five typed records
are tracked under `docs/wip/conformance/records/`. This closes the first expectation review.
Native FUSE conformance, wider special-file histories and other platform evidence remain
separate work in GAPS; the adapter cannot prove them.
