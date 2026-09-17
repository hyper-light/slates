# Transport conformance evidence (AC-9.7 / T-9.1; GAP-A9-15)

> **Status (2026-09-14).** The evidence surface exists and every cell of the matrix below has a
> record: the harness (`cargo xtask conformance`, the I/O half) drives the real `slates` binary —
> anchor, daemon, a provisioned volume, a real kernel mount — runs one named suite inside a bounded
> scratch directory in the mount, tears down, and writes one typed record per (transport × suite)
> into `docs/wip/conformance/records/`; the pure half (`crates/conformance`: records, the matrix,
> the reviewed expected-failure list that only shrinks, the parsers of pjdfstest TAP, fsx,
> fsstress, strace and fs_usage output, the workload roster and its byte-identity comparison) is
> under the lint wall with hostile-input tests. The matrix is a doc-truth block
> (`crates/conformance/tests/matrix.rs` re-renders it from the tracked records and fails on drift;
> `cargo xtask conformance matrix --write` or the `--ignored regenerate_the_evidence_matrix` test
> rewrites it). On this macOS host the native NFS loopback transport ran fsx (pass), fsstress
> (pass), the workload suite (every tool differs from the host: two declared limits of the NFS
> transport, §3.3) and pjdfstest unprivileged (LIMITED; §3.4). The hermeticity tracer needs root
> on macOS and is a typed privilege skip here; the CI lane (`conformance` in `ci.yml`) runs it with
> `sudo fs_usage` on macOS and `strace -f -y` on Linux. Linux, Windows, virtio-fs and OCI cells are
> SKIPPED with their typed reasons — none is claimed. "Historical stages overstate coverage" is
> closed by construction: a cell cannot say RAN without a record that carries counts, a date, the
> host and the command.

## 1. The rule of this document

Every cell of the matrix is one of:

- **RAN(counts; date; host)** — the suite ran over the transport as shipped; the counts are the
  record's, and the command behind the cell is listed under the table (CLAUDE.md §5: a number
  without its command is not evidence).
