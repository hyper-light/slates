# Hermeticity fixture admits too little space for the kernel client's metadata

Date: 2026-09-22. Baseline: `f02631f`; §4.2 resource dimensions, AC-4.5.

## Reproduction and cause

The corrected privileged trace attaches successfully and records 188 rows / 47,000
bytes, with no stderr output or surviving task process. The workload still refuses
`d/f2` with ENOSPC. The fixture admits 2,112 bytes, derived from eight Rust inodes.

An unprivileged reproduction through the same CLI and macOS kernel mount confirms
that the volume contains seven entries plus its root when the create is refused:
two parent directories, `f1`, `d`, and three AppleDouble metadata files. Only four
content bytes are charged; there is no snapshot. The eight-inode admission is full.
The client's sidecar writes also need an allocated page, which 2,112 bytes cannot
back. The VFS correctly enforces §4.2; bypassing this refusal would be a product bug.

The test's purpose is tracing a complete granted landing, not quota exhaustion.
Its fixture incorrectly counted only names explicitly requested by the shell.
NFSv3's client-generated metadata already has a separate failing workload record
(`2026-09-14-nfs-appledouble-sidecars.md`).

## Measured budget and oracle correction

The same workload, unchanged, completes and snapshots with eight host pages:
131,072 bytes on this 16-KiB-page host. All six explicit surviving entries and six
metadata files remain visible. Each metadata file has 4,096 logical bytes and costs
one 16-KiB allocation; final referenced content is 98,312 bytes. Command and logs:

```sh
python3.14 /private/tmp/slates-ci-35615970514-quota-diagnosis.py
python3.14 /private/tmp/slates-ci-35615970514-quota-diagnosis.py 131072
```

Logs: `/private/tmp/slates-ci-35615970514-quota-diagnosis.log` and
`/private/tmp/slates-ci-35615970514-quota-diagnosis-pages.log`.

The fixture reserves one queried host page for each of its eight peak namespace
entries. This accommodates the measured client metadata, including the temporary
entry, while leaving the shard's unpromised versions available for its snapshot.
The requested bytes and page derivation remain in the evidence record.

The landing oracle must retain the known files' byte/kind/link checks and compare
the entire pre-landing mounted manifest against the landed manifest. Every visible
entry, including `._*`, contributes to the Written count and trace matching. Missing,
extra, or altered sidecars must fail; no filename exclusion is added. Hermeticity
evidence does not close the separate workload-equivalence failure.

## Additional trace finding

The captured trace contains 23 `ftruncate`, 23 `mmap`, 92 `read`, and 50 `write`
rows. It contains no descriptor-creation rows for the IPC shared-memory objects,
and the NFS socket existed before attachment. These descriptor writes still require
sound attribution before the macOS hermeticity gate can pass. Readiness alone is
not a complete tracing verdict; unresolved writes must remain failures.

Apple's fs_usage source names `close`, `dup`, `dup2` and `fcntl`, but has no
`shm_open` entry. A descriptor snapshot alone cannot safely classify earlier writes:
descriptor reuse and replacement must be accounted for. A bounded DTrace probe is
prepared at `/private/tmp/slates-ci-35615970514-dtrace-probe.sh` to test syscall
coverage on a single owned daemon. It is not run without specific authorization,
and it changes no SIP or host setting.

## Validation

The missing-metadata negative control fails in 0.00 s. The corrected complete-tree
oracle rejects removed, added and changed metadata files. All 24 Linux harness tests
pass, followed by `cargo xtask check` and the full mounted NFS/strace lifecycle:
220 write-capable calls; 22 inside the target, six landed paths matched, zero
unmatched; 110 RAM-object calls, 88 standard-stream calls, zero unresolved or outside.
Command/log: `/private/tmp/slates-ci-35615970514-quota-linux.sh` and
`/private/tmp/slates-ci-35615970514-quota-linux-green.log`.

Strict Clippy passes on the isolated macOS source after moving the regression module
to the end of its file. The disposable Linux image lacks the Clippy component; its
offline attempt failed before linting, so no Linux Clippy pass is claimed. Logs:
`/private/tmp/slates-ci-35615970514-quota-clippy-green.log` and
`/private/tmp/slates-ci-35615970514-quota-linux.log`.
