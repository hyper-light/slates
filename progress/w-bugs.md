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
- A-737 (claimed; Windows' own upcase table) on branch agents/w-bugs-a737 (wip bfdc82eed, worktree ~/slates-work/w-bugs-a725
  which also carries B14's wip test commit -- land each from its net diff). Windows' table dumped on CI x64 build 26100
  + arm64 build 26200 (identical, 973 mapped, no many-to-one: final sigma kept apart; diag-w-bugs-upcase-1). vfs
  upcase/names 9/9, bridge-winfsp normalized 7/7 on Mac. Windows check run: diag-w-bugs-lib-737 (pending).
  Slip: a sed renaming A-726->A-737 hit another agent's A-726 refs in crates/svn + cli test; restored from HEAD by
  `git show HEAD:path > path` before any commit.
- B14: new test a_file_kept_for_a_mount_that_survived_a_takeover_is_given_back_when_the_mount_ends FAILS in green too
  (CI ubuntu kernel 7.0: 1,048,576 B stay referenced after lazy unmount + close; red arm the same). Hypothesis: an
  ordinary FUSE mount's end (no FUSE_DESTROY) never sweeps the attachment's restored lookup references
  (end_attachment only ends the takeover holder; sweep_attachment only from FUSE_DESTROY). diag-w-bugs-b14-green-2:
  status at the end + the same without a takeover.
- A-737 LANDED main 2a80ea399: Windows' own upcase table (973 units), Windows check passes on both arches.
- B14 = REAL BUG, shown by use (diag-w-bugs-b14-green-2, CI ubuntu kernel 7.0): a file unlinked while open on a FUSE
  mount that then ends by lazy unmount + last close stays referenced forever (status: attachments 0, no takeover_kept,
  referenced_bytes 1,048,576), WITH OR WITHOUT a takeover. Cause: nothing sweeps a FUSE attachment's references at an
  ordinary end (no FUSE_DESTROY; only virtio-fs/NFSv4 call sweep_attachment); end_attachment only ends the takeover
  holder. Fix: end_attachment sweeps RefOwner::Attachment(record.id) (sweep_attachment ends the holder too).
- A-739 (B14's fix) wip on agents/w-bugs-a739 (worktree ~/slates-work/w-bugs-a739): end_attachment sweeps
  RefOwner::Attachment(record.id); new cli test a_file_an_ended_mount_held_is_given_back + the takeover one. Mac:
  server --test attachment_term 4/4, attach_forms 9/9. CI: diag-w-bugs-a739-green-1 vs -red-2 (red-1 was identical to
  green: sed missed the reflowed call).
- A-739 LANDED main 137bf3633 (commit c468b1309): B14 built; found + fixed: an ordinary FUSE mount's end swept nothing
  its kernel held (end_attachment now sweeps the attachment). CI ubuntu red/green shown.
- bug-claims: junction tests (fixed 2359d2550) and Windows landing w/o link privilege (passes on jupiter) marked.
- Next candidates (by use): six fleet tests on jupiter (line 39); B5 arm64 WinFsp open/close 4 ms; B15 triage
  (winfstest 382 entries now runnable); hangup test flake (line 87).
- B15 ANSWERED (no code): every reviewed-list entry has a stated reason; winfstest on CI both arches (diag-w-bugs-b15-1)
  733 cases / 351 pass / 382 fail all expected. Line 39 (six fleet tests on jupiter): load/oversubscription, already
  handed to owners; not reproduced by me (would need whole fleet suites).
- Next: B5 (WinFsp arm64 open/close 4-5 ms) by use on CI arm64.
- B5 in flight: diag-w-bugs-b5-1 (both arches): a written file's open+close via PowerShell: arm64 NTFS 73 us, WinFsp
  memfs 190 us (no 4 ms on the platform's own sample FS); x86_64 NTFS 51 us, memfs 123 us. The slates half failed
  before timing (unknown step); b5-2 logs each slates step.
- B5 NOT REPRODUCED (diag-w-bugs-b5-4, main 137bf3633): written-file open+close p50 arm64 slates 229 us vs memfs
  186 us vs NTFS 68 us; x86_64 93 / 44 / 13 us. No 4-5 ms. Closed in bug-claims.
- Goal 87 B-list now: landed B3 (A-725), B14 (A-739), B16; fixed earlier B4; answered B15; not reproduced B5, B11;
  reading findings B8, B9 (main's). Plus A-717, A-737 found and landed on the way.
- hangup flake LANDED main 64e5667fa (test fix; reproduced 2/30, fixed 0/30 on CI ubuntu under load).
- Main's ask (2026-11-20): A-551/A-576/A-498 on Windows via CI, failing-test-first. In flight: diag-w-bugs-win-green-1 /
  -red-1 (red = CaseSensitiveSearch for every volume): A-551's two drive tests + new A-576 test
  names_windows_folds_alike_are_one_file_on_a_folding_drive_and_final_sigma_is_two (branch agents/w-bugs-win, commit
  "wip: winfsp drive test...") + the workloads suite (A-498 hg byte-identity on stock WinFsp).
  A-551's drive tests already passed on both arches in b3-11 (22/22 winfsp_semantics).
- A-498 hang: new test a_delete_racing_a_coherent_purge_returns_and_removes_the_file (crates/cli/tests/winfsp.rs; 60
  rounds: SDK write, drive open+hold, SDK rewrite -> purge, close+delete at once; hang = 10 s). diag-w-bugs-race-new-1
  (main with A-725) vs -old-1 (main just before A-725), 3 runs each, both arches. Patch: ~/.cache/w-bugs-msg/race.patch.
- B-list for main: B6 has no record anywhere (state, repo, bug-claims); g079's rebuilt list (2026-11-19) lacked it with
  B1/B7 (landed) -- likely closed before the wipe, unverifiable here.
- Disk (main 2026-11-20): deleted ~/.cache/w-bugs-target; ~/.cache/w-bugs-target-land kept without incremental/; builds now CARGO_INCREMENTAL=0. ~/.cache/slates-work-target-g227-* is not mine.
- A-747 LANDED main 4e9c31aba (14c95642f): main's red route::hostile_claims_... -- not A-735 (fails at its parent and
  back to d576a383a, which added the silent-connection step: never passed on macOS). Cause: XNU setsockopt(SO_NOSIGPIPE)
  EINVAL on an accepted socket whose client already closed -> Listener::accept Err -> route_failed. Fix: Accepted::
  {Connection, Departed}; serve counts Departed as route_malformed.
- Windows items (CI both arches): A-551 red-checked (folding test FAILS with CaseSensitiveSearch forced; green passes);
  A-576 drive test (ɐ/Ɐ one file, ς/σ two) green, red with the same forced flag; workloads (git/hg/svn) pass on stock
  WinFsp; A-498 hang REPRODUCED on pre-A-725 main x86_64 (2 of 3 runs: one 10 s timeout at round 7, one process hang
  at round 43) and gone with A-725 (6/6). To land: the race test + A-576 drive test (patches in ~/.cache/w-bugs-msg).
- LANDED main 15d25e8b2: the A-498 race test (A-725 cures the hang: pre-A-725 x86_64 2/3 hung, post 6/6) and the
  A-576/A-737 folding-drive test; A-551 drive tests red-checked; workloads pass on stock WinFsp both arches. GAPS updated.
- Goal 87's assigned items done. Open: B8, B9 (reading, main's); B6 no record anywhere.
- e8788ea65 (docs): A-551 sharing-violation + normalized-name parts red-checked apart (win-red-1 / win-nred-1, both
  arches); B7a/B7b tests pass on Windows x86_64 (jupiter); GAPS records the B-list closed and B6 unknown.
- Slip: `git switch -c` used once to name a detached diag worktree's branch (no harm); no repeat.
- Goal 87 parts done; waiting for main's next piece.
- LANDED main b56e5766d: the WinFsp fsx record re-recorded (diag-w-bugs-fsxrec-1: x86_64 + arm64 both pass seed 1
  whole, list empty) and conformance.md's matrix re-rendered; matrix test 4/4 (main's doc-truth red from my A-725).
- NEXT PIECE (main, goal 93): Subversion's fetch rule (§4.17.1 "A fetch's answer", A-745): measure checkout +
  update-report of a large repo over svn:// and DAV (past-budget counter, admitted sizes); if past budget or whole-answer
  admission, slice the editor drive on both edges, red first (checkout larger than a request's room), svnadmin-identical
  bytes as oracle, generation hold test. w-svn owns the svn crates: inbox note first.
- svn fetch (goal 93) in flight: reading says both edges already slice (svn:// report exchange writes calls until the
  turn's room or steps are spent, yields between turns; DAV REPORT a room-sized slice a reply run, store lent each run;
  drive steps are bounded). Measuring by use: diag-w-bugs-svnfetch-1 (ubuntu, release, git/git 1,000 commits as svn,
  checkout + update across 500 revs over svn:// and DAV), instrumented (diag-only) with per-conversation shard
  long_steps/longest_step_ns, the most bytes a turn wrote vs the room. Worktree ~/slates-work/w-bugs-svn (branch has
  the diag commit; a real change starts fresh from origin/main). Inbox note left for w-svn.
- svn fetch run 4 (macos-latest; Linux names no issuer surface so no token): checkout svn:// 4.97 s, DAV 5.88 s, update
  across 500 revs 2.0 / 2.03 s (4,872 files, 1,000 revs). FOUND: DAV report runs write up to 410,243 B against a room
  of 278,528 (1.47x) -- the room does not bound a slice. Quantum on CI mac 3.95 us; lifetime longest_step 1.04 s (the
  import). svn:// bytes were measured after step() clears out -- wrong place. Run 5 measures each slice's own time and
  bytes (svn:// answer(), DAV report.resume()).
- svn fetch run 5 (macos-latest): BOTH EDGES PAST BUDGET AND ROOM. svn:// slices up to 12.4 ms (99/108 past ~6.2 us
  quantum), up to 316,779 B vs room 278,528; DAV runs up to 8.3 ms (255 past), 410,243 B. Checkout 3.7/3.9 s, update
  2.1/2.06 s. Fix planned (A-745's hg template): slice never past its room (carry overflow), a stop-check closure from
  the server bounding each slice by the quantum, the report's generation held. Inbox note to w-svn written.

## 2026-11-20: A-751, the resumable svndiff window encoder (main's piece, Ada's rule: performance gaps upfront)
- Landed part 1 d12b5cdc8: svndiff::WindowMaking (the deltifier and the sections in units, no borrow of the texts)
  and svndiff::lz4 (lz4_flex 0.11.6's block compressor, same bytes, in units). Main's red: a 102,400 B window took
  6.0-7.7 ms in one step (debug); with 256 units a step, none past 500 us; every window is the whole window's bytes
  under every unit size, v0 and v2 stored/LZ4.
- Landed part 2 (main 1d7a60e1d): the update report's drive reads, makes and sends each window 1,024 units a step
  (Call::DeltaPiece). svn:// frames a write's pieces as one textdelta-chunk; DAV base64s them. Red: big.txt driven a
  step a call, 4 steps of 2.8-3.0 ms each run; green none past 500 us; the stream is the whole-window path's byte for
  byte; the svnserve sessions are unchanged.
- Owed: the replay drive (report/drive/replay.rs: svn:// replay/replay-range and the exporter's source) and
  get-file-revs (reads/file_revs.rs) still make a window whole; format 3 adopts the maker (w-svn, inbox note sent).
- Run request for main: re-run diag-w-bugs-svnfetch's checkout/update on macos-latest at 1d7a60e1d (svn:// and DAV
  against svnadmin load over file://). Red would be a diff against svnadmin's working copy, or a slice still in
  milliseconds.
- 2026-11-20, A-751 part 3 landed fd99ab174 (base f2421b5f3): the replay drive and get-file-revs make each window through
  window_unit, a step at a time; svn:// replay turns and the file-revs streams stop at the quantum. Red with
  whole-window steps (~3 ms a step), green (none past 500 us); deltas byte for byte the whole-window path's;
  svnserve sessions unchanged. The timed tests now re-time a step past budget from its state before (a preemption
  under the suite's load reached 3.6 ms).
- CI diag-w-bugs-svnfetch-8 (macos-latest, 1d7a60e1d): svn:// and DAV working copies are svnadmin's after the checkout,
  the update back 500 and the update forward. Most bytes in a slice 9,216 (svn://) / 2,732 (DAV). The longest slice
  is still 3.2-4.6 ms / 2.7-3.6 ms against a ~2.9 us quantum: the long step is not a window's. Unnamed; naming it
  needs per-step instrumentation (a diag run).
- 2026-11-20 A-751 tests on units landed 26e125adb (base adacdeec8): each window step held to WINDOW_STEP_UNITS (1,088),
  red 205,172; the maker's walk now checks its units after a failed look-up. (Its parent 03f872541 reached main as
  "wip": don't repeat; amend before push.)
- CI diag-w-bugs-svnfetch-9 (macos-latest, base 508b2be88, step kinds timed): dir:properties is the long step, max
  3.9-5.8 ms (svn://) and up to 19.8 ms (DAV), 100-650 us on average (both listings, every name ever made beneath,
  two resolves per child). Rarer: file:window-unit spikes to 1.9-5.4 ms (<10 per ~200K steps, before 26e125adb's walk
  fix), dir:targets 2.7-8.5 ms (1 each), file:properties 1.2-2.0 ms, a call's write 1.2-3.5 ms. Correctness holds.
  Checkout times 11.0 s / 14.2 s vs run 8's 7.1 / 7.6: unexplained (runner, part 3, or other commits), to A/B.
- Proposed to w-svn (inbox): per-directory-row persistent children maps in the index, listing at O(live children),
  entries with ids (no second resolve), then a cursor for very large directories. Waiting on w-svn's agreement
  before touching index.rs.
- 2026-11-20 runs 10/11 (macos-latest, 477abbf73): fetches 3-12x slower than run 8 (checkout 25.0 s / 23.2 s vs
  7.1 / 7.6; update 32.5 / 24.6 vs 2.7 / 3.6). Window steps ~100 us average (4.4 us in run 9), up to 95 ms. Cause:
  S4 c2 14215b0c3 (w-perf): Repository::text rebuilds the whole text from its skip-delta chain on every call, and
  the fetches read 1,024 bytes a step. Sent to w-perf's inbox with two shapes that fit the budget (a made-text cache
  or reader handle; the chain applied in units). Listing measurement landed 477abbf73 (3.84 ms children regardless
  of live count). Still waiting on w-svn's agreement for the listing index.
- 2026-11-20 A-759 landed: part 1 088bf935b (children maps, listing O(live), children 3.84 -> 0.23 ms), part 2
  1417c0868 (cursor, 16 entries a step, 1.6-1.75 us a step; red 1,801). Main signalled
  (inbox/main-from-w-bugs-a759.md). Left: replay add_subdir whole listing; CI re-measure waits on w-perf's S4 c2 fix.
- 2026-11-20 A-759 part 3 landed 7d2b45174 (replay add_subdir through the cursor, red 10,000 -> 16). Reported to
  main: vcs_ssh svnserve_t commit test red on clean main 359da2388, suspect 28e57ac18 (durable texts). Next: CI
  rerun after w-perf's S4 c2 fix, then the next open bug.
- 2026-11-20 next-bug triage (main: "the next open bug in your goal's list"): none left in my lane that use found and
  no one owns. verify_current and the NFS purge are already fixed (bug-claims.md struck); the completion record's
  framing is g262's; the svnserve_t red is A-761's (w-svn told). The jupiter fleet six are filed in run-requests.md.
  Asked main which to take.
