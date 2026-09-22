# macOS NFS failures that need distinct reproductions

Date: 2026-09-22. Evidence: run 35604717581, job 106348895431; pinned
pjdfstest commit `85a8aea9e685999ef0540392fd80535f873d7ff7`.

The downloaded macOS TAP has 1,906 failures. Of these, 1,789 case identifiers
also occur in the reviewed Linux list. Matching identifiers do **not** establish
matching causes. The remaining 117 identifiers fall into the groups below.
This is a diagnosis map, not an expected-failure list or a conformance closure.
No test assertion, suite exclusion, or capability promise changes here.

| Cases | Observed failure | Evidence and next discriminating check |
|---:|---|---|
| 74 | Maximum-path histories use empty operands or reach the wrong errno. | `tests/misc.sh::dirgen_max` does arithmetic on `pathconf(..., _PC_PATH_MAX)` without checking its result. Apple's `nfs_vnop_pathconf` returns EINVAL for this selector before an RPC. Reproduce the selector and generated operand on the mount, then test actual pathname boundaries independently. |
| 8 | Directory ownership changes retain mode 06555 where the fixture expects 0555. | The pinned `chown/00.t` marks these same histories TODO on Linux. `access::with_setid_side_effects` intentionally preserves directory bits. POSIX's mandatory clearing rule concerns executable regular files; review the directory policy separately from regular-file safety. |
| 6 | `chown(-1, -1)` changes ctime where the fixture requires equality. | The pinned suite marks this expectation TODO on Linux and quotes POSIX's optional timestamp update. Capture the macOS client's actual SETATTR fields; do not infer that an empty neutral `SetAttr` changed an inode. The shared bridge already avoids calls for absent fields. |
| 10 | FIFO open permission and open-without-reader histories differ. | Nine cases in `open/06.t`, one in `open/17.t`. Apple's FIFO vnode operations and client authorization need a mounted trace. The pure server's ACCESS decision alone cannot prove that this client consults it for a local FIFO endpoint. |
| 2 | A path truncate by the owner succeeds after write permission is removed. | `truncate/06.t:6` and `ftruncate/06.t:6`. NFS's owner override also preserves writes through a descriptor opened before chmod. Observe whether the client performs ACCESS; removing the override without both histories would break open-descriptor behavior. |
| 16 | Cross-parent directory rename permission histories and their following checks differ. | Eight cases each in `rename/09.t` and `rename/10.t`. The source-directory write check and the platform-specific expectations need an independent host/mount comparison. Later failures can follow an earlier refused rename. |
| 1 | RMDIR of `child/..` reports success. | Confirmed server defect: NOENT was translated to success by Apple's retry handling. The explicit NOTEMPTY reply now passes the wire regression; mounted confirmation remains owed. |

## Sources and limits

The pinned suite's own comments distinguish directory set-id and no-op ctime
behavior from portable requirements. The standard requires set-id clearing for
an unprivileged ownership change to an executable regular file; other file types
have latitude. It also permits timestamps to remain unchanged when both ownership
arguments are -1. These facts justify testing the actual contract; they do not
justify altering an unrelated failing history. See
[POSIX chown](https://pubs.opengroup.org/onlinepubs/9699919799.2013edition/functions/chown.html).

Apple's [NFS vnode implementation](https://github.com/apple-oss-distributions/NFS/blob/main/kext/nfs_vnops.c)
contains the unsupported pathconf selector and RMDIR retry behavior. The reviewed
copy is `/private/tmp/slates-apple-nfs_vnops.c`; the downloaded TAP and the 117-case
extraction are under `/private/tmp/slates-ci-35604717581-artifacts/` and
`/private/tmp/slates-ci-35604717581-macos-extra-failures.txt`.

The separate AppleDouble limitation remains open: the NFSv3 mount creates visible
sidecar files for provenance attributes, breaking real git/Python/rsync/editor
histories. The workload comparator now includes those names. Hiding their names
or converting these failures into passes would conceal missing behavior.
