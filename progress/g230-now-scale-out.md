# g230 -- progress (rewritten 2026-11-18 from my own context after the ~/.cache wipe)

- Role: Claude subagent of the slates supervisor, implementer. THE RULE (Ada, final): ONE piece, --lib unit tests
  (a bug's failing test first, red-checked once), cargo fmt, one-crate clippy + `cargo xtask check`, merge, END.
- Builds: Mac Studio d1323898 via kiln (sync false, a GitHub clone of a pushed branch, own CARGO_TARGET_DIR under
  ~/.cache there). Never on this MacBook.

## Earlier goal (UI 78, scale out; kiln dbe13f39) -- what landed (all on main)
- A-322..A-325, A-329 (ca216dbf8); A-338 (1e82ebda1); A-343 (9da036412); A-344 (4cbdcb5fc); A-346 (e7cae3f15);
  A-347 (1dce59dbd); A-353, A-354 (a2693b1eb); A-371 (07b9bd7cb); A-377 (28a590343 + repair d02505d3b); A-382
  (d13a991ba); A-398 (a4a9ef83e, with docs/wip/models/InstallInTurns.tla and its matrix); A-409 (1932dd0fd);
  A-415 (c1708fd1c = cc2eb0f01 + unit test 2849bc104; Studio r73 green 0.75 s, red with the publish removed).
- Left on that goal (none started; main assigns): hang paths (i) fence-triggered round restart and (ii) a round made
  again after the successor's warm restart (both re-prepare adopted objects above the epoch they are written at;
  recommendation: a writer with each acceptor position, model first, TLC filed before code); f = 1 live log over a
  shrunken holder set (A-272 (ii)); A-271's key-gap window at f = 1; install images sealed in turns; the second whole
  entry after every first build; takeover question waiting for its session (A-349 owed); A-338's routing leftovers;
  AC-8.9 fsx whole states.
- Studio leftovers that are mine: ~/.cache/g230-src-* clones, ~/.cache/slates-work-target-g230{,-whole}.

## Current piece: A-487 (UI goal 85, kiln 17ad60b6; bug 2), started 2026-11-18
- Bug: crates/server/tests/fleet.rs `a_volume_moved_to_another_member_is_served_there_and_its_old_owner_forwards`:
  the move back to its creator never finishes; answers alternate `Moving { to }` and
  `Refused { MoveRefused { reason: Unplaced } }` for the whole deadline. Failed 2 of 2 on the quiet Studio at main
  3022c6894, passed once at 2a5bf7264. Suspects between: A-436 (write log), A-437 (owner-location round), A-435 (an
  adopted move's seal waits in its owner shard's time, moves.rs). Main's bisect r341 running.
