# SQLite refuses WAL mode inside the NFS mount (T-3.3)

Status: **declared limit of the NFS loopback transport — not fixable on the server side**;
found by the conformance workload suite (`cargo xtask conformance run --suite workloads`,
docs/wip/conformance.md) on 2026-09-14.

## Description

The sqlite workload runs `PRAGMA journal_mode=WAL;` on a fresh database. On the APFS host it
answers `wal`; inside a live `slates mount` it answers `delete` — SQLite declined to switch and
stayed in rollback mode. Everything else in the workload (two processes inserting at once with a
busy timeout, the count and order, `PRAGMA integrity_check` = `ok`) matched the host.

## Root cause

SQLite's unix VFS chooses its locking method by the filesystem's type name: `autolockIoFinder`
returns `nfsIoMethods` when `fsInfo.f_fstypename == "nfs"` (`src/os_unix.c` at the current trunk,
lines 5995–6005), and that method table is declared with "shared memory is disabled" and no
`xShmMap` (lines 5940–5949). WAL needs the shared-memory map (`sqlite3PagerWalSupported`), so on
any mount whose type is `nfs` — which the loopback bridge is, by the mount's nature — WAL cannot be
enabled, whatever the server does. Evidence: SQLite's source, tier C.

## Impact

T-3.3 ("sqlite WAL-mode database inside the volume with two processes; expect correct results and
no corruption") cannot be met through the NFS fallback; a tool that insists on WAL (many do not:
SQLite silently keeps rollback journaling and still works) sees rollback mode instead. Correctness
was not affected in the measured run.

## Exact edits

None on the server. The FSKit primary path presents another filesystem type and would take
SQLite's default locking method; until it is live, §4.6's Degraded cells for the NFS fallback
should name "no WAL mode for SQLite (its VFS keys on the `nfs` type)", and the conformance record
carries this file as the reason for the sqlite workload's DIFFERS cell.
