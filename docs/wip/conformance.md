# Transport conformance evidence (AC-9.7 / T-9.1; GAP-A9-15)

> **First macOS root expectation review (2026-09-26).** The CI macOS lane's root run (run
> 36202635768, macOS 26.6.2) has 6,781 passes and 1,905 failures. The root list now names exactly
> those 1,905 cases:
> - 1,789 are the Linux list's own cases: A-26 device-fixture cascades, and the open-unlink
>   silly-rename.
> - 116 are macOS-only, each traced to its source: NFSv3 PATHCONF carries no PATH_MAX (74); two
>   behaviours POSIX leaves optional, which pjdfstest marks `todo Linux` (14); the owner's truncate
>   override every NFSv3 server grants (2); the macOS client's fifo open (10); and the macOS kernel's rename authorization (16), whose
>   RENAMEs the server accepts in two new tests.
>
> Judged against the list, that run's outputs have zero unlisted failures and zero listed cases now
> passing or absent. §3.4's unprivileged shapes are unchanged.
> [Review](../bugs/2026-09-26-macos-pjdfstest-root-review.md).

> **First root expectation review (2026-09-20).** The complete unchanged Linux NFS-adapter
> run has 6,970 passes, 1,800 failures and 28 TODO cases. The first root review left open
> in §3.4 now names exactly those 1,800 cases: deliberately refused block/character-device
> fixtures and their source-verified consequences (1,798), plus Linux NFS open-unlink and
> 32-bit timestamp limits (2). A diagnostic isolation of device loops validates the causal
> split; it is not counted as conformance. The complete unchanged suite passes the reviewed
> gate in 174.353 s: zero unexpected failures, zero listed cases now passing or absent.
> fsx, fsstress, all nine workloads and hermeticity also pass. Records remain LIMITED,
> and native FUSE coverage remains owed.
> [Review and guard](../bugs/2026-09-20-pjdfstest-device-fixture-cascades.md).