- g225 is fixing moves::seal_adopted's double seal (A-488) in the same file: merge carefully.
- DIAGNOSED (from the failing run's counters, ~/.cache/slates-gate/fleet-q1.log on adas-mac-studio-3, and the code):
  c refused every head/catalog record of the volume's owner b `Unauthorized` (8,006 = b's head.short 4,000 +
  catalog.short 4,006) and 2,663 content puts; b began 223 handoffs, finished none; no node counted
  `fleet.move.followed`. Cause: A-291 ran `moves::follow_moved_objects` at the end of the install's whole pass; A-398
  (mine) cut that pass into a per-copy walk and its merge 9c5fca527 left the call at the end of
  `takeover::resolve_held_object`, after its early return for a live owner, i.e. every moved object: no copy ever
  followed a move, so c kept the creator's authority and the move back could never place its head off b. Not A-435 /
  A-437 (A-398 predates both bisect points; the one pass at 2a5bf7264 unexplained). Bug 1 (A-485) may share it
  (hypothesis, for main's rerun).
- Fix (uncommitted in ~/slates-work/g230-a487, branch agents/g230-a487-move-back): `moves::follow_moved_object` per copy
  (whole pass `follow_moved_objects` now cfg(test), the oracle), called by `fleet::install_on_copy` between the lineage
  and the authority reconcile; removed from `resolve_held_object`; the cfg(test) `resolve_held_objects` ends with the
  whole follow (A-291's order). Test `fleet::tests::a_holders_copy_of_a_moved_object_follows_the_move_as_the_install_walk_reaches_it`.
  Docs: design A-291 status + A-398 status + A-487 amendment written; GAPS + bug record next; then Studio run.
- Committed fe785b0f3 (fix + unit test + design status/amendment + GAPS + bug record
  docs/bugs/2026-11-18-a-holders-copy-never-followed-a-move.md), pushed as agents/g230-a487-move-back. Studio d1323898
  r18 (clone ~/.cache/g230-src-a487, target ~/.cache/slates-work-target-g230, logs ~/.cache/g230-logs/a487-*.log):
  fmt --check, focused --lib tests (new test, moves::tests, the 8 daemon whole-pass-oracle tests + takeover's
  a_round_the_retirement_bound...), clippy -p slates-server --all-targets, cargo xtask check, then red (walk's follow
  call removed). Then: fetch, merge main (g225's A-488 in moves.rs), diff stat, push.
- 2026-11-18 (main's order): g079's A-485 (940f00615, on main 5fef5b1ef) is the same rule as my fe785b0f3, so mine is
  DROPPED (branch agents/g230-a487-move-back left unlanded; Studio r18 stopped). Bug 2 has A-485's cause. New worktree
  ~/slates-work/g230-a487b, branch agents/g230-a487-regression from main 5fef5b1ef: f5438c6e5 = my unit test only
  (`fleet::tests::a_holders_copy_of_a_moved_object_follows_the_move_as_the_install_walk_reaches_it` + helper
  `walk_the_install`), GAPS "Closed by A-485 too" bullet, a section in A-485's bug record, a short A-487 design
  amendment (no rule change). Studio r26: fmt, the test + A-485's test, clippy -p slates-server, xtask check, red with
  A-485's follow call removed. Then merge, diff stat, push, END.

## LANDED on main (2026-11-18): A-487 at cd8e7bcfe -- END of this piece
- f5438c6e5 (regression unit test `fleet::tests::a_holders_copy_of_a_moved_object_follows_the_move_as_the_install_walk_reaches_it`
  + helper `walk_the_install`; GAPS "Closed by A-485 too"; a bug-2 section in A-485's bug record; a short A-487 amendment,
  no rule change), 55ad57cc9 (docs: the runs), merge cd8e7bcfe (amendment-log conflict, main's A-488.. first).
- Bug 2 had A-485's cause (g079's 940f00615): no holder's copy followed a move after A-398's walk. My own fix of the same
  rule (fe785b0f3, branch agents/g230-a487-move-back) was dropped, unlanded, at main's order.
- Evidence: Studio r26 on f5438c6e5 (main 5fef5b1ef + the test): the test and A-485's green (0.77 s); red with A-485's
  follow call removed (routed to the creator, the new owner refused Unauthorized, followed 0). Not rerun on the merged
  tree (main's A-488 came in with the merge; filed). fmt clean. r26 also ran clippy (clean) and xtask (6 literals, not
  mine) before the rule changed again; no further builds.
- Filed: run-requests.md (bug 2's fleet test on cd8e7bcfe; the unit test on the merged tree).
- Studio: ~/.cache/g230-src-a487b clone and target ~/.cache/slates-work-target-g230 are mine (delete when main allows).

## Current piece: A-513 (UI goal 85; A-488's third sibling), started 2026-11-18
- recovery's `rebuild_remote_works`: its answer can be lost after the work ran, and its docs accept leaving a silent
  shard's works unbuilt. Judge under §4.16 first (can a work stay unbuilt after recovery without losing a client's
  change or wedging its green?); if not, record the outcome where the work runs (as A-488, A-505 did), failing unit test
  first; if so, write the judgment + evidence into design and GAPS with a unit test pinning it. Only
  `cargo test -p slates-server --lib <filter>` + `cargo fmt`; merge; END.
- Judgment (step 1): the acceptance does NOT hold under §4.16: a work left unbuilt by a lost answer is unserved for the
  daemon's life (verbs NotFound: every acknowledged change out of reach, against A-207 and A-247's "unserved until it
  has"), and its floor stays (reconcile keeps a floor of a work not rebuilt), holding its green's checkpoint; and the
  pass `take`s the whole list, so one lost answer costs every such work of the shard. So: fix.
- Fix committed d8565bbdd (worktree ~/slates-work/g230-a513, branch agents/g230-a513-remote-works): `verbs::unbuilt_works`
  (read, never taken), `install_replayed_work` ends the record and installs once, `daemon::ask_once_more` (second ask
  after a lost answer), `rebuild_remote_work` split out, owed works logged; test
  `verbs::tests::a_remote_works_rebuild_asked_again_after_a_lost_answer_finds_the_work_and_installs_it_once`; design
  §4.16 status + A-513 amendment, GAPS (class A siblings), bug record. Studio r114: green, then one red (both functions
  reverted in the throwaway clone). Then merge, diff stat, push, END.

## LANDED on main (2026-11-18): A-513 at d50fac5b9 -- END of this piece
- d8565bbdd (fix + unit test + design §4.16 status + A-513 amendment + GAPS + bug record
  docs/bugs/2026-11-18-a-remote-works-rebuild-lost-every-work-of-a-shard-to-one-lost-answer.md), c910efd14 (docs: the
  run), merge d50fac5b9 (amendment-log conflict, main's A-508.. first). amendments.tsv: landed.
- Judgment: the acceptance did not hold under §4.16 (a lost answer left works unserved for the daemon's life, their
  acknowledged changes out of reach, their floors holding their greens' checkpoints; the `take` lost a whole shard's
  list). Fixed: the list read never taken, every ask made once more, the install ends the record (installed once).
- Evidence: Studio r114 (debug): `verbs::tests::a_remote_works_rebuild_asked_again_after_a_lost_answer_finds_the_work_and_installs_it_once`
  green 0.74 s; red once with both functions reverted (listed_again [], second install charged 96 bytes again, the
  marked work dropped). cargo fmt clean. Nothing else compiled.
- Still owed (GAPS): a work whose asks go unanswered twice stays unbuilt until the next start; a retry after admission,
  and `Unpublished` rather than `NotFound` meanwhile, are not built.
- Studio: clone ~/.cache/g230-src-a513 and target ~/.cache/slates-work-target-g230 are mine (main may delete).

## Current piece: A-491 (UI goal 85 part 01a1f12cc15570268b7999919d896f91), started 2026-11-18
- Recovery's boot give-back slice is judged by the wall clock; judge it by the shard's own measured turn and the work
  it gives back (the per-iteration budget rule). g083 found it; the server recovery suite's
  a_boot_give_back... case failed once under load (adacachy r271). A --lib test with a controlled clock, red first.
- Studio scratch deleted at the start (g230-src-a513, slates-work-target-g230; the older ones were already gone).
- A-491 committed e6d44b905 (worktree ~/slates-work/g230-a491, branch agents/g230-a491-give-back): `daemon::GiveBackRate`
  (slice learned from the shard thread's own CPU via `slates_rt::registry::calling_thread_cpu`; steps a coarse clock
  shows none for folded into the next), `return_dead_recovery_timed` / `purge_write_log_timed` take their clock;
  `ShardState::give_back_rate` replaces `give_back_slice_bytes`; test
  `daemon::tests::a_give_back_step_learns_its_slice_from_its_own_cpu_not_the_wall_clock` (controlled CPU clock; expect
  page, 2, 2, 2, 6 pages); design A-226 note + A-491 amendment, GAPS (the 2026-11-08 open entry closed), bug record.
  Studio r147: green, then one red (wall clock again). Note for main: `cargo fmt` on main reformats
  crates/server/src/shards_route.rs (a test's remove_on_owners call); not mine, left as main has it.

## LANDED on main (2026-11-18): A-491 at c650f0b92 -- END of this piece
- e6d44b905 (fix + test + design A-226 note + A-491 amendment + GAPS + bug record
  docs/bugs/2026-11-18-the-give-back-slice-was-learned-from-the-wall-clock.md), 1aafdc760 (the test's
  slates_rt::futures path), docs commit (the run), merges up to c650f0b92. amendments.tsv: landed.
- Evidence: Studio r154: `daemon::tests::a_give_back_step_learns_its_slice_from_its_own_cpu_not_the_wall_clock` green
  0.76 s; red once with the steps wall-timed again (slices 21,239 then 82,598 bytes where the CPU clock gave 16,384,
  32,768, 32,768, 32,768, 98,304; 16 KiB pages, quantum 3,889 ns). r147 had not compiled (the test's `futures` path).
  Only that --lib test + rustfmt on my files.
- Filed: run-requests.md (the recovery give-back test quiet and under load; Windows coarse-clock note; shards_route fmt).
- Studio: clone ~/.cache/g230-src-a491 and target ~/.cache/slates-work-target-g230 (delete at the next piece's start).

## Current piece: A-537 (UI goal 85; g071's A-507 left two things open), started 2026-11-18
- (1) Linux busy unmount: `fusermount3 -u` refused answers `Unmount` with its exit code; judge by the kernel's mount
  table before and after (as A-507 judges macOS): still listed after a refused -u -> typed `MountFault::Busy`.
  (2) Every busy refusal names the processes holding the mount, from the kernel's own tables on this cold path:
  Linux /proc/<pid>/fd + cwd/root links on the mount's device; macOS proc_listpidspath / proc_pidinfo (libproc.h);
  bounded by what the kernel lists, typed refusal at the bound. Man pages/source read and cited. Failing --lib tests
  first; pure judgment tested on every host. Compile only `cargo test -p <crate> --lib <filter>` + fmt (Studio, or a
  capped adacachy unit for Linux-only tests). Wait for runs inside the turn.
- Studio scratch deleted at the start (g230-src-a491, slates-work-target-g230).

## LANDED on main (2026-11-18): A-537 at 49455b775 -- END of this piece
- f9b638b72 (change + tests), docs commit (runs), merges up to 49455b775 (main took contracts 43 (A-535) and 44 (A-520)
  meanwhile, so A-537 is CONTRACT 45; GAPS' A-507 entry repaired by hand in the second merge). amendments.tsv: landed.
- What: Linux plain `fusermount3 -u` refused while the table still lists the mount -> `MountFault::Busy`; every Busy
  carries `MountHolders` (new `slates_bridge_oci::mount_holders`: /proc fds + cwd/root statx'd with AT_STATX_DONT_SYNC,
  by device, bounded by pid_max/nr_open, `PastBound` past them, unreadable counted). macOS: NOT read, by judgment
  (xnu fill_vnodeinfo runs vn_stat; proc_listpidspath stat(2)s the path: a hard NFS mount whose daemon has gone waits
  forever) -> `HoldersUnread::NoReader`.
- Evidence: Studio r216 (bridge-oci 4/4, api mount:: + contract 14/14, red once without the Linux rule), r231 + r245
  (api 15/15 on merged trees); adacachy r219 (Linux 7.2, capped unit): bridge-oci 4/4 incl. the real /proc read.
- Filed: run-requests.md (kernel-mount busy test on Linux + macOS; slates-api Linux build/tests; xwin check; kiln
  downstream note).
- Studio: clone ~/.cache/g230-src-a537 + target ~/.cache/slates-work-target-g230; adacachy: ~/.cache/g230-src-a537,
  ~/.cache/slates-work-target-g230, ~/.cache/g230-tmp (delete at the next piece's start).

## Current piece: A-558 (UI goal 85; g225's inventory), started 2026-11-18
- §4.15's `verify_current` re-points an untouched inode to another file found at its name (GAPS:18090); A-151's rule
  detaches the inode and gives the name a new one. No test reaches the path. Failing --lib test first, then make
  verify_current follow A-151. Scratch on the Studio and adacachy deleted at the start.

## LANDED on main (2026-11-18): A-558 at cf17cae5d -- END of this piece
- f3db4d6c5 (fix + unit test + GAPS + amendment + bug record), b37ace8d1 (the test accepts the opened file's digest
  where the replacement landed after the digest's last check: at call 19 the digest rightly named the file it
  verified), docs commit (runs), merge cf17cae5d. amendments.tsv: landed.
- What: `verify_current` answers `Current::{Verified, Replaced}`; on an identity mismatch `leave_name` applies A-151
  (release_detached, listing invalidated, name dropped), counted `DigestStats::replaced`; an inode the listing already
  detached answers Replaced (was NotFound); `digest_begin` looks once more; `reopen_verified` -> DigestUnverified.
- Evidence: Studio r29: red once with the old re-point (call 4: the open handle read the outsider's file, name kept its
  number, replaced 1); r33: green. Only that --lib test + rustfmt on base.rs.
- Filed: run-requests.md (vfs base/base_identity, base host suites).
- Studio: clone ~/.cache/g230-src-a558 + target ~/.cache/slates-work-target-g230 (delete at the next piece's start).

## Current piece: A-560 (B4, UI goal 85; bug-claims.md), started 2026-11-18
- crates/bridge-winfsp/src/reply.rs (SAMPLES = 15, A-212): each receive worker sets its spin window once at start to
  the median of 15 channel wakes, unbounded, no agreement check; a noisy start (CI arm64: 143.9 / 108.1 us against
  4.1-11.5 us) spins that long for good. Make it learned and bounded: an estimator over the worker's own wakes that
  keeps adapting, an agreement check before trusting, a bound derived from the machine and the measured wake; no fixed
  sample count unless format or a cited statistic. Pure module tested on every host, failing first; Windows wiring
  written by me, compiled by main. Studio scratch to delete first (kiln was not connected at the start).

## LANDED on main (2026-11-18): A-560 at 7608869fe -- END of this piece
- ba48ab017 (pure `bridge-winfsp/src/spin_window.rs` + tests; Windows wiring: `reply::Stamped`, `reply::start`,
  `await_reply` learning on parked waits, `Served::send` stamping, `MountThread::start` reading the shard's step quantum,
  `Worker::start((bound, ceiling))`, `run_mount((requests, wake, ceiling))`; the 15-sample start probe removed; GAPS +
  amendment), docs commit (run), merge 7608869fe. amendments.tsv: landed.
- Rule: RFC 6298 (alpha 1/8, beta 1/4, seed S=R V=R/2, band S+4V) over the worker's own parked wakes for its life; window 0
  until V < S/2; window <= the starting shard's step quantum (0 off a shard).
- Evidence: Studio r51: spin_window 4/4; red once (unbounded, unchecked): 144,000 ns past the 50,000 ns ceiling after the
  first wake. The Windows half was NOT compiled by me (filed: xwin check, Windows lib tests, hop A/B, WinFsp CLI suite).
- Studio: clone ~/.cache/g230-src-a560 + target ~/.cache/slates-work-target-g230 (delete at the next piece's start).

## Current piece: A-562 (B9, UI goal 85; bug-claims.md), started 2026-11-18
- `everything_told` (crates/bridge-fuse/src/channel.rs, A-141) and the journal gather (crates/bridge-core/src/delivery.rs)
  each build a whole round over everything they name in one turn on the serving thread, against A-158's bounded-work
  rule. Make both build in slices under the shard's per-iteration budget, resuming where a slice stopped; a round that
  outgrows a turn is a counted signal; non-vacuity counter for the sliced path; failing --lib test first (a round over
  more names than one slice allows takes more than one turn and loses nothing). Studio scratch deleted at the start.
- 2026-11-19 02:25Z state: written and pushed on `agents/g230-a562-sliced-rounds` (60d630382; main 8cb6bef1a merged in;
  worktree ~/slates-work/g230-a562). bridge-core: `Bridge::invalidations` takes `more`; `Coherence::deliver` slices the
  gather per record (`Delivered::cut`, `owes`, `take_sliced_counts`; `served_own_request` only after a whole round).
  vfs journal: per-segment running charges so `note_round` costs a segment at most (each slice is measured).
  bridge-fuse: `OverrunRound` (root, nodes, names; `again` lap; `told_before`), `everything_told` and
  `invalidate_everything` gone; channel.rs: one `TurnSlice` per turn shared by takeover/gather/overrun, `owes_round_now`,
  `states_fed_through`, `told_through` bounded during an overrun, `slice_ns: Option` (None = whole for harnesses).
  server: fuse.rs `owes_round_now` + yield between empty slices + `copy_slice_ns` + `fuse.rounds_outgrown`/`slices_resumed`;
  winfsp.rs sliced gather + `Turned::Continued` + `winfsp.*` counters. Tests: delivery.rs
  `a_round_over_more_names_than_a_slice_allows_...`, coherence.rs `an_overrun_round_over_more_names_...`,
  `falling_behind_again_mid_lap_...`, proptest `every_item_held_at_an_overrun_...`, journal.rs
  `the_charge_after_any_sequence_crosses_segments_exactly`. Folded main's clippy finds (reply.rs allow, base.rs read_no).
  NOT YET RUN: kiln disconnected for every session on the MacBook since ~02:15Z (main confirmed); main's watch resumes me.
  Run plan (Studio, fresh clone of the branch, CARGO_TARGET_DIR=~/.cache/slates-work-target-g230): vfs `journal::`,
  bridge-core `delivery::`, bridge-fuse `coherence::tests::kernel_holds`, then the three sed red checks. Then merge, push,
  amendments.tsv landed, run-requests (draft ~/slates-work/g230-a562-runreq.md), 3-line report.
- 2026-11-19 02:55Z LANDED A-562 on main cbf2038ef (rule ff16bed94; merges with main's A-548, A-573, A-567, A-570,
  A-555, A-564, A-568, A-571; only this piece's 19 files in the stat). Studio: r260 green (vfs journal 13/13, bridge-core
  delivery 1/1, bridge-fuse kernel_holds 5/5) and red three ways; r304 after the A-567 merge, bridge-fuse coherence 17/17.
  amendments.tsv A-562 landed; run-requests.md entry filed (Linux channel.rs + server fuse.rs compile, Windows winfsp.rs,
  integration suites, CLI overrun tests). Open in GAPS: the gather's base-plane half and the byte-state feed run whole.
  Sibling (not fixed): a restored mount of an unrecovered volume (A-509) keeps its takeover owed on change turns
  (`refuse_unrecovered` returns Idle without `refuse_unserved` clearing it), so `wait_for_work` answers at once each pass.

## Current piece: A-583 (UI goal 89, slates federates across kiln runners), started 2026-11-19 02:44Z
- Write g267's two owed fleet tests in crates/server/tests/fleet.rs (or a sibling), beside the three-daemon tests:
  (1) A-544: a captured clone whose runner dies is landed by the successor against its capture (and the second case:
  killed before its first snapshot is placed, the successor serves an empty tree and refuses the landing `Unsupported`);
  (2) A-565: a live-shipped captured clone moved to B lands on B against its capture (the new owner gets the mark).
  Integration tests main compiles and runs; any pure helper gets --lib tests, failing first. Studio scratch deleted.
- 2026-11-19 03:20Z LANDED A-583 on main d03f5a014 (cef038d11 tests + GAPS + design entry; merge). Two integration tests
  in crates/server/tests/fleet.rs beside A-291's captured-clone move test, uncompiled by rule:
  a_captured_clone_taken_over_after_its_owners_loss_lands_against_its_capture (A-544, both cases) and
  a_moved_live_shipped_captured_clone_lands_on_its_new_owner_against_its_capture (A-565). No lib helper needed.
  run-requests.md has the exact `cargo test --locked -p slates-server --test fleet -- --exact <name>` lines.
  Still owed (GAPS): A-503's own move test (a to b, then b to c); not assigned.

## Current piece: A-589 (UI goal 90 95101ef3, federation across runners), started 2026-11-19 03:30Z
- A-503's own fleet test in crates/server/tests/fleet.rs beside A-583's: a captured clone moved a->b, then b->c. After
  each move: content and catalog record whole on the new owner; the previous owner answers current bytes (forwarded)
  or a refusal by kind, never stale data; a write after the second move lands at c; the move counters moved. Then
  (A-503's request) it lands on c against its capture. Integration test, uncompiled by rule; worktree
  ~/slates-work/g230-a503 (branch agents/g230-a503-move-twice).
- 2026-11-19 03:55Z LANDED A-589 on main 5f4325a87 (fast-forward; only fleet.rs, GAPS, design log in the stat). Test:
  a_captured_clone_moved_twice_is_whole_on_each_new_owner_and_never_stale_on_an_old_one. A-583's landing assert now
  takes its expected target. run-requests.md has the exact commands (this test plus A-583's two again). Process note:
  the stat and the push ran in one shell call this time; keep the push a separate command.

## Current piece: A-600 (UI goal 92 51cef9f7, git piece 2 rest), started 2026-11-19 04:10Z
- crates/git/src/object_name.rs (+ src/tests/object_name.rs, fixtures/objects with make-object-fixtures.sh, run on the
  MacBook with git 2.50.1 Apple Git-155): Kind (4 types, pack-only names refused by kind), Header read/write under the
  admission within MAX_HEADER_LEN 32, ObjectFormat both ways, ObjectId (hex either case), Hasher (SHA-1 via
  sha1-checked 0.10.0 configured as git_SHA1DCInit: detect on, safe hash off; SHA-256 via aws-lc-rs), name_of.
  sha1-checked checked: authors RustCrypto Developers, repo RustCrypto/hashes, pure Rust (digest/sha1); git's own t0013
  shattered-1.pdf (v2.50.1, SHA-256 2bb787a7...) is byte-identical to its test copy; shattered-2 from shattered.io
  (d4488775...). Fixture cuts: 320 bytes each (they differ at 192..319 only), plain SHA-1 f92d74e3... for both.
  Branch agents/g230-a600-git-objects; Cargo.lock to come from the Studio run (no cargo on the MacBook).
- 2026-11-19 03:21Z (e85c17f9f's commit time; first written 04:40Z in error) LANDED A-600 on main e85c17f9f (aa5dd45fb + 32b5af55e lock + merge). Studio r438 green 22/22 and
  red detect-off and pack-only; r444 `--locked` green 22/22 and red admission. sha1-checked approved on main's three
  conditions, each checked (RustCrypto's; pure Rust; git's t0013 shattered-1.pdf identical and refused). Lock: only the
  new packages (the Studio's extra windows-sys re-pointings left out). run-requests.md filed (clippy, xtask, doc,
  cross-target check).

## Current piece: A-604 (UI goal 92 51cef9f7, git piece 14 first part), started 2026-11-19 ~03:25Z
- crates/git/src/v0.rs (+ src/tests/v0.rs, fixtures/v0 with make-v0-fixtures.sh and SHA256SUMS, made on the MacBook
  with git 2.50.1 Apple Git-155, two later runs byte-identical): the ref advertisement (caps after the first ref's NUL,
  capabilities^{} for no refs, v1's `version 1`, peeled lines folded into their tags, shallow lines), ClientLine (want
  with caps, have, done, shallow, deepen, deepen-since, deepen-not, filter, ERR), ServerLine (NAK, ACK plain/continue/
  common/ready, ERR), ShallowUpdate. Readers take an optional LF and either-case ids (gitprotocol-pack's MUSTs);
  writers write git's per-line form; unknown caps kept by name. object_name gains null/is_null/write_hex and the shared
  decimal reader/writer. Fixture fix on record: the first capture of the deepen fetch held its tag-following second
  round; the tee now refuses a second upload-pack, the fetch runs --no-tags.
- 2026-11-19 04:50Z LANDED A-604 on main 22e1612fb (four merges as main moved; only this piece's 47 files in the
  stat). Studio: red first 24 of 25 v0 tests failing against a refusing codec; green 203 of 203 slates-git --lib over
  the merged tree 32a99481c; fmt clean; nine mutations each caught, control 25/25. Trap recorded: red and green
  checkouts in one CARGO_TARGET_DIR made green run red's binary (cargo clean -p fixed; separate dir after).
  run-requests.md filed (clippy, xtask, doc, cross-target). Studio scratch ~/g230-src-a604 and target dirs
  ~/.cache/slates-work-target-g230{,-red} to delete at the next piece's start.
- For main: the sibling v2 module (A-582 part 2) decodes losslessly (records each line's LF, refuses upper-case ids by
  gitprotocol-common's HEXDIG); v0 follows gitprotocol-pack's "MUST treat obj-id as case-insensitive" and "MUST NOT
  complain" about a missing LF, writing git's form back. lib.rs's crate doc still says v2's messages, deltas, indexes
  and packs are "A-582's next parts" though they have landed (their owners' sentence, left as is).

## Current piece: A-624 (UI goal 93, git), started 2026-11-19 ~05:00Z
- Part 1 (main's answer to A-604's question: read ids as git's server does): v2's `line::object_id` takes either
  case, writes lower; lines keep their recorded LF. Failing first (2 of 39 v2 tests), then slates-git --lib 221 of 221
  over main; LANDED f68d32006 (worktree ~/slates-work/g230-a624, branch agents/g230-a624-git-v2-rest).
- Part 2 next: object-info (size) and bundle-uri over store traits (A-614's store now lives in crates/git/src/store),
  the packfile-uris choice (inline prefix, URI-tiled suffix: git's client indexes the inline pack before it fetches
  any URI, fetch-pack.c), golden vectors from stock git 2.50.1 (object-info has no stock client in 2.50.1: scripted
  stateless requests; bundle-uri has one: clone with transfer.bundleURI over file://). Lesson: escape [B]/[C] in docs.
- Part 2 LANDED 599ac2782 (2026-11-19 05:35Z): v2/object_info.rs (ObjectSizes; Stored over A-614's Repository rows),
  v2/bundle_uri.rs (Bundles, BundleList; lines read as git's client reads them), v2/packfile_uris.rs (choose: longest
  tiled suffix, fewest packs; eligibility = protocol then ':' + sideband-all + self-contained unless thin-pack);
  request.rs reads both commands; fixtures/v2-rest (30 files, two runs alike). Red first 21, green 292, merged 326
  (+2 main's ignored); red ten ways by mutation. Process slip: one `git checkout-index` on the Studio scratch clone
  (harmless there, unneeded; not repeated). Merge met A-614 part 2's Incoming::depth (test set it to 0).

## Current piece: A-644 (UI goal 93, git piece 11: protected branches), started 2026-11-19 ~06:00Z
- Design decided (from principles, recorded in the amendment entry): the store answers, against each ref's own green
  alone (A-642), how a move changes it (fast-forward by generation numbers, no walk where they decide: gen(new) <=
  gen(old), new != old) and which commits it brings (D(k)'s; a create's whole history; Gitea's `--not --all` rejected
  as a global reference and a hole); linear history = no merge commit among them; signed commits = each brought commit's
  signature split as commit.c's parse_buffer_signed_by_header (fixed A-590's payload: any header beginning gpgsig),
  format by armour, SSH through a CommitSigners verifier, OpenPGP/X.509 typed as a later piece's. git's words: git's
  own non-fast-forward / deletion prohibited; hook declined after a hook's words in copy_to_sideband's 128-byte pieces
  (Declined::hook added). Golden vectors: 12 stock git 2.50.1 pushes to a protected server (fixtures/protected).
- Part 1 LANDED 8b3297fe0 (2026-11-19 ~07:20Z): red first 13 of 15 (r982), green 353 (r984), merged 431 (5f12ed3ea),
  ten mutations caught (r1012), fmt clean. Main's A-620 part 2 split receive into check/decide and a Judge with
  append; the golden test now admits the pushed objects between check and decide, as part 2 will.
- Process slip: the red Studio scratch clone was moved with `git switch --detach` (r982); never again — `git archive`.
- Part 2 next: A-622's record gains the requirement rules (merge commits, unsigned commits as rights a rule denies;
  unruled = not required) and the trusted commit signers; vcs_policy's admit asks them lazily and refuses typed
  (PushRefusal); words map to receive::protected; signed_work verifies under namespace `git`. Crates: db, ipc, server.
- Siblings for main: store::graph::reaches (A-640) is a third copy of the fast-forward rule and reads parents where the
  generations are equal; A-620's test GitPolicy walks parents by parsing commits.
- Part 2 built on agents/g230-a644-protected (2026-11-19 ~08:10Z): red 1c6ec1f1e (db 2/2, server 6/7 new failing),
  green ec49e9814 (db 140, ipc 74, server 621 + one pre-existing failure, fmt clean; r1125), docs 6aa4a45e9, eight
  mutations caught (r1169 + the guard's rerun). Merge a2e705f46 with main (A-648's hooks_env appended first, signers
  after; set_policy takes rules, hooks, hooks-env, signers; A-629's LFS admit given brings: None). The merged tree's
  run r1211 was stopped when main announced the Studio out of memory and then offline; owed: rerun db/ipc/git/server
  --lib under CARGO_BUILD_JOBS=4, one cargo at a time, then land, then delete ~/.cache/slates-work-target-g230{,-red}.
- Found beside it (for main): server `deadlines::tests::a_status_capture_is_held_the_failover_delay_from_its_capture_
  not_its_request` fails on main 8b3297fe0 3/3 alone on the Studio (load 10-16), and 5/5 on this branch; a hook_driver
  test (`a_subversion_hook_starts_with_its_own_environment_alone`) failed once under the M7 mutation run (load).
- Process slips this piece: `git switch --detach` on the Studio red clone (r982); a stray `git checkout -b` (no name,
  refused by git, nothing changed) in the worktree. Neither repeated.

## Relaunch 2026-11-19 ~16:15Z (fresh g230; the predecessor stopped by the usage limit)
- Found: agents/g230-a644-protected at d7490c07a (pushed; = 330e5b829 + a third merge of main 4644dcec3, which is
  still main), red branch agents/g230-a644-red a3ebe5ebd (stubs over d7490c07a; never to merge). The progress lines
  above stop at a2e705f46; after it: 0af61f547 (git tests' generators without prop_oneof!/unwrap, lint habits),
  3df847134 (server test names A-648's hooks-env), cde0f1230 merge, 330e5b829 (requirement test takes its case's name
  by value), d7490c07a merge. Crates touched: slates-db, slates-ipc, slates-server, slates-git (tests only).
- New tests: db `a_requirement_is_held_by_git_alone`, `a_policy_is_charged_its_trusted_signers`; server
  `git_protect::tests::{a_commit_git_signed_verifies_under_gits_namespace_alone,
  every_protected_push_is_answered_through_the_policy_with_the_bytes_git_answered, a_push_no_requirement_names_is_applied}`,
  `vcs_policy::tests::{a_requirement_binds_only_where_a_rule_imposes_it, signed_commits_are_judged_under_the_records_signers,
  a_linear_history_is_judged_before_signatures_and_a_store_refusal_is_typed}`,
  `repository::tests::protections_and_trusted_signers_are_a_git_repositorys_policy`.
- jupiter run r27 (script ~/slates-work/g230-a644-jupiter-run.sh; logs $HOME/.cache/g230-logs there): main's ipc lib,
  red (db 2 + server 7 filters), green (fmt x4; db, ipc, git, server --test-threads=4). Expect: red db 2 failing,
  server 6 of 7 failing (`a_push_no_requirement_names_is_applied` the control); green db 155, git 462 (4 ignored),
  server 594 passed with exactly the baseline's 23 failing, 3 ignored.
- Then: docs line for the runs, fetch + merge main, diff stat, markers, push HEAD:main; amendments.tsv landed;
  run-requests (macOS run owed); delete jupiter's g230-green, g230-target-green, g230-logs.

## Relaunch 2 2026-11-19 (fresh g230 again; brief: rerun the set on jupiter with ONE target dir ~/.cache/g230-target)
- Merged origin/main e3234e618 (A-586's CopyTree, slates-merge's verdict-makes-no-file) into the branch: 818b84f14,
  clean, markers none, stat 22 files all this piece's; pushed agents/g230-a644-protected 818b84f14. slates-server
  depends on slates-merge, so green runs over the merged tree.
- jupiter r93 (script ~/slates-work/g230-a644-jupiter-run2.sh): main's ipc lib, red (a3ebe5ebd: db 2 + server 7
  filters), green over 818b84f14 (fmt x4; db, ipc, git, server --test-threads=4), the server's failures compared with
  the 23-name baseline; main's server lib beside only if green fails beyond it. Target ~/.cache/g230-target kept;
  logs ~/.cache/g230-logs there.
- Main's order mid-run (CLAUDE.md section 8, binding): only `cargo test -p <crate> --lib <filter>` for what changed +
  fmt; red-over-stubs dropped (a feature, not a bug); no reruns; don't wait on a queued machine; land as one piece, END.
  r93 stopped in its red server step. It had given: main's slates-ipc lib on Windows at 9a7339e9e, 39 passed, 3 failed
  (rendezvous::platform::tests::{a_claim_dropped_unanswered_gives_its_slot_back,
  a_claim_under_a_live_anchor_waits_out_a_restart_slower_than_the_old_reconnect_budget,
  a_rendezvous_has_a_holder_while_its_anchor_or_its_own_daemon_lives}), 2 ignored -- the first Windows ipc baseline;
  red db 2 of 2 new failing over the stubs.
- Merges 818b84f14 (main e3234e618), b3998bc55 (9a7339e9e), cf01ab03b (92ffda926): design conflicts only (amendment
  log's end, then also §4.17.2's statuses), resolved main's first, each checked as the exact union by diff against
  both parents. Sibling found by reading every RepositoryPolicy literal: main's A-661 part 1 svn_hooks/tests.rs (Unix
  only) built the record without `signers`; fixed fe2533225 (+1 line); its compile is owed to main's Unix run.
- r196 (script ~/slates-work/g230-a644-jupiter-green.sh) over fe2533225: fmt x4; db `repository::`; ipc `protocol::`;
  git the four changed test modules; server `git_protect:: vcs_policy:: signed_work:: repository:: lfs::
  vcs_http::tests::git` at 4 threads (two of main's 23 baseline failures fall under repository::).

## Relaunch 3 2026-11-19 ~19:15Z (fresh g230; Studio; Ada's rule: focused unit AND integration tests, then land)
- Landing worktree ~/slates-work/g230-a644-land, branch agents/g230-a644-land (pushed): ONE commit 099267884 (the
  piece, squashed from agents/g230-a644-protected fe2533225 with `git merge --squash` on a fresh worktree from main
  cd84e3811; the amendment's "Applied" list corrected: Cargo.lock never changed, git tests/lfs/svn_hooks/vcs_http named;
  a Tests sub-bullet with r14/r31) plus merges of main: b08ff96cc (b1926d201), 63a8d12a6 (a6306963e; slates-vcs
  line shared with A-657), fdc5ee606 (e8ce55a83; vcs_ssh/tests/site.rs gains `signers: Vec::new()`), 069bed125
  (787f5948b), 5b8260eac (af520142f; vcs_policy imports take slates_vcs::wildmatch, A-671; one slates-vcs comment).
  Each design/GAPS resolution checked as the exact union (the piece's 94 design + 20 GAPS lines over main).
- Studio runs (macOS arm64): r14 over fe2533225: fmt x4 clean, db repository:: 23/23, ipc protocol:: 5+1 ign, git 4
  modules 49/49, db+ipc wire_order every_ 2/2 each, signed_work (signed_merge signed_landing trusted_signers
  signature_decides) 8/8, server lib did NOT compile (main's PackRefused::Stalled break, fixed by main 93bd6e807);
  r31 (scratch patch only) server lib 52/52 + vcs_http::git::tests 10/10; r52 over b08ff96cc: everything green
  (server 62/62); r64 over 63a8d12a6 green (server 58/58, filter repository::tests::); r76 over fdc5ee606 and r94 over
  069bed125: server 63/64, the one failure main's own: repository::tests::each_verb_moves_the_names_log_and_each_members_version_its_own_log
  ("not refused: GreenCreated", a branch at a version the root should have folded) fails identically on main 787f5948b
  (r94); passed at 63a8d12a6 (main a6306963e). Suspects: A-626 part 3 / A-651 part 1 (hypothesis; load 15-23 when it
  failed vs 7-12 when it passed). r112 over 5b8260eac running.
- Studio scratch: ~/g230-src-a644 (clone), ~/.cache/g230-target, ~/.cache/g230-logs; ~/slates-work-g230 and
  ~/.cache/g230-tla are older g230 scratch (left).

## LANDED on main (2026-11-19 20:58Z): A-644 part 2 at ee5cbed4c -- END of this piece
- One commit 099267884 (the piece, squashed from agents/g230-a644-protected fe2533225; the amendment's "Applied" list
  corrected, a Tests sub-bullet with r14/r31) plus merges of main as it moved, the last ee5cbed4c (main 8727ff426).
  Landing worktrees ~/slates-work/g230-a644-land (agents/g230-a644-land, superseded at fc417bc7d/0df9e8a8e, never
  landed) and ~/slates-work/g230-a644-land2 (agents/g230-a644-land2, from fc417bc7d; landed).
- Merge adaptations to main's newer code: hg_push (A-657 part 4) `Prepared::asks` brings None and `policy_refused`
  arms for MergeCommit/UnsignedCommit/Unjudged (unreachable for Mercurial, answered NOT_AUTHORIZED); empty `signers`
  in the test records of hg_push (x2), vcs_http/tests/hg.rs, vcs_ssh/tests/git.rs, vcs_ssh/tests/site.rs; vcs_policy
  imports the glob from slates_vcs::wildmatch (A-671); one slates-vcs dependency line with both comments.
- Evidence (macOS arm64, Mac Studio): final r5 over 2c6c3d6f4 (= ee5cbed4c outside crates/svn, docs): server --lib
  filters 92/92 incl. the 7 new, signed_work 8/8, fmt clean; r169 db 24/24, ipc 5+1 ign, wire_order 2+2; r183 git
  49/49. Each merge's docs checked as the exact union (the piece's 94 design + 20 GAPS lines over main).
- Main's failures met on the way (filed in run-requests): the PackRefused::Stalled compile break (fixed 93bd6e807);
  repository::tests::each_verb_moves_the_names_log_and_each_members_version_its_own_log failing on main 787f5948b and
  3329211e0, passing again from 7d020df47 (A-626 part 3 continued (1)).
- Filed: run-requests.md (2026-11-19T20:58Z entry); amendments.tsv A-644 row: part 2 landed.
- Studio: clone and ~/.cache/g230-target deleted; ~/.cache/g230-logs (a644-*) kept as evidence; ~/.cache/g230-tla and
  ~/slates-work-g230 are older g230 scratch, left for main to judge.
- Left on the goal (GAPS owes, not started): receive-pack's server half asking all of this per command (A-620 part 2's
  judge over the store), its hooks (A-622 part 3); Mercurial's linear history (A-641); OpenPGP and X.509 commit
  signatures; a push's commits judged in slices under the shard's budget.

## Current piece: A-629 part 2's rest (UI goal 93; Git LFS on the shared HTTP edge), started 2026-11-19 21:20Z
- Brief: LFS's Access in slates-server (policy per ref, the asking credential's expiry) and its routes on git's HTTP
  site (batch API, transfers, locks) through the edge's authentication (A-625 part 3). Failing tests first; focused
  tests (lfs, vcs_http's git site, credentials) on the Studio; land; END.
- Found: main already has part 2b-ii (24c2fc933: lfs::answer, LfsAccess, LogLocks), so the rest is the edge's routes,
  the status decided after the body, and the daemon's LFS service. A predecessor g230 had left UNCOMMITTED work in
  ~/slates-work/g230-a629 (branch agents/g230-a629-lfs-edge from main cb9d7f5a5): http-edge StatusMode +
  Connection::reply_status (+ test), server vcs_http Exchange::reply_status, git site LfsExchange/SiteExchange/
  answer_lfs/lfs_gate, lfs::answer over VcsStores (Unheld), slates-git Answer::fields. Kept as safety commit
  605936e03, pushed. Not yet: site LFS tests, lfs/tests.rs on the new answer signature, the daemon's LfsService
  wiring, docs. Landing = one squashed commit on a fresh branch from origin/main.
- 2026-11-19 ~22:00Z (predecessor): squashed the piece as ONE commit 1cdb47fc7 on a fresh worktree
  ~/slates-work/g230-a629-land (branch agents/g230-a629-land, pushed) from 8188f1214's tree. Studio over 1cdb47fc7
  (logs ~/.cache/g230-logs/a629-land-*.log): fmt clean, http-edge connection:: 18/18, git lfs:: 73/73, server vcs_http::
  lfs:: credentials:: 55/55, cli an_anchors_vcs_http_flag_starts_its_daemons_edge 1/1. Decision on record: the daemon's
  ShardLfs serves the locks API; batch API and transfers answer 503 at their head until LFS objects are durable on the
  owner shard (an upload must never be acknowledged that a restart forgets, §4.17.1). Then began merging main f05af5288
  (A-654 part 3; conflicts cli.rs + daemon_edge.rs resolved) and was interrupted ~22:10Z.

## Relaunch 4 2026-11-19 ~22:14Z (fresh g230; brief: A-629 part 2, land it; completeness first)
- Merge f05af5288 committed 60a17fd60 (resolution checked: DaemonSite = main's halves + ShardLfs; the CLI test's LFS
  request rides main's io::Result bare_request via edge_request). Merge b94ae6e90 committed 16d6baf20 (amendment log's
  end only: main's A-678 + A-653 (4) first, then A-629 part 2c; exact union checked). Stat vs main: the piece's 14
  files alone. Pushed agents/g230-a629-land 16d6baf20.
- Studio r258 over 16d6baf20 (clone ~/g230-src-a629, target ~/.cache/g230-target, logs ~/.cache/g230-logs/a629-l2-*):
  fmt, edge connection::, git lfs::, server vcs_http:: lfs:: credentials::, cli the LFS edge test + main's restart test.
- Next: push HEAD:main (own command), amendments.tsv (part 2c landed), run-requests, 3-line report, END.
- Found by reading (main's, not chased): the slates-server non-test build warns `unused import: UploadServices`
  (vcs_http/git.rs re-export; the daemon serves upload-pack Unserved since A-654 part 3), seen in the cli build log.
- Left of A-629 after this: LFS objects durable on the owner shard (an LFS record in the repository's log, replayed by
  recovery into an LfsObjects index beside A-662's git store), then batch API + transfers served from it.
- Studio r258 (over 16d6baf20): green (fmt; edge 18/18; git lfs 73/73; server 57/57; cli 2/2). r287 (over db87d1f9b, main
  e25799e8c merged): green (fmt; server 57/57; cli 2/2). Merges 8511bb5c1 (99068c566), d438d5f47 (550153532),
  4c52ddd8e (e8fbe9116; its auto-merge message rewritten before any push) each docs-only conflicts.
- 2026-11-19 ~23:00Z merge 460c29c22 (main 693bc8feb): A-681 part 1 (Subversion DAV site, 022866602) had landed the
  SAME mechanism (a status named as the reply begins) in http-edge + vcs_http. Made one: A-681's sync
  `Exchange::reply_status(&self) -> Status` (default 200) + my per-route StatusMode and content rule in
  `Connection::reply_status` (streamed route takes 200 alone); svn route = StatusMode::AfterBody; A-681's edge test on an
  AfterBody route; my edge test gains a streamed route naming its own 200; design entry + GAPS updated. Pushed.
  Studio r346 over 460c29c22 running (fmt 13 files, edge connection::, server vcs_http:: lfs:: credentials::, cli 2).

## LANDED on main (2026-11-19 ~23:50Z): A-629 part 2c at ba97c5ad5 -- END of this piece
- Piece commit 1cdb47fc7 + merges 60a17fd60..ba97c5ad5 (16 merges as main moved; main 574185afa..ba97c5ad5). Branch
  agents/g230-a629-land = ba97c5ad5. Superseded, unlanded: agents/g230-a629-lfs-edge (8188f1214, the pre-squash work).
- What: git's site routes <repo>/info/lfs (route refusals, 401 + LFS-Authenticate, owner shard, 404, 403, 503 for an
  unserved endpoint); served LFS status decided after the body; daemon ShardLfs = locks API over the locks log; batch
  API + transfers 503 at their head until durable LFS objects (decision on record: no upload acknowledged that a
  restart forgets, §4.17.1). lfs.rs's dead code on main is wired (ShardLfs calls lfs::answer/LogLocks/LfsAccess).
- Merge adaptations worth knowing: A-681's status naming and this piece's made ONE mechanism (460c29c22: A-681's sync
  Exchange::reply_status, this piece's per-route StatusMode + content rule; svn route AfterBody; streamed routes take
  their own 200 alone); A-680 part 2a's ToolSites (DaemonGitSite carries ShardLfs; Decided::map carries the mode);
  A-682's upload halves (DaemonServices = Halves<UploadServices<ShardStores>, ReceiveServices<ThisShard>>); A-685's
  CLI request helper (answer_head) replaced this piece's edge_request.
- Evidence (macOS arm64, Mac Studio): r433 over 258f45e70 (fmt 13 files clean; server vcs_http:: lfs:: credentials::
  65/65; cli 2/2), r397 over b2955148c (edge connection:: 19/19), r258 over 16d6baf20 (git lfs:: 73/73). Later merges
  touched no code of the piece (reasons in each merge message).
- Filed: run-requests.md (2026-11-19T23:50Z entry); amendments.tsv A-629 row: part 2c landed (backup
  amendments.tsv.bak-g230-a629p2c).
- Studio: clone and ~/.cache/g230-target deleted; ~/.cache/g230-logs kept (245 MB of logs, main may delete).
- MacBook disk: my scratch design copies (235 MB) deleted mid-landing at main's disk warning.
- Left of A-629 (next piece, unassigned): durable LFS objects on the owner shard (an LFS record in the repository's log,
  replayed by recovery into an LfsObjects index beside A-662's git store; slates-git's lfs::objects already indexes
  them over the shared ContentStore), then the batch API and the transfers served from it; locks by non-owners (A-635's
  policy check); the stock git-lfs client end to end (main's).
- Found by reading (main's, not chased): slates-server's non-test build warns `unused import: ShardRepositories`
  (vcs_http.rs re-export from A-651 part 4).

## Queue from main (2026-11-19 ~23:55Z), each its own piece (one commit + merge, failing test first)
1. A-629 part 3: durable LFS objects: an LFS record in the repository's log, replayed into an LfsObjects index beside
   A-662's store, so an acknowledged upload survives a restart; then batch API + transfers served. g300 adds a
   Mercurial record kind to the same log (A-687): merge, don't duplicate.
2. Stock git-lfs end to end: CLI integration test (stock git + git-lfs pushes a repo with LFS objects through a
   daemon's edge, a fresh clone gets them back; skips loudly without git-lfs on the Studio); repo via `slates
   repository create`; Bearer via `-c http.extraHeader` until A-686 lands, then Basic.
3. Non-owner locks (A-635).
- Disk care: MacBook ~5-6 GiB free; check `git diff --stat` for unexpected deletions before every commit.

## Piece 1 of the queue: A-629 part 3 (durable LFS objects), started 2026-11-20 ~00:05Z
- Branches: agents/g230-a629p3-lfs-objects (a5cf4d063 piece + fmt fix; worktree ~/slates-work/g230-a629p3) and the
  landing branch agents/g230-a629p3-land (ONE squashed commit aa2f5cd05 over main 20ae700a7; worktree
  ~/slates-work/g230-a629p3-land). The squash met main's A-652 part 3 in push.rs (`brings_a_record`): kept both rules.
- What: vcs_store/lfs_record.rs ("SGLF" record + hostile tests); GitStore::lfs (LfsObjects) + release; replay_into
  dispatches LFS records (replay_lfs); admit_lfs/LfsAdmitRefused/lfs_refusal_of/lfs_held/lfs_bytes; push.rs LfsUpload,
  push_lfs, enum Stored {Git, Lfs} (A-687's shape: g300's agents/g300-a687-import-durable adds Stored::Imported; merge
  = union of variants), sealed_record; lfs.rs ShardObjects + NoLocks + answer(state, call) choosing seams by endpoint;
  ShardLfs serves every endpoint; slates-git LfsObjects::release.
- Evidence: Studio r498 over a5cf4d063 (main be892650b): red 2 of 6 lfs tests failing (objects never kept), then green
  git lfs 74/74, server lfs:: vcs_store:: vcs_http:: repository::push 101/101, cli 1/1; fmt fixed per its diff.
  r525 over aa2f5cd05 (merged tree) running.
- Next: on green, fetch/merge main, stat, push HEAD:main; amendments.tsv A-629 row (part 3 landed), run-requests, report.
- Found (main's, not mine): non-test server build warns `method bytes is never used` (state_log.rs:122, A-636 part 2b)
  and `unused import ShardRepositories`.

## Relaunch 5 2026-11-20 ~02:00Z (fresh g230; brief: the queue's three pieces; completeness first)
- Found the landing worktree mid-merge of main 3b392dcb1 (A-681's lender), the design log's end conflicted. Studio logs
  (~/.cache/g230-logs on the Studio): a629p3l-* (over aa2f5cd05) fmt clean, git lfs 74/74, server 107/107, cli 1/1;
  a629p3m-* (over 4713e6261, main a7b5e4292 + A-686) server 111/111, cli 1/1. r525 itself was collected.

## LANDED on main (2026-11-20 ~02:10Z): A-629 part 3 at b63f8a05d -- piece 1 of the queue done
- Piece aa2f5cd05 + merges 4713e6261 (a7b5e4292) and b63f8a05d (3b392dcb1; amendment log's end, main's A-681 lender
  entry first, exact union 53,416 = 53,367 + 49). Stat vs main: the piece's 12 files alone. Fast-forward push.
- amendments.tsv A-629 row: part 3 landed (backup amendments.tsv.bak-g230-a629p3); run-requests.md 2026-11-20T02:15Z entry.
- Superseded branches (never to land): agents/g230-a629p3-lfs-objects (a5cf4d063/24bb4fad2, the pre-squash work).

## Piece 2 of the queue: stock git-lfs end to end (CLI integration test), started 2026-11-20 ~02:15Z
