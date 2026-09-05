# The equivalence policy of the differential suite (AC-1.2)

Status: reviewed for Phase 1, 2026-09-05. The suite is `crates/vfs/tests/differential.rs`; the
histories are those of the model suite (`crates/vfs/tests/common/steps.rs`). This document is
the reviewed list of what "the same" means when a volume and a host filesystem are compared
after every step, and of every place the two are allowed to differ. Anything not listed here is
compared exactly, and a difference there is a bug on one side or the other.

## 1. What is compared after every step

- The outcome of the step: success, or the refusal's errno name (`VfsError::errno_name` against
  the host's raw errno), subject to §3.
- Every directory's listing: the sorted set of `(name, kind)` where kind is directory, regular
  file or symlink. Order is not compared (the host's is hash order; the volume's is canonical).
- Every regular file's bytes, read whole, by path. Holes read as zeros on both sides, so a
  sparse write compares by content.
- Every regular file's link count (`nlink`), by path, which is how hard links are checked.

## 2. What is not compared, and why

- Timestamps: the host's granularity and update rules vary by filesystem; the design's own rules
  are checked by the model suite.
- Inode numbers: the host assigns its own; the volume's monotonic numbers are checked by the
  model suite (AC-1.6).
- Mode bits: the host applies the process umask; the volume stores what it was given.
- Symlink targets: the harness anchors every symlink at one regular file (below), so targets
  are constant by construction and only the entry's kind is compared.
- Block counts and sizes on disk: the volume's accounting is the design's (chunk windows), not
  the host's block allocation; AC-1.7 covers it.
- Snapshots: a host filesystem has none; the `Snapshot` step is a no-op on the host and the
  volume's snapshots are checked by the model suite.

## 3. Allowed outcome differences, by platform

| Step | Volume | Host may say | Where | Why |
|---|---|---|---|---|
| unlink of a directory | `EISDIR` | `EPERM` | macOS, BSD | POSIX permits either; Linux says `EISDIR` |
| rename over a non-empty directory | `ENOTEMPTY` | `EEXIST` | some filesystems | POSIX permits either |

Everything else must match exactly, including: `ENOENT` for a missing path component and
`ENOTDIR` for a component that exists and is not a directory (the model suite resolves paths
the same way); `EEXIST` on create, mkdir, symlink and link over an existing name; `EISDIR` and
`ENOTDIR` on renames between a file and a directory; `EINVAL` on a rename into the source's own
subtree; `EPERM` on a hard link to a directory; `ENOTEMPTY` on `rmdir` of a non-empty directory.

## 4. Names

The volume's name policy is chosen to match the host's, probed at the start of a run: the
harness creates `probe-a` and tries `PROBE-A` with `O_EXCL`; `EEXIST` means the host folds
(APFS by default) and the volume is created with `NameEquivalence::Fold`, otherwise `Exact`
(tmpfs, ext4, NTFS in its POSIX mode). The generated names are six ASCII names, two of them
case variants, so a folding host and a folding volume both refuse the variant and an exact pair
both keep it.

## 5. Symlinks in paths

The volume does not follow symlinks inside a path (a bridge does that above it, §4.6), so a
path through a symlink would resolve differently on the two sides. The harness makes every
symlink point at one regular file (`.anchor`, created per case and hidden from listings), so on
the host a path through a symlink reaches a regular file and fails with `ENOTDIR`, exactly as
the volume's "not a directory" for a symlink component. Unlink, rmdir, rename and create over a
symlink's own name compare as usual.

## 6. Quota and space

The volume runs with a bounded quota far above what a history can write (1 TiB); `ENOSPC` is
excluded from comparison and checked by the model suite and T-1.5.

## 7. Where the suite runs

Linux CI on `/dev/shm` (tmpfs), 2,000 histories of up to 40 steps per run with shrinking;
locally only when `SLATES_TEST_RAMDIR` names a RAM-backed directory, otherwise it prints that
it skipped and passes. It never writes disk: a RAM disk on macOS or an NTFS RAM VHD is a
system-state change that needs its own authorization, and the nightly lanes for those targets
are Phase 4's.
