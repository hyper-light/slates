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
- B11 run 1 (diag-w-bugs-b11-1, ubuntu-latest 4 CPUs, unloaded): 30/30 pass, real mount (3 seeds, conflicts/unknown/
  direct reads counted). Run 2 (b11-2): 50 runs beside 2 spinners/CPU, in flight.
- B3 run 2 (b3-2, run 44682893059): main's Windows build FAILS: crates/anchor/src/supervise.rs:381 E0560 (9abe7e9a2,
  A-689, `vcs_svn_listener: None` without #[cfg(unix)]). fsx never ran (exit 1). Fix = commit 88a96c97c on local main
  in ~/slates-work/w-bugs-main (not pushed yet); diag-w-bugs-b3-3 (2f47f0e0b = main + fix + workflow) proving Windows
  compiles; land the fix on its green.
- Main fixed the Windows build itself (36478561d); my identical fix 88a96c97c stays local and unpushed (dropped).
- B11 run 2 (b11-2, run 44683374880): 50/50 pass beside 8 spinners on 4 CPUs. B11 NOT reproduced by use (80/80 total).
  Per Ada's rule not chased further; reported to main.
- B3 run 3 (b3-3, main + the cfg fix): Windows builds; clippy fails on slates-db repository.rs:940 apply_advanced
  (complexity 13/10) and slates-vfs recover.rs:2853 rebuild_snapshot_tree (11/10) -> main's lint keeper, not mine.
  fsx did NOT run: xtask "the daemon did not come up within 60s (last exit 4)" though the anchor log shows the daemon
  serving on 2 shards. `slates volume list` exits 4 (EXIT_FAILED) on Windows every try. Run 4 (diag-w-bugs-b3-4, from
  main ea05b2ad6 past 36478561d, worktree ~/slates-work/w-bugs-b3 branch agents/w-bugs-b3 = diag only) captures
  volume list's stderr against an anchor started as xtask does.
- B3 run 4 (b3-4, run 44684243854, main ea05b2ad6): `slates volume list` against a live Windows anchor exits 4
  every try: "channel: shared memory: shared layout refused at 0 for 64 bytes: TouchesWord". Cause: rendezvous.rs
  Windows unanswered_end/holder_lives copy the whole 64-byte bootstrap header (28c4f3839, 3e93acc84); bytes 8-24 are
  declared words -> refused. = A-717 (claimed in amendments.tsv).
- A-717 fix: marks_image copies bytes 24..48 only. RED on macOS arm64 Mac Studio: new test
  a_claim_reads_the_process_marks_of_a_published_bootstrap_object failed LayoutRefused{0,64,TouchesWord} with the
  whole-header copy; GREEN: `cargo test --locked -p slates-ipc --lib rendezvous::platform` 15/15, fmt clean.
  Branch agents/w-bugs-claim-marks (worktree ~/slates-work/w-bugs-land): 072f36427 fix + 584f4d776 wip docs (bug
  record docs/bugs/2026-11-20-every-windows-client-connect-was-refused-touchesword.md, GAPS line). Windows proof:
  diag-w-bugs-b3-5 (048dbaf9b = fix + workflow) running. LAND as ONE commit from a fresh worktree off origin/main.
- Slip: one command ended in `git checkout -b diag-w-bugs-b3-5-local` in ~/slates-work/w-bugs-fix (same commit,
  nothing lost); no further checkouts.