- **LIMITED(adapter; not covered; counts; date; host)** — the suite ran, but through an adapter
  that is not the offered transport (the Linux lane's root NFS mount by the OS client) or at
  reduced scope (an unprivileged pjdfstest run); the cell names both what stood in and what it
  leaves uncovered (EQUIVALENCE.md §8: "Limited native NFS/Windows semantics must be declared").
- **SKIPPED(class: reason)** — the suite did not run; the class is `lane` (another OS's lane),
  `tool` (a tool absent on the host), `privilege` (root the suite — never the daemon — needs),
  `hardware`, `owed` (the product piece does not exist yet) or `not applicable`. The reasons come
  from the capability table (`crates/conformance/src/capability.rs`), one sentence each.
- **OWED (no record)** — no record at all; `every_cell_has_a_record` fails on it, so it cannot be
  committed.

A skip never overwrites evidence (`Record::may_replace`): a laptop's `plan` cannot erase what a
lane proved. Records are written outside the tree by CI (uploaded as artifacts) and copied in
deliberately, so a run's date cannot drift the doc-truth test.

## 2. Evidence matrix

Transports are the five AC-9.7 names (§4.6); suites are the design's (Part 6): the three
conformance suites, the workloads, the hermeticity tracer, and the pressure and failure suites,
which have no runnable form yet and say so.

<!-- conformance-matrix:begin -->
| Transport | POSIX conformance (pjdfstest) | fsx | fsstress | workloads | hermeticity | pressure | failure suites |
|---|---|---|---|---|---|---|---|
| native macOS (NFS loopback) | LIMITED(adapter: a non-root run; not covered: 4037 cases only root could pass (a uid/gid switch, a device node, a chown to another owner, or an expectation of one), counted as needs-root rather than run; 238 files, 8686 cases: 2429 passed, 2220 failed (0 expected, 2220 unexpected, 0 listed-now-passing), 4037 needs-root, 0 todo; 2026-09-16; macOS 26.4.1 (25E253)) | RAN(10000 operations, seed 1, file length 262144: passed; 2026-09-14; macOS 26.4.1 (25E253)) | RAN(500 operations × 4 processes, seed 1, 2000 logged, disabled: dread,dwrite: passed; 2026-09-14; macOS 26.4.1 (25E253)) | RAN(identical: cargo, npm, rg, sqlite; DIFFERS: git, python, rsync, editor; skipped: watcher (fswatch absent); 2026-09-16; macOS 26.4.1 (25E253)) | SKIPPED(privilege: fs_usage needs root for the kernel tracing facility it uses (its manual); macOS has no unprivileged filesystem-write tracer; this host has no passwordless sudo (`sudo -n true` was refused)) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) |
| native Linux (FUSE) | SKIPPED(lane: runs in the Linux lane, not on macOS) | SKIPPED(lane: runs in the Linux lane, not on macOS) | SKIPPED(lane: runs in the Linux lane, not on macOS) | SKIPPED(lane: runs in the Linux lane, not on macOS) | SKIPPED(lane: runs in the Linux lane, not on macOS) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) |
| native Windows (WinFsp) | SKIPPED(not applicable: a POSIX C suite with no Windows build (pjdfstest and fsstress use fork, uid switching and POSIX namespace calls)) | SKIPPED(owed: the WinFsp fsx port (Phase 4 task 5) is not wired into the harness, and the harness has no WinFsp mount step) | SKIPPED(not applicable: a POSIX C suite with no Windows build (pjdfstest and fsstress use fork, uid switching and POSIX namespace calls)) | SKIPPED(owed: the harness has no WinFsp mount step; the live WinFsp mount test (crates/bridge-winfsp/tests/mount.rs, gated WINFSP_TEST_MOUNT=1 on windows-latest) proves create/write/read/list/delete through the kernel and nothing more) | SKIPPED(owed: no ETW filesystem-write tracer is wired for Windows) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) |
| virtio-fs guest | SKIPPED(owed: no live Linux guest: the device half is proven only by the simulated guest driver (docs/wip/virtiofs.md), and AC-9.7 says a simulation cannot close the transport guarantee) | SKIPPED(owed: no live Linux guest: the device half is proven only by the simulated guest driver (docs/wip/virtiofs.md), and AC-9.7 says a simulation cannot close the transport guarantee) | SKIPPED(owed: no live Linux guest: the device half is proven only by the simulated guest driver (docs/wip/virtiofs.md), and AC-9.7 says a simulation cannot close the transport guarantee) | SKIPPED(owed: no live Linux guest: the device half is proven only by the simulated guest driver (docs/wip/virtiofs.md), and AC-9.7 says a simulation cannot close the transport guarantee) | SKIPPED(owed: no live Linux guest: the device half is proven only by the simulated guest driver (docs/wip/virtiofs.md), and AC-9.7 says a simulation cannot close the transport guarantee) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) |
| OCI container | SKIPPED(owed: the OCI handoff (the attach verb's container form, crates/server) is under construction; there is no container attachment form to drive a suite through) | SKIPPED(owed: the OCI handoff (the attach verb's container form, crates/server) is under construction; there is no container attachment form to drive a suite through) | SKIPPED(owed: the OCI handoff (the attach verb's container form, crates/server) is under construction; there is no container attachment form to drive a suite through) | SKIPPED(owed: the OCI handoff (the attach verb's container form, crates/server) is under construction; there is no container attachment form to drive a suite through) | SKIPPED(owed: the OCI handoff (the attach verb's container form, crates/server) is under construction; there is no container attachment form to drive a suite through) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) |

Commands behind the cells above (the contract of each number):

- **native macOS (NFS loopback) × POSIX conformance (pjdfstest)** (2026-09-16, macOS 26.4.1 (25E253), Darwin 25.4.0 arm64): `sh <each of 238 tests/**/*.t of pjd/pjdfstest@85a8aea> in a directory inside the mount; binary <scratch>/tools/pjdfstest-85a8aea9e685999ef0540392fd80535f873d7ff7/pjdfstest`; bound: every test file at the pinned commit (238 files), 300s per file.
  - pjdfstest: pjd/pjdfstest@85a8aea (BSD-2-Clause), tarball sha256 2005cdd83b76204177cf136792b1f2058a7418b4fc5b203f51274a698547d754, built `cc -O2 -w -I. pjdfstest.c` over a config.h the harness probed (HAVE_: CHFLAGS FACCESSAT FCHFLAGS FCHMODAT FCHOWNAT FSTATAT LCHFLAGS LCHMOD LINKAT MKDIRAT MKFIFOAT MKNODAT OPENAT READLINKAT RENAMEAT SYMLINKAT UTIMENSAT STRUCT_STAT_ST_ATIMESPEC STRUCT_STAT_ST_BIRTHTIME STRUCT_STAT_ST_BIRTHTIMESPEC STRUCT_STAT_ST_CTIMESPEC STRUCT_STAT_ST_MTIMESPEC)
  - 0 expected failures; 2220 unlisted failures (tests/chmod/00.t:22, tests/chmod/00.t:23, tests/chmod/00.t:24, tests/chmod/00.t:26, tests/chmod/00.t:27, tests/chmod/00.t:28, tests/chmod/00.t:31, tests/chmod/00.t:33 … and 2212 more); 0 listed now passing; 0 listed absent
  - failures by shape (12 of 182 shapes; names folded to N, inode numbers to <inode>): 634 × lstat N/N inode, expected ENOENT, got <inode>; 237 × unlink N/N, expected 0, got ENOENT; 187 × unlink N, expected 0, got ENOENT; 130 × test_check (a comparison of two values the script read; no message); 65 × create N/N 0644, expected 0, got EEXIST; 65 × symlink test N/N, expected 0, got EEXIST; 60 × bind N/N, expected 0, got EADDRINUSE; 60 × chmod N 06555, expected 0, got ENOENT; 60 × lstat N/N inode, expected <inode>, got <inode>; 60 × mkfifo N/N 0644, expected 0, got EEXIST; 60 × mkfifo N/N 0644, expected 0, got EIO; 59 × bind N/N, expected 0, got EIO
- **native macOS (NFS loopback) × fsx** (2026-09-14, macOS 26.4.1 (25E253), Darwin 25.4.0 arm64): `cd <mount>/conformance-<pid>/fsx && <scratch>/tools/fsx -N 10000 -S 1 -l 262144 -q -P <scratch>/fsx-logs fsx.bin`; bound: 10000 operations, seed 1, file length 262144 bytes.
  - fsx: freebsd/freebsd-src@42c6944 tools/regression/fsx/fsx.c (APSL 2.0), sha256 b064208bec8519e80038ee1da8cb9c0f7c512a3242bbf4c06809a88ce15ae019, built `cc -O2 -w -include time.h` unchanged
  - the exerciser's .fsxlog/.fsxgood files were kept outside the mount (-P)
- **native macOS (NFS loopback) × fsstress** (2026-09-14, macOS 26.4.1 (25E253), Darwin 25.4.0 arm64): `<scratch>/tools/ltp/fsstress -d <mount>/conformance-<pid>/fsstress -n 500 -p 4 -s 1 -v -f dread=0 -f dwrite=0`; bound: 500 operations per process × 4 processes, seed 1.
  - fsstress: linux-test-project/ltp@6af38cf testcases/kernel/fs/fsstress/fsstress.c (GPL-2.0), sha256 9a80bbe1f1ad933845b9272b5057776e74923503bbb6f392bf1e135c1744d644, built `cc -O2 -w -DNO_XFS -D_GNU_SOURCE -include shim/config.h` over the harness's shim config.h (Darwin: LFS names aliased to the native 64-bit ones, O_DIRECT defined 0, so dread/dwrite are disabled with -f)
  - the daemon answered `volume list` after the run: true
- **native macOS (NFS loopback) × workloads** (2026-09-16, macOS 26.4.1 (25E253), Darwin 25.4.0 arm64): `sh -c '<roster script>' in <host scratch>/<tool> and <mount>/<tool>, per tool of crates/conformance/src/workload.rs ROSTER; trees compared by manifest`; bound: 8 tools run, one script each.
  - the volume was created with --fold=true to match the host scratch's name policy; git identity and dates fixed; sqlite busy timeout 5000 ms; watcher wait 3 s
  - git: exit code: host 0, mount 8; mount output tail: 100644 eeba437bfae228be7c57b36d3423fcb15eb3412a 0	._link ⏎ 100644 2227cddb7f6318ea735a1c4adb52f5cd36c5783c 0	a.txt ⏎ 100755 eeba437bfae228be7c57b36d3423fcb15eb3412a 0	d/._c.txt ⏎ 100755 cc628ccd10742baea8241c5924df992b5c019f71 0	d/c.txt ⏎ 120000 8d14cbf983b3fad683171c9418998d9f68340823 0	link ⏎ error: refs/._heads: badRefName: invalid refname format ⏎ error: refs/._heads: badRefContent:  ⏎ error: refs/heads/._master: badRefName: invalid refname format ⏎ error: refs/heads/._mas…
  - python: output line 2: host "{\"names\": [\"main.py\", \"out.json\", \"pkg\"]}", mount "{\"names\": [\"._main.py\", \"._out.json\", \"._pkg\", \"main.py\", \"out.json\", \"pkg\"]}"
  - rsync: exit code: host 0, mount 1; mount output tail: 3 ⏎ Only in dst: ._._link ⏎ Only in dst: ._._one.txt ⏎ Only in dst: ._._sub ⏎ Only in dst/sub: ._._two.txt
  - editor: output line 2: host "note.txt", mount "._note.txt"
<!-- conformance-matrix:end -->

## 3. What the macOS runs found (2026-09-14, and 2026-09-16 UTC after the POSIX access-control change; this host)

Host: macOS 26.4.1 (25E253), Darwin 25.4.0, arm64, 18 cores; the box was shared and loaded (a
VM at 130–160 % CPU and several Chrome renderers near 100 % each; swap 16.0 GB of 17.4 GB used
per `sysctl vm.swapusage`), so the durations are the record's wall times, not benchmarks. The
transport is the daemon's NFSv3 loopback server under `slates mount` (`mount_nfs`, no privilege).

### 3.1 fsx — passed

`cd <mount>/conformance-<pid>/fsx && <scratch>/tools/fsx -N 10000 -S 1 -l 262144 -q -P
<scratch>/fsx-logs fsx.bin`: 10,000 operations, seed 1, file length 262,144 bytes, `All
operations completed A-OK!`, 5.6 s. The exerciser is FreeBSD's `fsx.c` (APSL 2.0) at
`freebsd-src@42c6944`, SHA-256 `b064208b…`, built unchanged with `cc -O2 -w -include time.h`.

### 3.2 fsstress — passed

`<scratch>/tools/ltp/fsstress -d <mount>/…/fsstress -n 500 -p 4 -s 1 -v -f dread=0 -f dwrite=0`:
500 operations × 4 processes, seed 1, 2,000 operations logged, exit 0, the daemon answering
`volume list` afterwards. LTP's `fsstress.c` (GPL-2.0) at `ltp@6af38cf`, SHA-256 `9a80bbe1…`,
built with `-DNO_XFS` over the harness's shim `config.h` (on Darwin the LFS names alias to the
native 64-bit ones and `O_DIRECT` is defined 0, so the two direct-I/O operations are disabled with
`-f` rather than run buffered under a false name; the record's `disabled_operations` says so).

### 3.3 Workloads — four identical, four differ only by AppleDouble sidecars; two declared limits

Nine roster tools (`crates/conformance/src/workload.rs`): git, cargo, npm, python3, rg, rsync,
sqlite3, vim (the editor save pattern), and a watcher (`fswatch` on macOS, absent on this host —
SKIPPED naming it). Each ran once in a host directory and once inside the mount under one fixed
environment (git identity and dates pinned, npm logs off, the volume created with `--fold=true`
to match the host directory's name policy), and the two runs were compared: exit code, output with
the directory roots normalized, and the tree manifest (path, kind, mode, size, BLAKE3, symlink
target) under the roster's reviewed exclusions, with the AppleDouble `._*` sidecars of the row
below stripped from both trees (`without_sidecars`; a tool's *output* is never edited).

Of the eight that ran (2026-09-16 UTC, 38 s), **cargo, npm, rg and sqlite are identical** to the
host; git, python, rsync and vim differ, each only by sidecars that reach the tool's own output:

| Cause | Tools it reaches | Record |
|---|---|---|
| Every entry created in the mount gets an AppleDouble `._name` sidecar (4,096 bytes): the kernel attaches `com.apple.provenance` to entries these processes create (every launch path tried on 2026-09-15 — a plain shell, `env -i`, `nohup`, `script` — carries the tag), NFSv3 cannot store an extended attribute, and the macOS NFS client keeps it in a sidecar; `mount_nfs` offers `namedattr` for NFSv4 only. Unlinking an entry removes its sidecar. | git (`git add -A` stages `._link` and `d/._c.txt`, and `.git/refs/._heads` makes `git fsck --strict` exit 8), rsync (`diff -r` finds `._._link` — the sidecar's sidecar), python (`os.listdir` lists `._main.py`), vim (`ls -A` lists `._note.txt`). cargo, npm and rg meet the sidecars only in the tree, which the comparison strips: identical. | [docs/bugs/2026-09-14-nfs-appledouble-sidecars.md](../bugs/2026-09-14-nfs-appledouble-sidecars.md) |
| SQLite refuses WAL mode on any filesystem whose type is `nfs`: its unix VFS picks `nfsIoMethods` ("shared memory is disabled", no `xShmMap`) by `f_fstypename`, so `PRAGMA journal_mode=WAL` answers `delete`. The two `journal_mode` lines are discarded (the fs's mmap support, not the workload); the two-process inserts, the count, the order and `integrity_check` are compared and match: identical. | sqlite (T-3.3's WAL requirement) | [docs/bugs/2026-09-14-nfs-sqlite-wal-refused.md](../bugs/2026-09-14-nfs-sqlite-wal-refused.md) |

Whether a host's processes carry the provenance tag is environmental; the CI macOS runner's record
will show whether its runs see sidecars. Harness gaps found and closed on the way, none touching
the transport: fsx's `-P` needs a relative file name; npm's timing line and `--install-links` made
`npm ls` differ; the watcher's raw `events.txt` embeds the transport's event coalescing (the
assertion is now that the creation was observed); and the harness itself had to `bootstrap root`
before its first volume (the `ConsensusNotInitialized` refusal every fresh daemon gives). The CI
macOS runner's run of 2026-09-16 (no provenance tag there: every other tool identical) showed one
more: git's `count-objects` kilobytes are `st_blocks`, the host filesystem's allocation unit (APFS
rounds each loose object up to 4 KiB; the export reports the bytes held), so the roster compares
git's object count and not the host's blocks.

### 3.4 pjdfstest — LIMITED (unprivileged): 2,429 passed, 2,220 failed, 4,037 needs-root

238 `tests/**/*.t` files at `pjd/pjdfstest@85a8aea` (BSD-2-Clause; tarball SHA-256
`2005cdd8…`), each run with `sh` from a directory inside the mount, 8,686 cases, 210 s
(2026-09-16 UTC), no file past its bound, the binary built without autotools over a `config.h`
the harness probed the way `configure.ac` does (one link test per `AC_CHECK_FUNC`, one compile
test per header and `struct stat` member; the probe must *call* the function — comparing its
address with zero is folded away by clang, which defined `HAVE_LPATHCONF` on Darwin until fixed —
and must fail on glibc's `__stub_` names, as `configure` does). pjdfstest's README requires root;
this run had none (uid 501, sixteen groups), so every case that **root alone could pass** is
counted **needs-root** (4,037), not failed. The classifier (`crates/conformance/src/tap.rs`,
`Runner`) decides that from the case's own line and the caller's identity, never from what came
before it in the file: a uid/gid switch (`-u`/`-g`), the `requires_root` guard, a block or
character `mknod` (root-only on every filesystem; a fifo through `mknod` is not, measured on
this host's APFS), a `chown`/`lchown`/`fchown`/`fchownat` to a uid that is not the caller's or a
group it is not in (POSIX.1-2024 `chown()` under `_POSIX_CHOWN_RESTRICTED`, which the export
advertises), and a `stat`/`lstat`/`fstat`/`fstatat` expectation naming such an owner.

How the numbers moved, so the history is not misread: the 2026-09-14 run counted 3,348 passed,
3,331 failed, 2,007 needs-root. The export then began enforcing POSIX access control
(2026-09-15, §4.6; [docs/bugs/2026-09-15-nfs-export-enforces-no-posix-permissions.md](../bugs/2026-09-15-nfs-export-enforces-no-posix-permissions.md)),
so a `chown` to another owner became the correct `EPERM` instead of a silent success, and under
the old rule (switches and device nodes only) that read as 4,255 failures — 713 of them the
`EPERM`s themselves, the rest their consequences. The rule was made precise (above), not the list
longer; the same outputs re-read under it give 2,225 failed and 4,037 needs-root. The run also
found two real faults, both fixed and re-run: the volume's `drop_link` never marked the inode's
`ctime` (`unlink/00.t`, `rename/23.t`;
[docs/bugs/2026-09-15-dropping-a-link-leaves-the-inodes-ctime.md](../bugs/2026-09-15-dropping-a-link-leaves-the-inodes-ctime.md)),
and the `LINK` reply's `linkdir_wcc` carried the directory's pre-link times, which the client's
one-second attribute cache then served (`link/00.t`;
[docs/bugs/2026-09-15-nfs-link-reply-carries-the-directorys-pre-link-times.md](../bugs/2026-09-15-nfs-link-reply-carries-the-directorys-pre-link-times.md)).
Earlier in the same day's runs, `truncate/12.t`'s 999,999,999,999,999-byte extension took the
daemon down at the D-18 barrier (109 aborts, `ETIMEDOUT` from file 79 on; fixed in
[docs/bugs/2026-09-15-recovery-image-materializes-a-sparse-files-holes.md](../bugs/2026-09-15-recovery-image-materializes-a-sparse-files-holes.md)),
which is why this run's 238 files all completed.

The 2,220 failures by shape (the record's note carries the 12 most frequent of 182; `cargo xtask
conformance tally --outputs <scratch>/pjdfstest-output` prints them, and the counts by file,
from any kept run — this host's `--keep` scratch or the CI lane's `pjdfstest-output-<os>`
artifact — under `--privilege root|unprivileged`):

| Cases | Shape (names folded to `N`, inode numbers to `<inode>`) | Cause | Standing |
|---|---|---|---|
| 634 | `lstat N/N inode, expected ENOENT, got <inode>` | `rename/09.t` and `10.t`, the sticky-directory suites: a rename by a switched user (needs-root) never ran, so the source is still there | a consequence the case's own line cannot show; judged by the root run |
| 424 | `unlink N/N` / `unlink N, expected 0, got ENOENT` | the name was a fifo, socket or device node that was never made | knock-on of the declared limit below and of the device-node rule |
| 310 | `create`/`symlink`/`mkfifo … got EEXIST`, `bind … got EADDRINUSE`, `chmod N 06555 … got ENOENT` (65, 65, 60, 60, 60) | the same sticky-directory suites: the name the failed rename should have vacated is still taken; the set-id file a root-only step should have made is absent | consequences, as above |
| 130 | `test_check` (a bare `not ok`: two values the script read, compared) | ctime comparisons after a root-only `chown` (`chown/00.t`, 62) or on the fifo/block/char/socket variants of the ctime checks (`unlink/00.t` 16, `link/00.t` 16, `mknod/*` 15, `chmod/00.t` 8, `rename/23.t` 4, …); the regular-file and symlink variants pass since the two ctime fixes | consequences |
| 119 | `mkfifo N/N 0644, expected 0, got EIO`; `bind N/N, expected 0, got EIO` | `MKNOD` answered with the typed `NFS3ERR_NOTSUPP` (slates creates no special nodes, by design); the macOS client presents it as `EIO` | a declared limit of the transport, [docs/bugs/2026-09-14-nfs-special-files-refused-as-eio.md](../bugs/2026-09-14-nfs-special-files-refused-as-eio.md) |
| 60 | `lstat N/N inode, expected <inode>, got <inode>` | the sticky suites again: the expected inode was captured from a step that had already gone wrong | consequences |
| 543 | the 170 remaining shapes, at most 37 cases each | the same three sources (the sticky suites, special-file knock-ons, root-only chown knock-ons), plus one genuine NFS semantic: `unlink/14.t:4` — `open`, `unlink`, `fstat nlink` expects 0 and gets 1, the NFS client's silly-rename of an open unlinked file (EQUIVALENCE.md §8 "unlink-open lifetime") | to be reviewed case by case from the root run's outputs; the silly-rename case is a candidate for the reviewed list with that reason |

No case is listed as expected yet (`docs/wip/conformance/expected-failures/`); the cell says
"0 expected, 2,220 unexpected" rather than hide the shapes behind a generated list, because
GAPS.md §8i forbids satisfying the POSIX contract by expanding an expected-failure list. The root
run the README defines is the CI macOS lane's (`sudo -n` is refused on this host); its per-file
outputs are uploaded as the `pjdfstest-output-macos-latest` artifact and are the input to the
first review through `tally --privilege root`, after which the list only shrinks
(`expected::judge`: an unlisted failure fails the run, a listed case that passes must be struck;
the record names the list's BLAKE3 so an edit without a re-run is caught).

### 3.5 Hermeticity — SKIPPED(privilege) here; wired for the lanes

The static half of R1 is `cargo xtask structural` (no write-capable syscall links outside
`slates-land`). The dynamic half is the tracer run: the whole lifecycle — anchor, daemon, volume,
kernel mount, a workload through the mount (`printf`, `mkdir`, `ln -s`, `mv`, `chmod`, `rm`), a
snapshot, `land` (presented), `slates grant` with the anchor handoff the daemon's own children get
(taken from the daemon's environment; on Linux the memfd is reopened through `/proc/<pid>/fd` and
inherited), `land --grant` — under a filesystem-write tracer, every write-capable call placed in a
closed taxonomy: inside the granted target (matched to the landed entries and the engine's
`.slates-` hidden siblings), a RAM-only kernel object (memfd, `shm_open`, socket, pipe, event
descriptor, the FUSE device), the processes' own standard streams, unresolved (the tracer printed
no path), or outside — a violation. On macOS `fs_usage` needs root and this host has no
passwordless sudo (`sudo -n true` is refused), so the cell is a typed privilege skip; on the CI
macOS runner it runs as `sudo fs_usage -w -f filesys -f network <daemon pid>` (scoped to the
daemon — the only writer by design — because fs_usage cannot tell same-named processes apart and
prints `openat` paths as `[dirfd]/name`, which the parser resolves through a descriptor table);
on Linux as `strace -f -y -qq -s 0 -e trace=%file,write,pwrite64,…` wrapping the anchor so every
child is in one log. Both parsers are tested against the documented row layouts (Apple's
`fs_usage.c` `print_open`/`format_print`; `strace(1)` `-y`), with hostile input; a live run has
not yet been read by either — the first lane run is the first.


> **Runner identity correction (2026-09-17).** Ubuntu job 105312670403 ran pjdfstest as uid 1001
> while reporting root: passwordless sudo availability was mistaken for the caller's effective
> identity, suppressing elevation. The invocation now derives elevation from the actual caller,
> and TAP classification, expected-failure selection and the result record use that invocation's
> identity. Other suites report their workload's identity independently of mount/tracing helpers.
> The dispatch regression fails before the fix and passes afterward; no native conformance
> rerun or closure of the 6202 reported failures is claimed. No expected-failure list changed.
> Record: `docs/bugs/2026-09-17-conformance-confuses-available-and-effective-root.md`.

## 4. The lanes and the other transports

| Transport | How the lane drives it | Standing |
|---|---|---|
| native macOS (NFS loopback) | `slates mount` as shipped; runs here (unprivileged) and in the `conformance` job on `macos-latest` (passwordless sudo: pjdfstest as root, `fs_usage`) | records above |
| native Linux (FUSE) | **no daemon transport serves the FUSE bridge**: `crates/server` links `slates-bridge-fuse` only as a dev-dependency, `slates mount` is `mount_nfs`-only (macOS/BSD), and the crate's `channel::serve_blocking` + `mount::mount` are driven by no test, example or CI step. The lane therefore reaches the daemon's NFSv3 export through a root `mount -t nfs` by the Linux NFS client and records every cell **LIMITED** with that adapter and "the FUSE bridge itself" as not covered; the FUSE cells become RAN only when the daemon serves `/dev/fuse` and the harness mounts it | wired; not yet run (the job has not executed) |
| native Windows (WinFsp) | pjdfstest and fsstress do not apply (POSIX C suites); fsx needs the WinFsp port and a harness mount step; the workloads need a harness mount step; no ETW tracer. The live WinFsp mount test (`crates/bridge-winfsp/tests/mount.rs`, `WINFSP_TEST_MOUNT=1`) proves create/write/read/list/delete through the kernel and nothing more | SKIPPED, typed |
| virtio-fs guest | no live Linux guest; the device half is proven only by the simulated guest driver (`docs/wip/virtiofs.md`), and AC-9.7 says a simulation cannot close the guarantee | SKIPPED(owed) |
| OCI container | the OCI handoff (the attach verb's container form) is under construction; no attachment form to drive | SKIPPED(owed) |

The pressure and failure suites (Part 6 "Soak and scale", "Fault injection on real processes")
have no runnable form in the harness yet; every transport's two cells say so.

## 5. Running it

```
cargo xtask conformance plan                      # skip records for every cell this host cannot run
cargo xtask conformance run --suite fsx           # one suite over this host's native transport
cargo xtask conformance all                       # plan, then every runnable suite, continuing past failures
cargo xtask conformance matrix --write            # regenerate §2 from the tracked records
cargo test -p slates-conformance --test matrix    # the doc-truth tests
```

`--records DIR` (default `docs/wip/conformance/records`), `--scratch DIR` (default a fresh
`mktemp -d`; removed unless `--keep`), the bounds `--fsx-ops/--fsx-seed/--fsx-length`,
`--fsstress-ops/--fsstress-procs/--fsstress-seed` (defaults are `Shape:` constants in
`xtask/src/conformance/mod.rs` and are recorded in each record's `bound`). The suite sources are
fetched from pinned commits into the scratch and verified by SHA-256 before `cc` builds them
(`xtask/src/conformance/fetch.rs`); nothing is vendored (fsx is APSL 2.0, fsstress GPL-2.0 —
licences do not enter this MIT tree by a harness's hand) and nothing is installed. A suite that
does not meet its verdict exits non-zero after writing its record, so `all` on this host exits
non-zero today (pjdfstest's unreviewed failures, the workload differences); the lane does the
same, honestly, until the reviews land.

## 6. Owed

- The first lane runs (Linux adapter, macOS root): their records, copied in, and the first
  review of the root pjdfstest list; the macOS lane's sidecar behaviour (environmental).
- A daemon transport for the FUSE bridge, so the Linux cells can be RAN, not LIMITED.
- A WinFsp mount step in the harness (workloads) and the fsx WinFsp port; an ETW tracer.
- A live virtio-fs guest and the OCI attachment form; then their cells.
- Pressure and failure suites (Phase 9).
- Sibling findings for their owners: the volume root directory lists as `root wheel` through the
  mount ([docs/bugs/2026-09-14-volume-root-owned-by-root-wheel.md](../bugs/2026-09-14-volume-root-owned-by-root-wheel.md));
  `slates grant`'s human surface has no documented way for a shell to obtain the anchor handoff
  (the harness reads it from the daemon's environment; a user has no equivalent).
