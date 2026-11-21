# w-svn -> main: P7, the real daemon's peak against the dump (on main 8508a8d52) (2026-11-21 ~00:55 PST)

`an_import_measured` (slates-cli svn_interop, ignored; BENCHMARKS "A Subversion import on the real daemon"): release
anchor + daemon with a store, `slates repository import` of git-2000 (437 MiB dump), RSS sampled:
- 26.2 s; RSS 57 MiB -> peak 1,570 MiB (3.6x the dump).
- `status --memory`: segment resident 782 MiB; the register's log appended 838 MiB during the import (snapshot: chain
  entries 39 MiB, checkpoints 44 MiB). Heap the rest, ~790 MiB.
- The test binary's 1.5 GB of freed large blocks (my earlier attribution) is the system allocator's; the daemon's own
  allocator gives large blocks back. That part is not the daemon's.
Finding: the green's chain entries carry each revision's file bytes into the register's log, so svn history is held
three ways: records' deltas (35 MB), the store's changesets (disk, C2), the green's chain (782 MiB resident). The
chain's copy is the largest. Moving it means a Subversion green's chain naming texts by identity with the bytes in the
store. That is the register's and the merge engine's, not svn's alone: yours to assign (w-store / w-engine). Also
rejected today, measured: folding an svn green's history to its readers (copies from older revisions need those
versions: `CopyRefused VersionFolded`); recorded in BENCHMARKS.
P8 (500 MB commit) and the stock legs are running on the big Studio now (kiln r8, an HTTPS clone of main; kiln's sync
to that box kept timing out).
