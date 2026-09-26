# First macOS root pjdfstest review: 1,905 failures by cause

Date: 2026-09-26. Contracts: GAPS §8i (the POSIX contract is never satisfied by growing a list), A-26
(block and character devices refused; FIFO and socket metadata authorized), RFC 1813. Input: CI run
36202635768 (`85a29fb`), job `conformance (macos-latest)`, macOS 26.6.2 (25G83), run as root: 238
files, 8,686 cases, 6,781 passed, 1,905 failed, all unexpected because the root list was empty.

## Evidence

The run's kept per-file outputs (artifact `pjdfstest-output-macos-latest`) were compared case by case
with the reviewed Linux root list (`native-linux-fuse.pjdfstest.txt`, 1,800 cases):

- **1,789** failures are the same cases the Linux list names. All are A-26 device-fixture cascades
  (the server refuses `mknod b/c` whichever client asks), plus `unlink/14.t:4`: the macOS client
  silly-renames an open unlinked file too (`nfs_sillyrename`, NFS kext `nfs_vnops.c`), so `nlink` stays 1.
- **11** Linux cases pass on macOS (`utimensat/00.t:17–26`, `utimensat/09.t:5`): the Linux client's time
  clamp, which macOS does not share.
- **116** fail only on macOS. Each class below was reproduced or traced to source. The earlier diagnosis map
  (`2026-09-22-macos-nfs-conformance-boundaries.md`) named the same groups; its `rmdir child/..` case is
  fixed and no longer fails.

| Class | Cases | Evidence |
|---|---|---|
| NFS-PATHCONF | 74: every `*/03.t` (70) and `rename/02.t` (4) | On a live slates mount on this Mac, `pathconf(".", _PC_NAME_MAX)` is 255 (our PATHCONF reply) and `_PC_PATH_MAX` is `-1 EINVAL`. NFSv3's `PATHCONF3resok` has no PATH_MAX field (RFC 1813 §3.3.20), so no server can supply it. pjdfstest's `dirgen_max` then builds no path; `chmod/03.t` reproduced here with the same five failures CI shows. |
| POSIX-OPTIONAL | 14: `chown/00.t` | Eight: a directory keeps S_ISUID/S_ISGID (06555) after an unprivileged chown. Six: `chown(-1, -1)` moves the change time. POSIX leaves both to the implementation. pjdfstest marks these same histories `todo Linux` ("Linux doesn't clear the SGID/SUID bits for directories"; "If both owner and group are -1, the times need not be updated"). slates follows the Linux choice (`access::with_setid_side_effects` clears set-id on files only). |
| NFS-OWNER-OVERRIDE | 2: `truncate/06.t:6`, `ftruncate/06.t:6` | The owner truncates its own 0444 file. NFSv3 cannot tell `truncate(path)` from `ftruncate(fd)`, so a server lets the owner change the size (Linux knfsd, `nfsd_may_truncate` → `NFSD_MAY_OWNER_OVERRIDE`: "We must trust the client to do permission checking - using ACCESS with NFSv3"). The Linux client checks locally and passes. The macOS client sends the SETATTR unchecked. The non-owner case (`:3`) is refused and passes. |
| NFS-FIFO-OPEN | 10: `open/06.t` (FIFO section, 9), `open/17.t:2` | On a live mount as the owner of a 0600 fifo: `access(R_OK/W_OK)` is true, `open(O_RDONLY/O_WRONLY \| O_NONBLOCK)` is EACCES; on APFS the same opens give 0 and ENXIO. The NFS kext gives a fifo the FIFO vnode table only `#if FIFO` (`nfs_node.c`). Otherwise it gets the plain table, whose `nfs_vnop_open` refuses anything but a file, directory or link with EACCES. |
| MACOS-RENAME-AUTH | 16: `rename/09.t` (8), `rename/10.t` (8) | A directory renamed onto an existing directory in the caller's own sticky directory is refused EACCES. The server accepts both RENAMEs as the caller: two new tests in `crates/bridge-nfs/tests/procedures.rs` reproduce `rename/09.t:2279/2299` and `rename/10.t:2056/2063`. xnu's `vn_authorize_renamex_with_paths` asks `ADD_SUBDIRECTORY` of the replaced directory itself when a directory target exists (`moving`), which POSIX does not ask. The `rename/10` cases are exactly that, with a target the caller cannot write. The `rename/09` cases' kernel-side check is not pinned to one line; their server acceptance is. |

## Change

- `docs/wip/conformance/expected-failures/native-macos-nfs.pjdfstest.txt` names the 1,905 cases, each
  with its reviewed reason (BLAKE3 `7cc0d2ea…0c3a`). `cargo xtask conformance tally --outputs
  <artifact> --privilege root` over the CI run's outputs: 1,905 expected, 0 unlisted, 0 listed now
  passing, 0 listed absent.
- Two server tests pin the POSIX rename rules the macOS client refuses.

## Not changed

- The device refusals read EIO on the macOS mount (`mknod b/c`). The server answers the typed
  `NFS3ERR_NOTSUPP`, and the macOS client presents it as EIO
  (`docs/bugs/2026-09-14-nfs-special-files-refused-as-eio.md`); that is already declared.
- The same lane's workloads verdict (cargo differs by a `._target` AppleDouble sidecar) and hermeticity
  verdict (93 write-capable calls unresolved) still fail; they are separate reviews.