> **Repair (2026-09-19).** The Linux root-NFS adapter now acquires a durable host-mount
> attachment through `Client::attach_mount`, as the macOS CLI already does, and presents its
> capability in the export path (§4.13, AUD-01). Its guard detaches after unmount or a failed
> mount attempt; helper errors redact the token. The real-daemon socket regression passes on
> macOS and Linux without a privileged mount. The failing CI job's separate traced-startup
> timeout was caused by ptrace stops on unselected allocator syscalls. The tracer now uses
> `--seccomp-bpf` with the same filesystem-write selection and explicit `--kill-on-exit`.
> Its live Linux regression proves startup without a restart and observes real file mutations;
> the Linux conformance lane runs both regressions before its mounted suites. Full mounted
> results now pass under the reviewed limits above. Records: `docs/bugs/2026-09-19-linux-conformance-mount-authority.md`,
> `docs/bugs/2026-09-19-hermeticity-tracer-stops-unselected-syscalls.md`.

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
| native macOS (NFS loopback) | LIMITED(adapter: a non-root run; not covered: 4037 cases only root could pass (a uid/gid switch, a device node, a chown to another owner, or an expectation of one), counted as needs-root rather than run; 238 files, 8686 cases: 2429 passed, 2220 failed (0 expected, 2220 unexpected, 0 listed-now-passing), 4037 needs-root, 0 todo; 2026-09-16; macOS 26.4.1 (25E253)) | RAN(10000 operations, seed 1, file length 262144: passed; 2026-09-14; macOS 26.4.1 (25E253)) | RAN(500 operations × 4 processes, seed 1, 2000 logged, disabled: dread,dwrite: passed; 2026-09-14; macOS 26.4.1 (25E253)) | RAN(identical: cargo, npm, rg, sqlite; DIFFERS: git, python, rsync, editor; skipped: watcher (fswatch absent); 2026-09-16; macOS 26.4.1 (25E253)) | RAN(204 write-capable calls: 60 inside the granted target (12 matched to Written, 0 unmatched), 49 RAM-only objects, 95 standard streams, 0 unresolved, 0 outside — no outside writes; 2026-09-26; macOS 26.4.1 (25E253)) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) |
| native Linux (FUSE) | LIMITED(adapter: a root `mount -t nfs` by the Linux NFS client of the unprivileged daemon's NFSv3 loopback export (the same serving code as macOS); not covered: the FUSE bridge itself (crates/bridge-fuse): no daemon transport serves /dev/fuse (`crates/server` links the crate only as a dev-dependency) and `slates mount` is `mount_nfs`-only; 238 files, 8798 cases: 6970 passed, 1800 failed (1800 expected, 0 unexpected, 0 listed-now-passing), 0 needs-root, 28 todo; 2026-09-20; Debian GNU/Linux 13 (trixie)) | LIMITED(adapter: a root `mount -t nfs` by the Linux NFS client of the unprivileged daemon's NFSv3 loopback export (the same serving code as macOS); not covered: the FUSE bridge itself (crates/bridge-fuse): no daemon transport serves /dev/fuse (`crates/server` links the crate only as a dev-dependency) and `slates mount` is `mount_nfs`-only; 10000 operations, seed 1, file length 262144: passed; 2026-09-20; Debian GNU/Linux 13 (trixie)) | LIMITED(adapter: a root `mount -t nfs` by the Linux NFS client of the unprivileged daemon's NFSv3 loopback export (the same serving code as macOS); not covered: the FUSE bridge itself (crates/bridge-fuse): no daemon transport serves /dev/fuse (`crates/server` links the crate only as a dev-dependency) and `slates mount` is `mount_nfs`-only; 500 operations × 4 processes, seed 1, 2000 logged: passed; 2026-09-20; Debian GNU/Linux 13 (trixie)) | LIMITED(adapter: a root `mount -t nfs` by the Linux NFS client of the unprivileged daemon's NFSv3 loopback export (the same serving code as macOS); not covered: the FUSE bridge itself (crates/bridge-fuse): no daemon transport serves /dev/fuse (`crates/server` links the crate only as a dev-dependency) and `slates mount` is `mount_nfs`-only; identical: git, cargo, npm, python, rg, rsync, sqlite, editor, watcher; 2026-09-20; Debian GNU/Linux 13 (trixie)) | LIMITED(adapter: a root `mount -t nfs` by the Linux NFS client of the unprivileged daemon's NFSv3 loopback export (the same serving code as macOS); not covered: the FUSE bridge itself (crates/bridge-fuse): no daemon transport serves /dev/fuse (`crates/server` links the crate only as a dev-dependency) and `slates mount` is `mount_nfs`-only; 199 write-capable calls: 22 inside the granted target (6 matched to Written, 0 unmatched), 89 RAM-only objects, 88 standard streams, 0 unresolved, 0 outside — no outside writes; 2026-09-20; Debian GNU/Linux 13 (trixie)) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) |
| native Linux (NFSv4.2) | RAN(238 files, 8798 cases: 6971 passed, 1799 failed (1799 expected, 0 unexpected, 0 listed-now-passing), 0 needs-root, 28 todo; 2026-09-27; Debian GNU/Linux 13 (trixie)) | RAN(10000 operations, seed 1, file length 262144: passed; 2026-09-27; Debian GNU/Linux 13 (trixie)) | RAN(500 operations × 4 processes, seed 1, 2000 logged: passed; 2026-09-27; Debian GNU/Linux 13 (trixie)) | RAN(identical: git, cargo, npm, python, rg, rsync, sqlite, editor, watcher; 2026-09-27; Debian GNU/Linux 13 (trixie)) | RAN(239 write-capable calls: 22 inside the granted target (6 matched to Written, 0 unmatched), 119 RAM-only objects, 98 standard streams, 0 unresolved, 0 outside — no outside writes; 2026-09-27; Debian GNU/Linux 13 (trixie)) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) |
| native Windows (WinFsp) | SKIPPED(not applicable: a POSIX C suite with no Windows build (pjdfstest and fsstress use fork, uid switching and POSIX namespace calls)) | SKIPPED(owed: the WinFsp fsx port (Phase 4 task 5) is not wired into the harness, and the harness has no WinFsp mount step) | SKIPPED(not applicable: a POSIX C suite with no Windows build (pjdfstest and fsstress use fork, uid switching and POSIX namespace calls)) | SKIPPED(owed: the harness has no WinFsp mount step; the live WinFsp mount test (crates/bridge-winfsp/tests/mount.rs, gated WINFSP_TEST_MOUNT=1 on windows-latest) proves create/write/read/list/delete through the kernel and nothing more) | SKIPPED(owed: no ETW filesystem-write tracer is wired for Windows) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) |
| virtio-fs guest | SKIPPED(owed: a live Linux guest mounts the tag through QEMU's vhost-user-fs-pci and runs the workload roster (crates/server/tests/virtiofs.rs), but this suite has no guest leg yet; AC-9.7 asks it run in the guest) | SKIPPED(owed: a live Linux guest mounts the tag through QEMU's vhost-user-fs-pci and runs the workload roster (crates/server/tests/virtiofs.rs), but this suite has no guest leg yet; AC-9.7 asks it run in the guest) | SKIPPED(owed: a live Linux guest mounts the tag through QEMU's vhost-user-fs-pci and runs the workload roster (crates/server/tests/virtiofs.rs), but this suite has no guest leg yet; AC-9.7 asks it run in the guest) | SKIPPED(owed: the roster runs in a live Linux guest through QEMU's vhost-user-fs-pci, each workload compared between slates and the guest's RAM — all nine identical on 2026-10-01 (crates/server/tests/virtiofs.rs a_live_guest_runs_the_roster_workloads_identically_on_slates_and_on_its_ram, gated on QEMU and a guest kernel); this harness has no VMM leg (a vhost-user device is attached in-process), so the cell is not run here) | SKIPPED(owed: a live Linux guest mounts the tag through QEMU's vhost-user-fs-pci and runs the workload roster (crates/server/tests/virtiofs.rs), but this suite has no guest leg yet; AC-9.7 asks it run in the guest) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) |
| OCI container | LIMITED(adapter: a non-root run; not covered: 3691 cases only root could pass (a uid/gid switch, a device node, a chown to another owner, or an expectation of one), counted as needs-root rather than run; 238 files, 8798 cases: 2315 passed, 2764 failed (0 expected, 2764 unexpected, 0 listed-now-passing), 3691 needs-root, 28 todo; 2026-10-01; macOS 26.4.1 (25E253)) | RAN(10000 operations, seed 1, file length 262144: passed; 2026-10-01; macOS 26.4.1 (25E253)) | RAN(500 operations × 4 processes, seed 1, 2000 logged: passed; 2026-10-01; macOS 26.4.1 (25E253)) | RAN(identical: git, cargo, python; skipped: npm (npm absent), rg (rg absent), rsync (rsync absent), sqlite (sqlite3 absent), editor (vim absent), watcher (inotifywait absent); 2026-10-01; macOS 26.4.1 (25E253)) | SKIPPED(owed: the container form exists — `attach` returns a verified non-recursive private bind (the source checked again by `slates oci-check`), and fsx, fsstress, the workloads and pjdfstest run inside a container through it on the macOS lane under the runtime handshake (`slates oci-runtime docker`); the hermeticity container leg runs on Linux (`oci-linux`, a root tracer over a Docker Engine bound to the daemon's shared FUSE mount), and on macOS it is owed: its tracer (`eslogger`) needs root, which this lane's harness never takes) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) |
| OCI container (Linux engine) | RAN(238 files, 8798 cases: 6972 passed, 1798 failed (1798 expected, 0 unexpected, 0 listed-now-passing), 0 needs-root, 28 todo; 2026-10-01; Debian GNU/Linux 13 (trixie)) | RAN(10000 operations, seed 1, file length 262144: passed; 2026-10-01; Debian GNU/Linux 13 (trixie)) | RAN(500 operations × 4 processes, seed 1, 2000 logged: passed; 2026-10-01; Debian GNU/Linux 13 (trixie)) | RAN(identical: git, cargo, python; skipped: npm (npm absent), rg (rg absent), rsync (rsync absent), sqlite (sqlite3 absent), editor (vim absent), watcher (inotifywait absent); 2026-10-01; Debian GNU/Linux 13 (trixie)) | RAN(353 write-capable calls: 25 inside the granted target (6 matched to Written, 0 unmatched), 222 RAM-only objects, 106 standard streams, 0 unresolved, 0 outside — no outside writes; 2026-10-01; Debian GNU/Linux 13 (trixie)) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) | SKIPPED(owed: no pressure or failure suite exists in the harness: Part 6's 'Fault injection on real processes' and 'Soak and scale' are Phase 9 work and have no runnable form yet) |

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
- **native macOS (NFS loopback) × hermeticity** (2026-09-26, macOS 26.4.1 (25E253), Darwin 25.4.0 arm64): `sudo eslogger open close create write truncate rename unlink link clone copyfile exchangedata setextattr deleteextattr setmode setowner setflags utimes setattrlist setacl (events of the slates binary); slates anchor/volume create/mount, `sh -c 'printf 'one\n' > f1 && mkdir d && printf 'two\n' > d/f2 && ln -s f3 link && mv f1 f3 && chmod u=rw,go=r f3 && printf 'gone\n' > tmp && rm tmp'`, snapshot, land, grant, land --grant`; bound: one lifecycle: one volume, one mount; known workload bytes/kinds and the entire mounted tree, including client metadata, landed and verified.
  - tracer: sudo eslogger open close create write truncate rename unlink link clone copyfile exchangedata setextattr deleteextattr setmode setowner setflags utimes setattrlist setacl (kept: events of the slates binary); 204 events parsed from 1092 log lines; landing reported 12 written; 8 hidden siblings (.slates-*) seen inside the target
  - the granted target: /Users/adalundhe/slates-herm/scratch3/land-target
  - volume quota: 131072 bytes, derived from 8 peak workload entries × 16384-byte host page
  - landing completeness: 12/12 entries reported; disk verification: Ok(())
  - eslogger: 1092 slates events kept, 0 dropped (global_seq_num continuous); stopped after a drain marker
- **native Linux (FUSE) × POSIX conformance (pjdfstest)** (2026-09-20, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `sudo -n sh <each of 238 tests/**/*.t of pjd/pjdfstest@85a8aea> in a directory inside the mount; binary <scratch>/tools/pjdfstest-85a8aea9e685999ef0540392fd80535f873d7ff7/pjdfstest`; bound: every test file at the pinned commit (238 files), 300s per file.
  - pjdfstest: pjd/pjdfstest@85a8aea (BSD-2-Clause), tarball sha256 2005cdd83b76204177cf136792b1f2058a7418b4fc5b203f51274a698547d754, built `cc -O2 -w -I. pjdfstest.c` over a config.h the harness probed (HAVE_: FACCESSAT FCHMODAT FCHOWNAT FSTATAT LCHMOD LINKAT MKDIRAT MKFIFOAT MKNODAT OPENAT POSIX_FALLOCATE READLINKAT RENAMEAT SYMLINKAT UTIMENSAT SYS_SYSMACROS_H STRUCT_STAT_ST_ATIM STRUCT_STAT_ST_CTIM STRUCT_STAT_ST_MTIM)
  - 1800 expected failures; 0 unlisted failures; 0 listed now passing; 0 listed absent
  - the suite volume is 190MiB: 512MiB was refused BudgetExceeded with 265814016 bytes available on the daemon's shard, so it was sized to three quarters of that
  - failures by shape (12 of 207 shapes; names folded to N, inode numbers to <inode>): 180 × lstat N/N inode, expected ENOENT, got <inode>; 150 × lchown N/N 65534 65534, expected 0, got ENOENT; 120 × mknod N/N b 0644 1 2, expected 0, got 527; 119 × mknod N/N c 0644 1 2, expected 0, got 527; 92 × unlink N/N, expected 0, got ENOENT; 80 × unlink N, expected 0, got ENOENT; 76 × -u 65534 -g 65534 rename N/N N/N, expected EACCES|EPERM, got ENOENT; 52 × test_check (a comparison of two values the script read; no message); 40 × -u 65534 -g 65534 rename N/N N/N, expected 0, got ENOENT; 40 × mknod N b 0644 1 2, expected 0, got 527; 38 × mknod N c 0644 1 2, expected 0, got 527; 36 × lstat N/N inode,uid,gid, expected ENOENT,65534,65534, got <inode>,65534,65534
- **native Linux (FUSE) × fsx** (2026-09-20, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `cd <mount>/conformance-<pid>/fsx && <scratch>/tools/fsx -N 10000 -S 1 -l 262144 -q -P <scratch>/fsx-logs fsx.bin`; bound: 10000 operations, seed 1, file length 262144 bytes.
  - fsx: freebsd/freebsd-src@42c6944 tools/regression/fsx/fsx.c (APSL 2.0), sha256 b064208bec8519e80038ee1da8cb9c0f7c512a3242bbf4c06809a88ce15ae019, built `cc -O2 -w -include time.h -include stdint.h` (source unchanged)
  - the exerciser's .fsxlog/.fsxgood files were kept outside the mount (-P)
  - the suite volume is 190MiB: 512MiB was refused BudgetExceeded with 265814016 bytes available on the daemon's shard, so it was sized to three quarters of that
- **native Linux (FUSE) × fsstress** (2026-09-20, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `<scratch>/tools/ltp/fsstress -d <mount>/conformance-<pid>/fsstress -n 500 -p 4 -s 1 -v`; bound: 500 operations per process × 4 processes, seed 1.
  - fsstress: linux-test-project/ltp@6af38cf testcases/kernel/fs/fsstress/fsstress.c (GPL-2.0), sha256 9a80bbe1f1ad933845b9272b5057776e74923503bbb6f392bf1e135c1744d644, built `cc -O2 -w -DNO_XFS -D_GNU_SOURCE -include shim/config.h` over the harness's shim config.h
  - the daemon answered `volume list` after the run: true
  - the suite volume is 190MiB: 512MiB was refused BudgetExceeded with 265814016 bytes available on the daemon's shard, so it was sized to three quarters of that
- **native Linux (FUSE) × workloads** (2026-09-20, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `sh -c '<roster script>' in <host scratch>/<tool> and <mount>/<tool>, per tool of crates/conformance/src/workload.rs ROSTER; trees compared by manifest`; bound: 9 tools run, one script each.
  - the volume was created with --fold=false to match the host scratch's name policy; git identity and dates fixed; sqlite busy timeout 5000 ms; watcher wait 3 s
  - the suite volume is 190MiB: 512MiB was refused BudgetExceeded with 265814016 bytes available on the daemon's shard, so it was sized to three quarters of that
- **native Linux (FUSE) × hermeticity** (2026-09-20, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `strace --seccomp-bpf --kill-on-exit -f -y -qq -s 0 -o trace.log -e trace=%file,write,pwrite64,writev,pwritev,pwritev2,ftruncate,fchmod,fchown,fsync,fdatasync,fallocate,copy_file_range,sendfile,splice,vmsplice -- slates --instance <i> anchor --quick --shards 2; then create, mount, `sh -c 'printf 'one\n' > f1 && mkdir d && printf 'two\n' > d/f2 && ln -s f3 link && mv f1 f3 && chmod u=rw,go=r f3 && printf 'gone\n' > tmp && rm tmp'`, snapshot, land, grant, land --grant`; bound: one lifecycle: one volume, one mount, four workload entries and their two parent directories landed and verified.
  - tracer: strace -f -y -qq -s 0 -e trace=%file,write,pwrite64,writev,pwritev,pwritev2,ftruncate,fchmod,fchown,fsync,fdatasync,fallocate,copy_file_range,sendfile,splice,vmsplice -- <anchor>; 199 events parsed from 427 log lines; landing reported 6 written; 4 hidden siblings (.slates-*) seen inside the target
  - the granted target: /scratch/slates-scratch/land-target
  - volume quota: 2112 bytes, derived from 8 workload inodes
  - landing completeness: 6/6 entries reported; disk verification: Ok(())
- **native Linux (NFSv4.2) × POSIX conformance (pjdfstest)** (2026-09-27, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `sh <each of 238 tests/**/*.t of pjd/pjdfstest@85a8aea> in a directory inside the mount; binary <scratch>/tools/pjdfstest-85a8aea9e685999ef0540392fd80535f873d7ff7/pjdfstest`; bound: every test file at the pinned commit (238 files), 300s per file.
  - pjdfstest: pjd/pjdfstest@85a8aea (BSD-2-Clause), tarball sha256 2005cdd83b76204177cf136792b1f2058a7418b4fc5b203f51274a698547d754, built `cc -O2 -w -I. pjdfstest.c` over a config.h the harness probed (HAVE_: FACCESSAT FCHMODAT FCHOWNAT FSTATAT LCHMOD LINKAT MKDIRAT MKFIFOAT MKNODAT OPENAT POSIX_FALLOCATE READLINKAT RENAMEAT SYMLINKAT UTIMENSAT SYS_SYSMACROS_H STRUCT_STAT_ST_ATIM STRUCT_STAT_ST_CTIM STRUCT_STAT_ST_MTIM)
  - 1799 expected failures; 0 unlisted failures; 0 listed now passing; 0 listed absent
  - failures by shape (12 of 206 shapes; names folded to N, inode numbers to <inode>): 180 × lstat N/N inode, expected ENOENT, got <inode>; 150 × lchown N/N 65534 65534, expected 0, got ENOENT; 120 × mknod N/N b 0644 1 2, expected 0, got 527; 119 × mknod N/N c 0644 1 2, expected 0, got 527; 92 × unlink N/N, expected 0, got ENOENT; 80 × unlink N, expected 0, got ENOENT; 76 × -u 65534 -g 65534 rename N/N N/N, expected EACCES|EPERM, got ENOENT; 52 × test_check (a comparison of two values the script read; no message); 40 × -u 65534 -g 65534 rename N/N N/N, expected 0, got ENOENT; 40 × mknod N b 0644 1 2, expected 0, got 527; 38 × mknod N c 0644 1 2, expected 0, got 527; 36 × lstat N/N inode,uid,gid, expected ENOENT,65534,65534, got <inode>,65534,65534
- **native Linux (NFSv4.2) × fsx** (2026-09-27, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `cd <mount>/conformance-<pid>/fsx && <scratch>/tools/fsx -N 10000 -S 1 -l 262144 -q -P <scratch>/fsx-logs fsx.bin`; bound: 10000 operations, seed 1, file length 262144 bytes.
  - fsx: freebsd/freebsd-src@42c6944 tools/regression/fsx/fsx.c (APSL 2.0), sha256 b064208bec8519e80038ee1da8cb9c0f7c512a3242bbf4c06809a88ce15ae019, built `cc -O2 -w -include time.h -include stdint.h` (source unchanged)
  - the exerciser's .fsxlog/.fsxgood files were kept outside the mount (-P)
- **native Linux (NFSv4.2) × fsstress** (2026-09-27, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `<scratch>/tools/ltp/fsstress -d <mount>/conformance-<pid>/fsstress -n 500 -p 4 -s 1 -v`; bound: 500 operations per process × 4 processes, seed 1.
  - fsstress: linux-test-project/ltp@6af38cf testcases/kernel/fs/fsstress/fsstress.c (GPL-2.0), sha256 9a80bbe1f1ad933845b9272b5057776e74923503bbb6f392bf1e135c1744d644, built `cc -O2 -w -DNO_XFS -D_GNU_SOURCE -include shim/config.h` over the harness's shim config.h
  - the daemon answered `volume list` after the run: true
- **native Linux (NFSv4.2) × workloads** (2026-09-27, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `sh -c '<roster script>' in <host scratch>/<tool> and <mount>/<tool>, per tool of crates/conformance/src/workload.rs ROSTER; trees compared by manifest`; bound: 9 tools run, one script each.
  - the volume was created with --fold=false to match the host scratch's name policy; git identity and dates fixed; sqlite busy timeout 5000 ms; watcher wait 3 s
- **native Linux (NFSv4.2) × hermeticity** (2026-09-27, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `strace --seccomp-bpf --kill-on-exit -f -y -qq -s 0 -o trace.log -e trace=%file,write,pwrite64,writev,pwritev,pwritev2,ftruncate,fchmod,fchown,fsync,fdatasync,fallocate,copy_file_range,sendfile,splice,vmsplice -- slates --instance <i> anchor --quick --shards 2; then create, mount, `sh -c 'printf 'one\n' > f1 && mkdir d && printf 'two\n' > d/f2 && ln -s f3 link && mv f1 f3 && chmod u=rw,go=r f3 && printf 'gone\n' > tmp && rm tmp'`, snapshot, land, grant, land --grant`; bound: one lifecycle: one volume, one mount; known workload bytes/kinds and the entire mounted tree, including client metadata, landed and verified.
  - tracer: strace -f -y -qq -s 0 -e trace=%file,write,pwrite64,writev,pwritev,pwritev2,ftruncate,fchmod,fchown,fsync,fdatasync,fallocate,copy_file_range,sendfile,splice,vmsplice -- <anchor>; 239 events parsed from 862 log lines; landing reported 6 written; 4 hidden siblings (.slates-*) seen inside the target
  - the granted target: /tmp/conformance-scratch/native-linux-nfs4/land-target
  - volume quota: 32768 bytes, derived from 8 peak workload entries × 4096-byte host page
  - landing completeness: 6/6 entries reported; disk verification: Ok(())
- **OCI container × POSIX conformance (pjdfstest)** (2026-10-01, macOS 26.4.1 (25E253), Darwin 25.4.0 arm64): `docker run --rm --user <uid>:<gid> --mount type=bind,source=<mount>,destination=/work,bind-recursive=disabled,bind-propagation=private --mount type=bind,source=<scratch>/tools/pjd-linux,destination=/src,readonly rust:1.98.0 sh -c cp -R /src/pjdfstest-85a8aea9e685999ef0540392fd80535f873d7ff7 /tmp/p && cd /tmp/p && cat /src/probes/head.h > config.h && for c in /src/probes/*.c; do b=${c%.c}; cc -std=gnu17 -w $(cat $b.flags) -o /tmp/probe.out $c >/dev/null 2>&1 && cat $b.define >> config.h; done; cc -O2 -w -I. -o pjdfstest pjdfstest.c && cd /work/conformance-<pid>/pjd && for f in $(cd /tmp/p && find tests -name '*.t' | sort); do echo '@@@slates-pjdfstest '$f; timeout 300 sh /tmp/p/$f 2>/dev/null; [ $? -eq 124 ] && echo 'TIMED OUT'; done`; bound: every test file at the pinned commit (238 files), 300s per file.
  - pjdfstest: pjd/pjdfstest@85a8aea at 85a8aea9e685999ef0540392fd80535f873d7ff7, compiled inside the container with config.h answered by its own compiler from the harness's 33 probes
  - 0 expected failures; 2764 unlisted failures (tests/chmod/00.t:22, tests/chmod/00.t:23, tests/chmod/00.t:24, tests/chmod/00.t:26, tests/chmod/00.t:27, tests/chmod/00.t:28, tests/chmod/00.t:31, tests/chmod/00.t:33 … and 2756 more); 0 listed now passing; 0 listed absent
  - the runtime's profile (`slates oci-runtime docker`): profile: engine="Docker Desktop" version=29.3.1 endpoint=unix://~/.docker/run/docker.sock rootless=false userns=false; evidence: T-4.13 (Docker Desktop on macOS over its local socket, binding the host mount through Desktop's file sharing); identity: host_user_through_share (container ids are not forwarded; the attachment's capability is the authority); hard_links: other_names_stale_after_the_first_is_removed (until the guest revalidates; git: core.createObject=rename)
  - the source checked again (`slates oci-check`, mount 2 on device 436207711) just before the bind
  - failures by shape (12 of 217 shapes; names folded to N, inode numbers to <inode>): 650 × lstat N/N inode, expected ENOENT, got EACCES; 287 × unlink N/N, expected 0, got EACCES; 211 × unlink N, expected 0, got EACCES; 140 × test_check (a comparison of two values the script read; no message); 110 × bind N/N, expected 0, got EACCES; 110 × mkfifo N/N 0644, expected 0, got EACCES; 99 × symlink test N/N, expected 0, got EACCES; 90 × chmod N 06555, expected 0, got EACCES; 73 × create N/N 0644, expected 0, got EACCES; 54 × rmdir N, expected 0, got ENOTEMPTY; 44 × lstat N/N type, expected ENOENT, got EACCES; 41 × mkfifo N 0644, expected 0, got EACCES
- **OCI container × fsx** (2026-10-01, macOS 26.4.1 (25E253), Darwin 25.4.0 arm64): `docker run --rm --user <uid>:<gid> --mount type=bind,source=<mount>,destination=/work,bind-recursive=disabled,bind-propagation=private --mount type=bind,source=<scratch>/tools,destination=/src,readonly rust:1.98.0 sh -c cc -O2 -w -include time.h -include stdint.h -o /tmp/fsx /src/fsx.c && cd /work/conformance-<pid>/fsx && /tmp/fsx -N 10000 -S 1 -l 262144 -q -P /tmp fsx.bin`; bound: 10000 operations, seed 1, file length 262144 bytes.
  - fsx: freebsd/freebsd-src@42c6944 tools/regression/fsx/fsx.c (APSL 2.0), sha256 b064208bec8519e80038ee1da8cb9c0f7c512a3242bbf4c06809a88ce15ae019, built `cc -O2 -w -include time.h -include stdint.h` (source unchanged)
  - the runtime's profile (`slates oci-runtime docker`): profile: engine="Docker Desktop" version=29.3.1 endpoint=unix://~/.docker/run/docker.sock rootless=false userns=false; evidence: T-4.13 (Docker Desktop on macOS over its local socket, binding the host mount through Desktop's file sharing); identity: host_user_through_share (container ids are not forwarded; the attachment's capability is the authority)
  - the source checked again (`slates oci-check`, mount 2 on device 436207667) just before the bind
  - fsx compiled inside the container from the same pinned source; its .fsxlog/.fsxgood files kept in the container's /tmp
- **OCI container × fsstress** (2026-10-01, macOS 26.4.1 (25E253), Darwin 25.4.0 arm64): `docker run --rm --user <uid>:<gid> --mount type=bind,source=<mount>,destination=/work,bind-recursive=disabled,bind-propagation=private --mount type=bind,source=<scratch>/tools/ltp-linux,destination=/src,readonly rust:1.98.0 sh -c cp -R /src /tmp/ltp && cd /tmp/ltp && cc -O2 -w -DNO_XFS -D_GNU_SOURCE -include shim/config.h -I. -Ishim -o /tmp/fsstress fsstress.c && /tmp/fsstress -d /work/conformance-<pid>/fsstress -n 500 -p 4 -s 1 -v`; bound: 500 operations per process × 4 processes, seed 1.
  - fsstress: linux-test-project/ltp@6af38cf testcases/kernel/fs/fsstress/fsstress.c (GPL-2.0), sha256 9a80bbe1f1ad933845b9272b5057776e74923503bbb6f392bf1e135c1744d644, compiled inside the container `cc -O2 -w -DNO_XFS -D_GNU_SOURCE -include shim/config.h -I. -Ishim` over the harness's Linux shim config.h
  - the runtime's profile (`slates oci-runtime docker`): profile: engine="Docker Desktop" version=29.3.1 endpoint=unix://~/.docker/run/docker.sock rootless=false userns=false; evidence: T-4.13 (Docker Desktop on macOS over its local socket, binding the host mount through Desktop's file sharing); identity: host_user_through_share (container ids are not forwarded; the attachment's capability is the authority)
  - the source checked again (`slates oci-check`, mount 2 on device 436207665) just before the bind
  - the daemon answered `volume list` after the run: true
- **OCI container × workloads** (2026-10-01, macOS 26.4.1 (25E253), Darwin 25.4.0 arm64): `docker run --rm --user <uid>:<gid> --mount <the binding> --mount <host scratch>:/host rust:1.98.0 sh -c '<roster script>' in /host/<tool> and /work/conformance-<pid>/workloads-run/<tool>, per tool of crates/conformance/src/workload.rs ROSTER the image holds; trees compared by manifest`; bound: 3 tools run, one script each.
  - the roster ran in `rust:1.98.0` as the mounting user; the reference side is a host scratch directory bound at /host, so both sides cross the runtime's file sharing; the volume was created with --fold=true; git ran with core.createObject=rename on both sides, as the profile's hard-link rule asks
  - the runtime's profile (`slates oci-runtime docker`): profile: engine="Docker Desktop" version=29.3.1 endpoint=unix://~/.docker/run/docker.sock rootless=false userns=false; evidence: T-4.13 (Docker Desktop on macOS over its local socket, binding the host mount through Desktop's file sharing); identity: host_user_through_share (container ids are not forwarded; the attachment's capability is the authority); hard_links: other_names_stale_after_the_first_is_removed (until the guest revalidates; git: core.createObject=rename)
  - the source checked again (`slates oci-check`, mount 2 on device 436207690) just before the bind
  - git: 20 `.nfs.*` names set aside on the mount side — the NFS client's silly-renames of names removed while the runtime's share held them open (the transport's declared delete-while-open rule, `SillyRenamed`)
  - cargo: 20 `.nfs.*` names set aside on the mount side — the NFS client's silly-renames of names removed while the runtime's share held them open (the transport's declared delete-while-open rule, `SillyRenamed`)
- **OCI container (Linux engine) × POSIX conformance (pjdfstest)** (2026-10-01, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `docker run --rm --user 0:0 --mount type=bind,source=<mount>,destination=/work,bind-recursive=disabled,bind-propagation=private --mount type=bind,source=<scratch>/tools/pjd-linux,destination=/src,readonly rust:1.98.0 sh -c cp -R /src/pjdfstest-85a8aea9e685999ef0540392fd80535f873d7ff7 /tmp/p && cd /tmp/p && cat /src/probes/head.h > config.h && for c in /src/probes/*.c; do b=${c%.c}; cc -std=gnu17 -w $(cat $b.flags) -o /tmp/probe.out $c >/dev/null 2>&1 && cat $b.define >> config.h; done; cc -O2 -w -I. -o pjdfstest pjdfstest.c && cd /work/conformance-<pid>/pjd && for f in $(cd /tmp/p && find tests -name '*.t' | sort); do echo '@@@slates-pjdfstest '$f; timeout 300 sh /tmp/p/$f 2>/dev/null; [ $? -eq 124 ] && echo 'TIMED OUT'; done`; bound: every test file at the pinned commit (238 files), 300s per file.
  - pjdfstest: pjd/pjdfstest@85a8aea at 85a8aea9e685999ef0540392fd80535f873d7ff7, compiled inside the container with config.h answered by its own compiler from the harness's 33 probes
  - 1798 expected failures; 0 unlisted failures; 0 listed now passing; 0 listed absent
  - the runtime's profile (`slates oci-runtime docker`): profile: engine="Debian GNU/Linux 13 (trixie)" version=26.1.5+dfsg1 endpoint=unix:///var/run/docker.sock rootless=false userns=false; evidence: T-4.13 (Linux, a shared FUSE mount) (a Docker Engine on its own Linux host over its local socket, binding the daemon's shared FUSE mount); identity: container_ids_as_host_ids (no remapping; the kernel checks each id's permission bits, container root bypasses them); hard_links: every_name_served_at_once
  - the source checked again (`slates oci-check`, mount 412 on device 196) just before the bind
  - failures by shape (12 of 205 shapes; names folded to N, inode numbers to <inode>): 180 × lstat N/N inode, expected ENOENT, got <inode>; 150 × lchown N/N 65534 65534, expected 0, got ENOENT; 120 × mknod N/N b 0644 1 2, expected 0, got EOPNOTSUPP; 119 × mknod N/N c 0644 1 2, expected 0, got EOPNOTSUPP; 92 × unlink N/N, expected 0, got ENOENT; 80 × unlink N, expected 0, got ENOENT; 76 × -u 65534 -g 65534 rename N/N N/N, expected EACCES|EPERM, got ENOENT; 52 × test_check (a comparison of two values the script read; no message); 40 × -u 65534 -g 65534 rename N/N N/N, expected 0, got ENOENT; 40 × mknod N b 0644 1 2, expected 0, got EOPNOTSUPP; 38 × mknod N c 0644 1 2, expected 0, got EOPNOTSUPP; 36 × lstat N/N inode,uid,gid, expected ENOENT,65534,65534, got <inode>,65534,65534
- **OCI container (Linux engine) × fsx** (2026-10-01, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `docker run --rm --user <uid>:<gid> --mount type=bind,source=<mount>,destination=/work,bind-recursive=disabled,bind-propagation=private --mount type=bind,source=<scratch>/tools,destination=/src,readonly rust:1.98.0 sh -c cc -O2 -w -include time.h -include stdint.h -o /tmp/fsx /src/fsx.c && cd /work/conformance-<pid>/fsx && /tmp/fsx -N 10000 -S 1 -l 262144 -q -P /tmp fsx.bin`; bound: 10000 operations, seed 1, file length 262144 bytes.
  - fsx: freebsd/freebsd-src@42c6944 tools/regression/fsx/fsx.c (APSL 2.0), sha256 b064208bec8519e80038ee1da8cb9c0f7c512a3242bbf4c06809a88ce15ae019, built `cc -O2 -w -include time.h -include stdint.h` (source unchanged)
  - the runtime's profile (`slates oci-runtime docker`): profile: engine="Debian GNU/Linux 13 (trixie)" version=26.1.5+dfsg1 endpoint=unix:///var/run/docker.sock rootless=false userns=false; evidence: T-4.13 (Linux, a shared FUSE mount) (a Docker Engine on its own Linux host over its local socket, binding the daemon's shared FUSE mount); identity: container_ids_as_host_ids (no remapping; the kernel checks each id's permission bits, container root bypasses them); hard_links: every_name_served_at_once
  - the source checked again (`slates oci-check`, mount 412 on device 196) just before the bind
  - fsx compiled inside the container from the same pinned source; its .fsxlog/.fsxgood files kept in the container's /tmp
- **OCI container (Linux engine) × fsstress** (2026-10-01, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `docker run --rm --user <uid>:<gid> --mount type=bind,source=<mount>,destination=/work,bind-recursive=disabled,bind-propagation=private --mount type=bind,source=<scratch>/tools/ltp-linux,destination=/src,readonly rust:1.98.0 sh -c cp -R /src /tmp/ltp && cd /tmp/ltp && cc -O2 -w -DNO_XFS -D_GNU_SOURCE -include shim/config.h -I. -Ishim -o /tmp/fsstress fsstress.c && /tmp/fsstress -d /work/conformance-<pid>/fsstress -n 500 -p 4 -s 1 -v`; bound: 500 operations per process × 4 processes, seed 1.
  - fsstress: linux-test-project/ltp@6af38cf testcases/kernel/fs/fsstress/fsstress.c (GPL-2.0), sha256 9a80bbe1f1ad933845b9272b5057776e74923503bbb6f392bf1e135c1744d644, compiled inside the container `cc -O2 -w -DNO_XFS -D_GNU_SOURCE -include shim/config.h -I. -Ishim` over the harness's Linux shim config.h
  - the runtime's profile (`slates oci-runtime docker`): profile: engine="Debian GNU/Linux 13 (trixie)" version=26.1.5+dfsg1 endpoint=unix:///var/run/docker.sock rootless=false userns=false; evidence: T-4.13 (Linux, a shared FUSE mount) (a Docker Engine on its own Linux host over its local socket, binding the daemon's shared FUSE mount); identity: container_ids_as_host_ids (no remapping; the kernel checks each id's permission bits, container root bypasses them); hard_links: every_name_served_at_once
  - the source checked again (`slates oci-check`, mount 412 on device 196) just before the bind
  - the daemon answered `volume list` after the run: true
- **OCI container (Linux engine) × workloads** (2026-10-01, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `docker run --rm --user <uid>:<gid> --mount <the binding> --mount <host scratch>:/host rust:1.98.0 sh -c '<roster script>' in /host/<tool> and /work/conformance-<pid>/workloads-run/<tool>, per tool of crates/conformance/src/workload.rs ROSTER the image holds; trees compared by manifest`; bound: 3 tools run, one script each.
  - the roster ran in `rust:1.98.0` as the mounting user; the reference side is a host scratch directory bound at /host, so both sides cross the runtime's file sharing; the volume was created with --fold=false; git ran with core.createObject=rename on both sides, as the profile's hard-link rule asks
  - the runtime's profile (`slates oci-runtime docker`): profile: engine="Debian GNU/Linux 13 (trixie)" version=26.1.5+dfsg1 endpoint=unix:///var/run/docker.sock rootless=false userns=false; evidence: T-4.13 (Linux, a shared FUSE mount) (a Docker Engine on its own Linux host over its local socket, binding the daemon's shared FUSE mount); identity: container_ids_as_host_ids (no remapping; the kernel checks each id's permission bits, container root bypasses them); hard_links: every_name_served_at_once
  - the source checked again (`slates oci-check`, mount 412 on device 196) just before the bind
- **OCI container (Linux engine) × hermeticity** (2026-10-01, Debian GNU/Linux 13 (trixie), Linux 6.12.76-linuxkit aarch64): `strace --seccomp-bpf --kill-on-exit -f -y -qq -s 0 -o trace.log -e trace=%file,write,pwrite64,writev,pwritev,pwritev2,ftruncate,fchmod,fchown,fsync,fdatasync,fallocate,copy_file_range,sendfile,splice,vmsplice -- slates --instance <i> anchor --quick --shards 2; then create, mount --shared, attach --oci, oci-check, `docker run --mount <the binding> sh -c 'printf 'one\n' > f1 && mkdir d && printf 'two\n' > d/f2 && ln -s f3 link && mv f1 f3 && chmod u=rw,go=r f3 && printf 'gone\n' > tmp && rm tmp'` inside the bind, snapshot, land, grant, land --grant`; bound: one lifecycle: one volume, one mount; known workload bytes/kinds and the entire mounted tree, including client metadata, landed and verified.
  - tracer: strace -f -y -qq -s 0 -e trace=%file,write,pwrite64,writev,pwritev,pwritev2,ftruncate,fchmod,fchown,fsync,fdatasync,fallocate,copy_file_range,sendfile,splice,vmsplice -- <anchor>; 353 events parsed from 1149 log lines; landing reported 6 written; 4 hidden siblings (.slates-*) seen inside the target
  - 2 calls by an OS mount broker (/usr/bin/fusermount3, /bin/fusermount3, /usr/bin/mount, /bin/mount) set aside, not judged (R10; macOS's eslogger leg keeps only the slates executable's): ["mkdirat /run/mount", "openat /dev/fuse"]
  - the granted target: /target/conformance-scratch-180/oci-linux/land-target
  - the granted landing 0000000000000001: the daemon's traced ids [1804, 1808, 1809], 1790895580468360167 ns to 1790895580487125459 ns (wall clock); hidden names: Ok(2)
  - volume quota: 32768 bytes, derived from 8 peak workload entries × 4096-byte host page
  - landing completeness: 6/6 entries reported; disk verification: Ok(())
  - the runtime's profile (`slates oci-runtime docker`): profile: engine="Debian GNU/Linux 13 (trixie)" version=26.1.5+dfsg1 endpoint=unix:///var/run/docker.sock rootless=false userns=false; evidence: T-4.13 (Linux, a shared FUSE mount) (a Docker Engine on its own Linux host over its local socket, binding the daemon's shared FUSE mount); identity: container_ids_as_host_ids (no remapping; the kernel checks each id's permission bits, container root bypasses them); hard_links: every_name_served_at_once
  - the workload ran inside a container: docker run --rm --user <uid>:<gid> --mount type=bind,source=<mount>,destination=/work,bind-recursive=disabled,bind-propagation=private rust:1.98.0 sh -c cd /work/conformance-<pid>/traced && printf 'one\n' > f1 && mkdir d && printf 'two\n' > d/f2 && ln -s f3 link && mv f1 f3 && chmod u=rw,go=r f3 && printf 'gone\n' > tmp && rm tmp
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

> **AppleDouble sidecars resolved (A-33, 2026-09-26).** The NFSv3 bridge serves `._name` as a view
> of `name`'s extended attributes, so no sidecar reaches a tool. Local run on macOS 26.4.1: git,
> cargo, npm, python, rg, rsync, sqlite and vim are all identical to the host, and the watcher is
> skipped (no `fswatch`). The sidecar row below is kept as the record of what was found. Record:
> `docs/bugs/2026-09-14-nfs-appledouble-sidecars.md`.

> **Ubuntu editor correction (2026-09-17).** Job 105312670403 reported eight identical tools
> and a missing mounted `note.txt~`. The roster now clears Vim's temporary-path `backupskip`
> exclusion and reads the backup into the compared output; missing backups fail the script.
> The real-save regression checks exact new and backup bytes inside and outside TMPDIR, gated
> on supplied RAM scratch and Vim. Local real-save execution awaits RAM-volume authorization;
> no native conformance rerun is claimed. Record:
> `docs/bugs/2026-09-17-editor-backup-depends-on-scratch-path.md`.

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

### 3.4a pjdfstest through the OCI container bind — LIMITED (unprivileged): 2,315 passed, 2,764 failed, 3,691 needs-root

Run 2026-10-01 on macOS 26.4.1 through Docker Desktop 29.3.1. The command was `cargo xtask conformance run --suite
pjdfstest --transport oci --keep`, using every file of pjdfstest at `85a8aea`. The tree is compiled inside
`rust:1.98.0`, with `config.h` answered by the container's own compiler from the harness's 33 probes. The run is
the mounting user (501:20) in a directory inside the verified bind, with the same 300 s per-file bound as the host
lane. 238 files, 8,798 cases. The failures, shaped by cause from the kept outputs:

| Count | Shape | Cause | Standing |
|---|---|---|---|
| ~2,400 | `lstat`/`unlink`/`bind`/`mkfifo`/`symlink`/`create`/`chmod … got EACCES` | **NFS-FIFO-OPEN through the share.** Docker Desktop's server opens every node it makes. The macOS NFS client refuses to open a fifo or socket node `EACCES` in `nfs_vnop_open`, before fifofs runs (the reviewed host-lane reason, `expected-failures/native-macos-nfs.pjdfstest.txt`). So the container's `mkfifo`, `bind` or `mknod` reports `EACCES`, yet the node exists on the host (measured: the host lists `prw-r--r-- f`, the container cannot see it). Every later step of the script that reuses that name is refused | a declared limit of the macOS client, surfaced by Desktop; not a slates defect (the host lane's fifo metadata passes) |
| 55 | `rmdir N, expected 0, got ENOTEMPTY` | a directory holding a node the container cannot see | knock-on of the row above |
| 9 (varying) | `rename/09.t` `lstat … inode` (7) and `rmdir … ENOTEMPTY` (2) | **Not deterministic through Desktop.** Two runs of the same code on 2026-10-01 (2,308/2,771 and 2,315/2,764) differ in exactly these nine cases. All nine come after a step only root may take, which this run is refused. In `rename/09.t`, each `lstat` follows a refused `-u 65534` rename or `chown 65534` and compares inode numbers that differ by exactly one (21869 against 21870), as if one more inode had been allocated earlier in one run than in the other. The two `rmdir`s meet a directory still holding an entry. The same scripts' inode checks are deterministic in the root run through a Linux engine on slates' own FUSE mount (§3.4b, identical across its two runs), where no step is refused and the path has neither Desktop's file-sharing layer nor the macOS NFS client. So the variance lies in this unprivileged run's knock-on chain through those layers. An extra node from the NFS client's silly-rename of a file removed while open would fit the off-by-one, but nothing in the outputs shows one | knock-ons of refused root-only steps, as the last row; the cause of the off-by-one is unproven |
| the rest | `ENOENT`/`EEXIST` knock-ons, `test_check` | as §3.4: steps only root could take never happened | as §3.4 |

Before 2026-10-01 the leg merged each file's stderr into its TAP, so pjdfstest's helper printing `stat returned -1`
for an invisible node, and the engine's image pull, made 19 files malformed. The leg now reads TAP from stdout
alone, as the host lane does, and the rerun has no malformed file.

The record is `records/oci.pjdfstest.json`. The cases root alone could pass are counted needs-root, as on the host
lane. Like the host lane's unprivileged run, nothing is listed case by case: every shape is the declared client
limit or its knock-ons. A Linux engine binding the daemon's FUSE source would not have this limit and has no lane
yet (AUD-29-67/74).

### 3.4b pjdfstest through the Linux container lane — RAN as root: 6,972 passed, 1,798 expected failures

Run 2026-10-01 in the Linux container lane (Debian 13, Docker Engine inside the lane container, a privileged dind
as CI's runner has). The command was `cargo xtask conformance run --suite pjdfstest --transport oci-linux --keep`,
using every file of pjdfstest at `85a8aea`. The session mounts with `slates mount --shared` (`allow_other`), and
the suite runs as container root through the verified bind (`ContainerIdsAsHostIds`: a Linux engine's ids reach
the mount as themselves, root bypassing the bits). So this is a root run, with no case needs-root. It is the
first run of the suite over slates' own FUSE mount; the `native-linux-fuse` lane is an NFSv3 adapter.

The first run matched the root-reviewed Linux list (`expected-failures/native-linux-fuse.pjdfstest.txt`) on 1,798
of its 1,800 cases. Two listed cases passed, and 18 cases failed that the list does not name. All 18 were one
FUSE bug: a component past `NAME_MAX` was refused `EINVAL` (creation) or `ENOENT` (lookup) instead of
`ENAMETOOLONG`. It is fixed on every bridge
(`docs/bugs/2026-10-01-a-long-name-was-einval-or-enoent-not-enametoolong.md`). The rerun's failures are exactly
the 1,798 reviewed cases. `expected-failures/oci-linux.pjdfstest.txt` carries each one with its reason unchanged
(device-fixture cascades; A-26 refuses block and character devices). Its gate is the same as every list's: an
unlisted failure, a listed pass or an absent listed case fails the run. The record is `records/oci-linux.pjdfstest.json`.

### 3.5 Hermeticity — SKIPPED(privilege) here; wired for the lanes

The static half of R1 is two source-level checks, neither a linker proof (AUD-29-31): `cargo xtask
structural` (spelled symbols and expanded `use` trees; globs and root aliases refused) and the resolved-path
lints of `clippy.toml` (`std::fs`, `rustix::fs` and `libc` write calls), both confining write calls to
`slates-land`. The dynamic half is the tracer run: the whole lifecycle — anchor, daemon, volume,
kernel mount, a workload through the mount (`printf`, `mkdir`, `ln -s`, `mv`, `chmod`, `rm`), a
snapshot, `land` (presented), `slates grant` with the anchor handoff the daemon's own children get
(taken from the daemon's environment; on Linux the memfd is reopened through `/proc/<pid>/fd` and
inherited), `land --grant` — under a filesystem-write tracer, every write-capable call placed in a
closed taxonomy: inside the granted target (matched to the landed entries and the engine's
`.slates-` hidden siblings), a RAM-only kernel object (memfd, `shm_open`, socket, pipe, event
descriptor, the FUSE device), the processes' own standard streams, unresolved (the tracer printed
no path), or outside — a violation.

Since 2026-10-01 (AUD-29-42):
- **A write inside the target counts only as the granted landing's:** made by the daemon's pid within the
  landing's interval on the tracers' clock (strace `-ttt`, eslogger `time`). Otherwise it is refused for
  its reason: no grant, another process, unstamped, or outside the interval.
- **A standard stream is descriptor 1 or 2 to a pipe, terminal or null device,** and the harness gives the
  slates processes a pipe as stderr, drained into the log by the harness itself.
- **A hidden sibling must be the presented landing's own name form,** and none may remain afterwards.
- **Each refusal is tested as a trace mutation**
  (`docs/bugs/2026-10-01-the-hermeticity-judge-exempted-by-name-and-descriptor.md`). On macOS `fs_usage` needs root and this host has no
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
| native Linux (FUSE) | the daemon serves its own FUSE mount (`slates mount` on Linux, `crates/server/src/fuse.rs`; proven on a real kernel mount by `crates/server/tests/fuse_mount.rs` and the CLI suite), but the Linux lane still drives the suites through a root `mount -t nfs` of the daemon's loopback export (recorded `LIMITED`); the suites over the FUSE mount are owed (pjdfstest's root cases need `allow_other`, an operator's `user_allow_other`) | records above, `LIMITED` |
| native Windows (WinFsp) | pjdfstest and fsstress do not apply (POSIX C suites); fsx needs the WinFsp port and a harness mount step; the workloads need a harness mount step; no ETW tracer. The live WinFsp mount test (`crates/bridge-winfsp/tests/mount.rs`, `WINFSP_TEST_MOUNT=1`) proves create/write/read/list/delete through the kernel and nothing more | SKIPPED, typed |
| virtio-fs guest | no live Linux guest; the device half is proven only by the simulated guest driver (`docs/wip/virtiofs.md`), and AC-9.7 says a simulation cannot close the guarantee | SKIPPED(owed) |
| OCI container | the macOS host mount handed to a real container: the runtime handshake (`slates oci-runtime docker`, its profile kept in the record), `attach --oci`, `slates oci-check` of the source just before the bind, then one `docker run` of the exact entry as `--mount` (non-recursive, private) as the mounting user, the suite compiled from its pinned source inside (`xtask/src/conformance/container.rs`; AUD-29-78). fsx and fsstress run (Docker Desktop 29.3.1, 2026-10-01: fsx 10 000 operations and fsstress 500 × 4 processes, all 2 000 logged — dread/dwrite included, Linux having `O_DIRECT` — passed); the workloads run too (git, cargo and python identical between a Desktop-shared host directory and the bind; git with `core.createObject=rename`, as the profile's measured hard-link rule asks, and the NFS client's `.nfs.*` silly-renames set aside and counted); pjdfstest runs as well (§3.4a: LIMITED, unprivileged, its failures shaped by cause, NFS-FIFO-OPEN surfaced through Desktop); the hermeticity container leg runs on Linux (the next row) and is owed on macOS (its tracer, `eslogger`, needs root) | fsx, fsstress and workloads `RAN`, pjdfstest `LIMITED` on the macOS lane where Docker answers; hermeticity `SKIPPED(owed)` |
| OCI container (Linux engine) | the Linux container lane: the session's FUSE mount made with `slates mount --shared` (`allow_other`, granted by `user_allow_other`), the runtime handshake, `attach --oci`, `slates oci-check`, then one `docker run` of the exact entry as `--mount` into a Docker Engine (`transport oci-linux`; `ci/linux-oci`, and the conformance job on CI's Linux runner). fsx, fsstress and the workloads run as the mounting user; pjdfstest runs as container root (§3.4b, its own reviewed list); hermeticity traces the anchor with a root tracer (`sudo strace -u <user>`, since an unprivileged tracer strips `fusermount3`'s setuid) while the workload runs in the bind, setting aside and naming the OS mount broker's own calls (`fusermount3`, `mount`; R10) as macOS's leg sets aside every non-slates executable | fsx, fsstress, workloads, pjdfstest and hermeticity `RAN` (2026-10-01, Debian 13) |

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

`--records DIR` (default `docs/wip/conformance/records`), `--scratch DIR` (default
`<target>/conformance-scratch-<pid>` in the build output — A-50: never `/tmp` or a RAM directory;
removed unless `--keep`), the bounds `--fsx-ops/--fsx-seed/--fsx-length`,
`--fsstress-ops/--fsstress-procs/--fsstress-seed` (defaults are `Shape:` constants in
`xtask/src/conformance/mod.rs` and are recorded in each record's `bound`). The suite sources are
fetched from pinned commits into the scratch and verified by SHA-256 before `cc` builds them
(`xtask/src/conformance/fetch.rs`); nothing is vendored (fsx is APSL 2.0, fsstress GPL-2.0 —
licences do not enter this MIT tree by a harness's hand) and nothing is installed. A suite that
does not meet its verdict exits non-zero after writing its record, so `all` on this host exits
non-zero for an unmet verdict. The 2026-09-20 Linux NFS-adapter run now passes every runnable
suite under the reviewed limits above. Native macOS root and other transport evidence must
still be established separately.

## 6. Owed

- The native macOS root rerun and its reviewed expectations; the macOS lane's sidecar
  behaviour (environmental). Linux adapter records and its first root review are now tracked.
- A daemon transport for the FUSE bridge, so the Linux cells can be RAN, not LIMITED.
- A WinFsp mount step in the harness (workloads) and the fsx WinFsp port; an ETW tracer.
- A live virtio-fs guest and the OCI attachment form; then their cells.
- Pressure and failure suites (Phase 9).
- Sibling findings for their owners: the volume root directory lists as `root wheel` through the
  mount ([docs/bugs/2026-09-14-volume-root-owned-by-root-wheel.md](../bugs/2026-09-14-volume-root-owned-by-root-wheel.md));
  `slates grant`'s human surface has no documented way for a shell to obtain the anchor handoff
  (the harness reads it from the daemon's environment; a user has no equivalent).
