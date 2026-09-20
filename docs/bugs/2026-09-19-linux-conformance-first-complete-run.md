# Linux conformance: failures after mount establishment

## Observed failure

On 2026-09-19, [job 105974090627](https://github.com/hyper-light/slates/actions/runs/35471844854/job/105974090627)
at `bf0e690` completed the first full Linux adapter run after the mount-authority and traced-startup
repairs. Its command was:

```sh
cargo xtask conformance all --records "$RUNNER_TEMP/conformance-records" \
  --scratch "$RUNNER_TEMP/conformance-scratch" --keep
```

The runner was Ubuntu 24.04.5, x86_64, kernel 6.17.0-1022-azure. This lane mounts the daemon's
NFS export with the Linux kernel's NFS client; its record correctly says that this does not
test the FUSE transport.

| Suite | Actual result |
| --- | --- |
| fsx | 10,000 operations, seed 1: passed |
| fsstress | 500 operations × four processes, seed 1: failed after 2,498,466 ms |
| pjdfstest | 8,798 cases: 5,175 passed, 3,595 unexpected failures, 28 todo |
| workloads | All nine workloads matched their host results |
| hermeticity | 178 write-capable calls: four outside the target, 20 unresolved; landing reported zero written |

## Evidence and distinctions

The fsstress anchor log repeatedly reports a lapsed daemon heartbeat and kills/restarts the
daemon. Later restarts also fail to produce their first heartbeat within the recovery budget.
The final health query fails. This is not evidence that fsstress itself failed a POSIX assertion;
the daemon's loss of service must be reproduced separately.

Many pjdfstest failures start with refused special-file creation: `mkfifo`, socket `bind`, and
block/character `mknod` return Linux errno 524, followed by missing-file failures. The NFS
implementation explicitly refuses MKNOD and the VFS has only file, directory and symlink kinds.
The Linux adapter's reviewed expected-failure list is empty. The complete per-file TAP output
is required to distinguish those capability limits and their dependent assertions from actual
incorrect behavior; the aggregate count cannot justify an allowlist.

The hermeticity trace shows a stage directory beside the granted target, an exchange of that
directory with the target, removal of the displaced directory, and a parent-directory fsync.
Those four writes are outside the target. The existing stage-and-exchange paragraph in §4.15
step 10 conflicts with R1's containment promise and with step 11's inside-target cleanup rule.
The trace also shows repeated failures to open the newly created `conformance-5543` directory
inside the target. No workload file is written. The harness reads the zero written count but
does not require the expected entries to land, so a containment-only repair could leave a
vacuous passing test.

Separately, the parser does not recognize CI's `82</memfd:slates-cr-…>(deleted)` descriptor
decoration. Its fixture instead covers `82</memfd:slates-cr-… (deleted)>`. Both identify a RAM
object; an ordinary deleted disk file must still be judged against the grant.

## Repair and validation status

The ordinary-user NFS lifecycle also reproduces a false pass locally: the original harness
exited zero with zero entries written and an empty target. After requiring all six manifest
entries (the four workload entries and their two parents), their file bytes, directory kinds
and symlink target, the same lifecycle exits one. Its trace has zero outside and zero unresolved
writes; those counts alone were not a correctness proof. Evidence: the disposable-container
command in `/private/tmp/slates-run-mounted-hermeticity.sh`, output
`/private/tmp/slates-mounted-hermeticity-incomplete-red.log`, and preserved trace/record archive
`/private/tmp/slates-mounted-evidence/hermeticity-incomplete-red.tar.gz`.

Two reduced oracle tests fail with the original landing code (`cargo test -p slates-land
--test oracle -- --nocapture`): a scratch tree created through inode operations reports zero
written and `ParentMissing`; a target with a parent handle creates an outside `.slates-…-stage`
directory. The other 13 oracle tests pass. `Volume::diverged` recognizes only opaque directories
as created, omitting scratch directories whose base state is `None`. Existing fixtures used
base-aware mkdir even for scratch volumes and therefore missed the mounted bridge's path.

The repair includes both `None` and `Opaque` directories in the created set. It removes
whole-target staging, its parent-directory write authority and its unused selection policy;
the existing per-entry writer keeps temporary files and exchanges inside the granted target.
The target inode remains stable. §4.15 step 10 is amended to obey R1 instead of authorizing a
parent-directory write. The harness also refuses unresolved writes and matches exact manifest
paths, rather than allowing an arbitrary path ending in a workload filename.

The original artifacts are downloaded outside the repository. Local reproduction and repairs
are in progress. Do not mark this lane green from unit tests, a skipped mount, the successful
startup regression, or an epoll-only container run. The complete mounted commands and their
non-vacuity checks must pass, with the intended io_uring backend verified separately.

### Local landing result

The original generic stress-volume quota reserved the entire inode slab on the 4 GiB
container. A snapshot was admitted, but its first divergent inode copy refused `NoSpace`;
this happened after the six files/directories had reached disk. The traced workload now
requests the quota of its maximum eight live inodes (`8 × size_of::<Inode>() = 2,112 bytes`),
leaving unpromised capacity for retention. This does not change production admission.
The post-write refusal and target-host lifetime need their own landing regressions (TBD_FIXES).

The final real mounted run passes: six entries written, all bytes/kinds/symlink checked,
22 inside-target calls, six exact manifest paths matched, zero unmatched, zero unresolved,
zero outside. RAM-backed targets are attributed to their grant before the generic shared-memory
exception; unnamed temporary paths are recognized from successful `O_TMPFILE` results, never
just a filename prefix. The pure trace suite passes 44/44; the landing oracle passes 15/15
(10.92 s), with the nested scratch test also passing after retaining a snapshot.
Evidence: `/private/tmp/slates-mounted-hermeticity-attributed-green.log` and the archive of the
same name under `/private/tmp/slates-mounted-evidence/`. Container: Rust 1.98.0, Debian 13,
aarch64 Linux 6.12.76-linuxkit, four CPUs and 4 GiB. Records use UTC 2026-09-20 (local date
2026-09-19). This is the Linux NFS adapter, not the FUSE transport.

### Local fsstress reproduction

The pinned 50 × four-process seed-1 prefix passes (200 logged operations). The original
500 × four-process seed-1 history reproduces heartbeat kills within seconds; all four
workers block while successive recovered daemons consume a CPU and lapse again. The run
was stopped after preserving its anchor log instead of waiting for CI's 42-minute failure.
`/private/tmp/slates-fsstress-live-diagnosis.log` records the process and restart evidence;
`/private/tmp/slates-mounted-evidence/fsstress-500.tar.gz` keeps its scratch. Diagnosis continues.
