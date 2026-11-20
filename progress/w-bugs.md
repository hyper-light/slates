# w-bugs -- kiln goal 87 "Every open slates bug is fixed, failing test first and red-checked" (Mac Studio worker)

## Started 2026-11-19 (fresh agent; no predecessor for w-bugs)
- Read: relaunch-2026-11-19.md, briefs/common.md, briefs/w-bugs.md, restart-preamble, lint-habits, g258 + g079 progress,
  bug-claims.md, amendments.tsv rows.
- Host: Adas-Mac-Studio-4 (14 cores, 36 GB, disk 460 GiB at 98%: 9.8 GiB free at start, all of ~/.cache others' (kiln-*),
  nothing of mine to delete). Under the 20 GB guard: build only small crates, df before each build.
- Clone ~/slates-work/w-bugs-main; target ~/.cache/w-bugs-target (CARGO_BUILD_JOBS=8).

## Where the brief's steps stand (found 2026-11-19)
1. A-498: LANDED by g258 (1d2723e51): the stated hard-link lines only when the drive makes no link. Left: the delete/purge
   hang's fix (needs WinFsp VM runs; hypothesis = single receive worker deadlocked inside FltMgr, A-198); WinFsp
   released driver refuses FileLinkInformation, so stock WinFsp on CI can't link.
2. A-551 (B2 Windows half): LANDED by g258 (8cb6bef1a). Windows build + drive tests owed (CI Windows runner).
3. A-576 part 1: LANDED by g258 (520a86184). Part 2 owed (selection wire/CLI/SDK/MCP, WinFsp mount of a Fold volume):
   a feature, not a bug.
4. goal-83 B-list: landed B1, B2 (A-543/A-551), B7a (A-526), B7b (A-530), B10 (A-573, g232), B12 (A-550), B13 (A-535).
   Open: B3 (WinFsp fsx wedge, Windows), B4 (WinFsp reply spin SAMPLES, reply.rs), B5 (WinFsp arm64 open/close 4 ms),
   B8 (FUSE takeover coherence, Linux), B9 (FUSE notify rounds unbounded, Linux), B11 (FUSE coherence chaos flake),
   B14 (test gap, Linux real mount), B15 (conformance expected-failures triage), B16 (two bug records still "open").

## Log
- B16 LANDED main 439699c56 (d47b10a38 + merge): both records' Status set to fixed (8613d1107 MKNOD fifo/socket;
  7deddf549 the wake wired), GAPS' wakes_with entry gained its Fixed line. Tests on main 8eeb004bd, macOS arm64, Mac
  Studio: bridge-nfs --test procedures mknod 4/4; vfs --test change_waiters 1/1. Docs only.
- Push: this Studio has no HTTPS GitHub credentials; SSH works (`git remote set-url --push origin
  git@github.com:hyper-light/slates.git`).
- Next: B4 (WinFsp reply spin, crates/bridge-winfsp/src/reply.rs SAMPLES).
- B4: already fixed on main by A-560 (ba48ab017, 2026-11-18); bug-claims marked. Nothing to land.
- B8, B9: reading findings ("not reproduced"/"not measured"); per Ada 22:45Z not chased until use shows them. Need
  a Linux FUSE mount (not this Mac).
- Main's CI (2026-11-20 ~05:30Z) is red on every Windows step after "Clippy the Windows-buildable crates" (all lanes),
  fmt fails on macOS/ubuntu. Job logs need admin rights (unauthenticated here), so I read Windows through a results
  branch: workflow .github/workflows/diag-w-bugs.yml on diag-w-bugs-<bug>-<n> (worktree ~/slates-work/w-bugs-diag),
  outputs committed to <branch>-results-<run>-<runner> (status.txt, clippy.txt, fsx.txt, records). Run 1
  (diag-w-bugs-b3-1, 44682281003): steps under bash -e ended before publishing; fixed (set +e) -> diag-w-bugs-b3-2.
- B3 question: does fsx seed 1 still wedge over the WinFsp drive on main's tree (efa2266be's parent = main 9ea6e1311)?
