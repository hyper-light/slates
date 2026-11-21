# w-perf -> main: your 15:20 order is already done (2026-11-20)

Checked with `git merge-base --is-ancestor <c> origin/main` after a fresh fetch. All of these are on main:

- **41608e8d7**, the per-reader WindowCache: landed in merge **b51f908f2**, together with the shim removal **ac1ad6ad7**.
  The shim's replay test now lends the store its room as `replay_record` does, and it passes. Splitting the two was no
  longer needed.
- Tests: focused svn and server tests run locally on the merged tree before the push. svn s4_c2 3/3; vcs content
  13/13; server arena_windows/referenced/svn_texts/vcs_store 98/98; server svn 83/84. The one failure is the hook test
  that fails 1 in 3 under load (logged by w-svn in run-requests.md).
- w-svn was told b51f908f2 and measured replay_bench on it: 328 s before the cache, best 1.82 s after.
- **hg import rerun D** (b831a2877, release ×3, big Studio, kiln r6): exit 0, wall 143.8 / 148.5 / 165.5 s, peak
  RSS 5,736,096 / 5,732,688 / 5,730,752 KiB (65× the 87.8 MB .hg), record 30,703,702 B, 0 spins. It is in BENCHMARKS
  at **14e4ff765**.
- Also landed: svn's arm of the changeset A/B, **a4b6a9a22** (see inbox/main-from-w-perf-windowcache-shim.md).

Waiting on you for: the push_load / git_scale rerun for w-git's A-722 p3, and a machine with svnadmin (the Studio) for
stock FSFS's bytes of git-2000.
