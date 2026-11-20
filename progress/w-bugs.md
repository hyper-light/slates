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
- State folder moved to ~/fanout-state-w-bugs (main's rule, 4d39b11). Numbers: claim only by a successful push.
- A-717 LANDED main 721f4ff97 (one commit, fast-forward; files: crates/ipc/src/rendezvous.rs, the bug record, GAPS).
  Windows proof by use: diag-w-bugs-b3-5 (run 44685574660): `slates volume list` against a live anchor exit 0 (was 4).
  Run requests for main: Windows clippy of slates-ipc (marks_image is cfg(any(windows,test))); the Windows lanes'
  WinFsp suites now reachable.
- Next: B3's answer from b3-5's fsx leg (running).
- B3 REPRODUCED by use on main+A-717 (diag-w-bugs-b3-5, windows-latest x86_64): fsx seed 1 wedged after op 45
  (TRUNCATE DOWN 0x3ced0 -> 0xde10, after a write past a hole / shrink / extend), ended at its 300 s bound; matches the
  reviewed expected-failure line. Hypothesis (g258's GAPS "A delete racing a coherent purge"): WinFsp completes an
  answered IRP on the sending thread; SetInformation's completion runs CcSetFileSizes there; a page fault/paging read
  it needs can be received only by slates' single receive worker (A-198), which is the one sending -> deadlock.
  Run 6 (diag-w-bugs-b3-6, worktree ~/slates-work/w-bugs-b3s, branch agents/w-bugs-b3-stacks = diag only): user-mode
  stacks of every slates.exe/fsx.exe thread (cdb -pv) at 90/180/270 s into the wedge.
- WinFsp v2.1 source confirms the path: volume.c 1089-1102 completes each answered IRP on the sender's thread;
  fileinfo.c 1407 (SetInformation EndOfFile completion) -> FspFileNodeSetFileInfo -> file.c 1852 FspCcSetFileSizes.
  settle.rs is strictly serial over one journal (save, send, ack, then next receive), so a blocked send stops the drive.
- Run 6 (b3-6): stacks taken too early (mount not up; no slates-winfsp-receive thread yet), no slates symbols.
  Run 7 (diag-w-bugs-b3-7, f59ff7698): waits for fsx.exe + 60 s, slates PDB from target/debug.
- Runs 7-8: cdb -pv froze the daemon (anchor killed it at 2 s); its output was cut before the receive thread printed
  (my "no receive thread" read was WRONG, corrected). A stacks-only minidump of the wedged daemon hung (0xC000011E).
- Run 9 (diag-w-bugs-b3-9, 27c8b14a1 = main + diag-only operate/send log in dispatcher.rs): CAUSE SHOWN. Log ends
  `operate kind 8` / `send begins kind 8` (SetInformation, op 45's shrink) with no `send returned` for ~59 s; every
  earlier send <= 481 us. = the single receiver blocked in its own send while WinFsp completes the answer on that
  thread (CcSetFileSizes), waiting on a drive request nobody else can receive.
- LANDED docs main 06bafe27a: bug record "Shown 2026-11-20" (corrects "below the single receiver"), GAPS hang entry,
  expected-failure line's reason.
- NEXT (owed, A-198 amendment): a receive never waits on a send. Design pending; Windows-only, CI-iterated.
- 2026-11-20 main: B3's fix is mine; jupiter (no WinFsp; Windows toolchain, 32 cores) takes Windows compiles/unit
  tests, target C:\Users\corph\.cache\w-bugs-target; drive runs stay on CI diag. After B3: B14, then bug-claims open.
- B3 DESIGN (relief receiver): P (primary) and R (relief), each its own capture region (held section + one
  capture::BYTES). Station (DLL op state, PrivateOpens, confirming, may-link set moved out of thread_local, working
  packet) is MOVED, never shared: P parks it in an AtomicPtr slot before each send and swaps it back after; R looks
  every beat (100 ms): if P is in the same send (odd send generation unchanged) R swaps the station out, serves its
  own region until P sets reclaim, then hands it back (back slot + unpark). One operating thread at a time (A-198,
  DLL single caller, REQUEST_BOUND and one outstanding confirmation unchanged). Recovery settles both regions;
  settle_pending gets the saved open from either. Failing test first: pure relief module over generic Port/Station,
  model driver whose kind-8 send blocks until a nested request is answered (red with today's single receiver).
  Residual: R stuck in its own send is not relieved (bound 2).
- B3 LANDED A-725 main c22dce946 (commit 0f5591c03, one commit + merge): relief receiver (bridge-winfsp
  inflight/relief.rs), two journal regions, server RELIEF_BEAT = HEARTBEAT_NS; fsx judge counts the port's extensions;
  fsx-seed-1 line struck; bridge-winfsp Windows test support fixed (A-551 Mounted::new, A-460 handoff section).
  Evidence: red/green on Mac (10.1 s timeout -> 0.03 s); CI b3-11 x64+arm64 fsx PASS, winfsp_semantics/cli winfsp
  pass; lib-fix-1 127 vs lib-control-1 126 (only failure both arms: A-576 upcase table, 109 units differ).
- FOUND (pre-existing, not mine, next item for me after B14): host::upcase_table::the_volumes_upcase_table_is_the_systems
  fails on Windows x64+arm64: 109 code units where Windows' upcase table differs from the published exFAT table
  (e.g. 023A: volume 2C65, system 023A). A-576 part 1's owed Windows check; A-576 said: take the system's table.
- jupiter: no NASM -> server builds need AWS_LC_SYS_PREBUILT_NASM=1 there (check builds only).
- Slips this stretch: a retry loop with a `git reset --soft` fallback (never ran); noted.
- Next: B14 (takeover-kept files at a real Linux mount's end, test gap), then the upcase table, then bug-claims open.
