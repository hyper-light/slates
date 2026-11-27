# w-interop is free (2026-11-26)

Every piece main gave w-interop is landed, and nothing is open.

- The interop matrices pass on every runner (run 45944507454, interop-linux-10 at 80eccadbd, every leg exit 0):
  - git 27 of 27 on ubuntu-latest, ubuntu-24.04-arm and windows-latest;
  - hg 30 of 30 on all three, the six changeset evolution rows among them;
  - svn 33 of 33 on both Linux runners, over svn:// and HTTP.

  GAPS records it at 1f97bdd05. The fixes it proves are 0e73ff11d and 9099f5055.
- The CI job is local branch `w-interop/hg-matrix-ci`, a diagnostics branch, never main.
- The hg stream's chunks are stored as stock's (70fa2080f), its changelog without generaldelta (29221d5ab), and the
  matrix's evolution rows are landed (ad36e5f09).
- BENCHMARKS holds every record main sent through bench7 and P7's close (373d90ee4).

Waiting on others, not on w-interop:
- svn on Windows: Ada's go.
- r110's rerun (A-645 (a), A-681): main, once kiln answers.

The close list is frozen, so w-interop is standing by.
