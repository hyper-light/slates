# w-lint progress

## 2026-11-19 round 0
- Machine: Adas-Mac-Studio-2.local (kiln runner b24c5c95; 14 cores, 36 GB, 460 GB disk), 14-17 GiB free: under the
  20 GB guard. None of the space in use is w-lint's.
- So: a blobless clone at ~/slates-work/w-lint (210 MB) here; clippy, cross lints and xtask go through a kiln run with
  needs disk=60g (r1, at origin/main 7f81be683), CARGO_TARGET_DIR=~/.cache/w-lint-target on whichever machine it lands on.
- Commit signing (kiln-git-sign) refuses this session: "only a process of an agent session kiln runs here signs".
  Ada OK'd this one progress commit unsigned; landings on main still need signing fixed or Ada's OK.

## 2026-11-20 round 0, landings so far (commits unsigned: Ada's OK, kiln-git-sign refuses this session)
- Disk recovered to 66-119 GiB free, so builds run here: clone ~/slates-work/w-lint, target ~/.cache/w-lint-target;
  helpers have worktrees ~/slates-work/w-lint-{svn,git,merge,hg} with their own targets (delete when done).
- Landed on origin/main (each confirmed with merge-base --is-ancestor):
  - 7c91c0a32 slates-vcs (8); 91d85df6e slates-ipc + slates-rt cfg(test) split (9, cross targets);
    2b0ea20bc slates-db (13 + 3 literals); d1ccb1ad2 xtask literals follows #[path] test modules (125 false hits in hg);
    7de1f32b1 + 75eb6527a slates-http-edge (6); 7e5eab88e slates-vfs (3).
  - helpers: 058481ce8 slates-merge (46); 5828c1457 slates-ssh (9); 64300b31e, 7975f5cd7, dc3044069 slates-svn
    (17 + 22 + 3, 89 literals).
- In flight: slates-git helper, slates-hg helper. Left for me: slates-server (24 literals + clippy once deps clear),
  vcs/src/share.rs literal, then the full wall again (host, 6 cross targets, xtask check incl. unsafe, fmt).
- For main: the literal check reads a byte char like b'3' as the number 3 (doesn't strip char literals).

## 2026-11-20 round 0, continued
- More landed: 9ab4e4e4c slates-hg (29, helper); 5e9693ac7, acf28f79f, 9f46e907b slates-git (21 + 83 + 29 literals,
  helper); f5c515f55 xtask literals: `pub(crate) mod tests` + char-literal digits (22 false hits);
  453a40005 hg Intake::finish + land open_target_in boxed (5); 7464601e1 server literals (2);
  986376c88 COMPILE FIX: main's slates-server didn't build (08b440739, A-724 3a, called a cfg(test) accessor).
- In flight: slates-server clippy, 106 spots, split by file: helper A (all but vcs_*/hook_driver), helper B
  (vcs_*, hook_driver); worktrees ~/slates-work/w-lint-srv-{a,b}. Then me: crates/server/tests, then the crates
  behind server (cli, anchor, mcp, bridges ...) as they show up.
- Running: 6 cross targets, `cargo xtask check`, host clippy --keep-going.
- For main: vcs/src/share.rs:130 `f64::EPSILON * 64.0` (4bbf7f4ba, A-724 piece 2) has no stated derivation; not
  marked by me. Skipped: land's x86_64-pc-windows-msvc cross clippy (C build scripts can't cross-build here).

## 2026-11-20 round 0, server
- Landed: 89865595c bridges/base/http tests (7, helper); cd475f43c vcs A-724 tests (5); eb1429de0 hg Rebase::begin
  boxed (1); slates-server part B 64d292a09, da3dfd2d3, 55eec54ae (48); part A 47c4506b3, a3e06a396 (60);
  8f6472947 server tests/fleet.rs (4).
- Main's disk rule (2026-11-20): ONE target dir (~/.cache/w-lint-target, --target per platform), no helper targets,
  >40 GB free between rounds. Helper worktrees and targets all deleted.
- Waiting on main: 3 server test errors need an in-place #[allow] with reason (carry.rs:497 test sleep; deploy.rs:1238,
  1245 rustls Arc in tests). Part B replaced test sleeps in vcs_* tests with park_timeout loops to pass the lint:
  main should judge that (part A declined the same dodge).

## 2026-11-20 round 0 CLOSED at 45e6cf9aa
- Landed since last entry: 495fc59dc api (17); 4e204e83f merge/git/mcp/client (7); 66fe990d3 svn compile break
  (A-726 test vs svndiff Cow) + 1 lint; ffecc5bcc cli tests (6); 45e6cf9aa server A-719 part 3 (2).
- Wall at 45e6cf9aa: 6 cross targets clean; structural, unsafe, fmt clean; host clippy fails only on 3 server test
  errors (carry.rs:497 sleep; deploy.rs:1238, 1245 rustls Arc) that need an in-place #[allow] with reason -- main's
  call; literals fails only on vcs/src/share.rs:130 (EPSILON * 64.0, 4bbf7f4ba) -- needs its author's derivation.
- Totals round 0: 583 errors fixed (549 clippy, 34 literals), 2 compile breaks, 36 commits (all unsigned, Ada's OK).
- Next: hourly rounds (fetch, ff ~/slates-work/w-lint, same commands, fix and land). One target dir only.

## 2026-11-20 main's rulings on round 0's leftovers
- c9137cb9f: deploy.rs rustls Arc test allowed in place (D-8 test-harness exception, as the other rustls tests);
  carry.rs looked_until_recorded is a RECORD_WAIT (Derived) deadline wait re-checking each look; part B's park loops
  judged: hook_driver `gone` + vcs_http `marked_within` are real condition waits (kept); the two `park_for` helpers
  (hook "sleep" role, ssh client pauses) now sleep on slates_rt's timer on a LocalRuntime (elapsed time is the input).
- share.rs:130 literal: w-balance's (A-724), skipped per main.
- Windows: native clippy on jupiter via kiln run (on jupiter, target C:\Users\corph\.cache\w-lint-target), CI's
  three Windows steps; run r2 in flight.
- Hourly rounds: session cron b32dcdb1 at :17 (session-only, 7-day expiry).
- Note: the blobless clone once reported a commit "in the commit graph but not in the object database" on fetch;
  a plain retry fetched fine (likely background maintenance racing). Watch for it.

## 2026-11-20 round 0 CLOSED with Windows, at b79227222
- d11c04473: native Windows (jupiter) clean for CI's three Windows steps: hook path gated cfg(unix) end to end
  (Answering::Hook, presenter, hook_times, Hooked id/repos, the off-unix HookCall/HookAnswer/hook_ran removed),
  Answered/Hook boxed, bridge-winfsp transact.rs takes (read_only, NameSearch) (A-551), cli winfsp tests reuse
  write_through_drive and move impersonation asserts; plus verbs.rs ParkedKind boxed + SubmitCarried, svn test clone,
  hg_made test split.
- 9d81a56e7: wire-derive (A-742) and vcs content/rows splits. b79227222: hg delta.rs + intake test, server
  vcs_http/svn commit_side, vcs_svn inspect_err, place.rs cast, cli svn_interop port helper.
- RED (not mine, fails on main's own code): slates-vcs tests::the_same_bytes_from_git_and_mercurial_are_one_content_
  held_by_both_repositories (store.get None after the git release); suspects f3429bdcd (storage seam) / e064c2dc7.
- Skipped loudly: slates-sdk-python on jupiter (pyo3-build-config: no Python 3 interpreter there).
- Hourly cron aa63302c at :17 (session-only, 7-day expiry); includes the jupiter Windows lane.

## 2026-11-20 disk trim + A-742 unsafe check (for main)
- Trimmed ~/.cache/w-lint-target 45 GB -> 17 GB (every incremental/; no release/ dirs). Rounds now run with
  CARGO_INCREMENTAL=0 and trim incremental/ + release/ after each (cron 162512f6). Box at 136 GB free.
- A-742 (e064c2dc7, merged by 80a7d5788): `cargo xtask unsafe` FAILS on main: "slates-mem: 81 unsafe sites exceed
  the budget of 79". The xtask counts 18 new `unsafe` tokens in crates/mem/src/large.rs; the budget edit names 16.
- SAFETY lines: every unsafe block in large.rs has a `// SAFETY:` comment directly above it, and the `unsafe impl
  GlobalAlloc` has one. Without their own: the four trait methods' `unsafe fn` (alloc, alloc_zeroed, dealloc,
  realloc; covered only by the impl's comment) and the three `unsafe fn unmap` declarations (a `/// # Safety` doc
  each, no `// SAFETY:` line). Not changed by me: the budget is the author's to raise and name (w-engine).

## 2026-11-20 "Waiting on main" lines: ANSWERED (main's rulings)
1. deploy.rs rustls Arcs in a test: approved as D-8 exception 2. Was already green (c9137cb9f); now in main's ruled
   form, `#[allow(clippy::disallowed_types, reason = "D-8: rustls takes Arc<..> by signature")]` with a comment
   naming the two owners (landed, see git log for deploy.rs).
2. Part B's park_timeout loops (marked_within, hook_driver `gone`): waits on an observable condition with a deadline;
   kept. Scanned test code for the dodge shape (wait out a time, then assert something did NOT happen): none found;
   doorbell.rs and closed_loop.rs pace cycles / rest a hold out, then assert on counters (events). Nothing to send.
3. Closed by main: carry.rs:497's sleep gone; share.rs EPSILON derived (Higham 3.1, share.rs:159-166).
- Handed off by main: A-742 unsafe budget (81 vs 79) to w-engine, urgent; the red content-once test to w-perf.
  Hourly rounds drop both from "known".

## 2026-11-20 round 1 (main asked: Windows lane at or after 90d405247) -- at fe4c64851..8ebdc1d1a
- 90d405247 (w-svn's off-unix run_svn_hook, 165006): compiles and is clippy-clean on Windows. Nothing to send back.
- Fixed and landed: 231ac7d11 cluster/mem/server/bridge-winfsp Windows lane (14); c37ffd11e mem resident + 38
  literals (anchor, rt, cluster, server, vcs, vfs); fff3ebaa7 git/merge/wire + new svn fsfs_view test (36; it got the
  crate-level harness allow the 329 other integration tests carry); 0907608c4 db/server/cli/mcp (8); 12f2ed442 hg
  test (1); 8ebdc1d1a cli winfsp remove_through_drive (1, Windows).
- Wall at fe4c64851 + fixes: host clean, six cross targets clean, structural/literals/unsafe/fmt clean, Windows
  (crates, cli, xtask) clean. Unsafe now 80/80 (w-engine's fix landed).
- Target trimmed after the round (21 GB, CARGO_INCREMENTAL=0); box 100 GB free.

## 2026-11-20 round 2 at 86d9e666c
- Host: 16 errors in git, svn, server, cli (+ structural: git positions.rs proptest not through slates_test_seeds;
  + 4 literals in server git_push.rs PhaseSteps) -> fixed in 9bb1558c2 and its merge (main restructured
  negotiation/objects.rs meanwhile; took main's file; boxed Begun::Selecting). Host workspace clean after.
- Six cross targets, unsafe (80/80), fmt: clean. structural, literals: clean after the fixes.
- Windows (jupiter): r11 stopped (pre-fix tree); r14 on the fixed tree in flight -- kiln server timing out on reads.
- Target trimmed after the round.

## 2026-11-20 round 3 at 562b58593 (closed at 4ce207710, clean on every lane)
- Fixed and landed: e3f012a61 (19: git, vcs, hg, server; structural A-745 suite seeds; literals; two polling loops
  as deadline waits, neither a "did not happen" wait), f23d5ac80 (2: ungated Hook arm off unix; A-697 floor test),
  22fcede45 (4: A-697 part c computed_request boxed, judge_submission split), 4ce207710 (3: svn_interop hook test
  cfg(unix); A-697 settle test).
- Windows lane now runs on a FRESH CLONE on jupiter (kiln run, sync: false, ~/w-lint-src re-cloned each round,
  same target dir): kiln's synced copy there is damaged (a read-only .git pack "Access is denied"), which gave stale
  files and false errors in r16/r17.
- At 4ce207710: host, six cross targets, structural, literals, unsafe, fmt, Windows (crates, cli, xtask): clean.
- Target trimmed after the round.

## 2026-11-20 round 4 at 26e125adb (closed on macOS at 2e521d875; Windows last seen at 982ea33d5)
- Landed: 945b5f5cb (41: svn A-751 svndiff/lz4/making, git staging, vcs, rt Windows decode, server daemon/verbs/
  vcs_store, A-697 fleet tests, cli edge restart), ecf8f9f6e (client never-looping loops x8, hg ssh test casts +
  slow-client pause on the runtime timer, two fleet tests, a literal), eed69ad0b (3: server config, ssh import, hg
  keys literal), b3fd44bda (10: A-718 git encoding's large enums and arithmetic + literals; git_encode test on a
  channel; unused pushed_large removed).
- Process notes: the blobless clone failed to merge (missing promisor object) with a local "wip-local" commit in its
  history; re-cloned FULLY into the same path (warm target kept) and applied the round's diff as one patch, so no wip
  message reached main. 03f872541 and 7113b38e7 on main are titled "wip" (Ada's commits): told main.
- Windows: r21 clone hit a GitHub reset; r22 lost to a jupiter runner restart; the run at 982ea33d5 found A-718's
  git errors (fixed in b3fd44bda, unverified on Windows); the last re-run failed (network reset, then kiln timeout).
- At 2e521d875 on macOS: host, six cross targets, structural, literals, unsafe, fmt clean.

## 2026-11-20 round 5 (Windows confirmed): main 4c2a3aede clean on Windows; 9e33fd418 clean on the macOS host
- Landed: d412275d3 (Windows at a5233e2: git_push tests' PUSH_WAIT_NS/pack_of unix-only yet used everywhere,
  give_back.rs import, client leftover; ipc hex literal), 4c2a3aede (INTERLEAVED gated as its unix test),
  9e33fd418 (cli merge_kill never-looping loop).
- At 4c2a3aede: Windows (crates, cli, xtask) clean on jupiter; six cross targets, structural, unsafe, fmt clean.
- Open, not mine: literals in merge pace.rs RUN = 16 and push.rs UNIT_BYTES = 64 KiB (A-752, 004f072d4) -- owner
  to derive. Red test slates-vcs the_same_bytes_from_git_and_mercurial... (w-perf).

## 2026-11-20 round 6 at 53a81211d (Windows clean at 441bf83; host clean after 46a065d0d)
- Landed: f89178c81 + 614a9b71d (svn A-759 index/children + drive tests, vcs A-727 content, hg, server
  content_holder/daemon/vcs_store/verbs, fleet serve_peer_records restructured into helpers with a PeerRequestServed
  value per arm; merge with A-724 8c region holds; homes.rs; svn example), 46a065d0d (Windows at 0edd12a: two
  constants gated as their unix readers; t_a654 hook steps; dead live_ship payload; vcs_svn test scratch removal).
- Tests: svn 36, vcs 13, hg 5, server 244 + 18 + 39, fleet integration 79 (discovery/forward/live/takeover): pass.
- This clone's commits now carry a local user.name/email (Ada Lundhe <adalundhe@Mac.attlocal.net>, as before):
  the hostname-derived identity stopped resolving.
- Still red, not mine: literals in merge pace.rs RUN and push.rs UNIT_BYTES (A-752, owner).

## 2026-11-20 ~11:15 main's routing
- slates-vcs tests::the_same_bytes_from_git_and_mercurial_are_one_content_held_by_both_repositories: PASSES on
  origin/main 85d66d9cb (cargo test --locked -p slates-vcs --lib <name>, macOS arm64, this Mac). No note to w-perf.
- A-752 literals (merge pace.rs RUN, push.rs UNIT_BYTES): w-engine's to derive under R3; rounds report them only.
- "wip" commits 03f872541, 7113b38e7: not rewritable (main never forced); rule now in the fleet brief.

## 2026-11-20 round 7 at bbc6aa614
- Host: svn migration/records.rs read_step (13/10) and a test cast -> 001318ae7 (chain_of, decoded_step); svn
  migration tests 45 pass on base 8ebf8a1c6. Cross targets, structural, literals (now clean: w-engine's A-752
  derivations landed), unsafe, fmt: clean.
- Windows: r32 at 32164bf showed the same svn error only (svn lib blocked the rest); re-run on c0958ff35 started in
  the background (r33).
- Ada (2026-11-20): don't block on remote/CI runs; run focused unit and integration tests here and merge. Windows lane
  now runs in the background and its findings are fixed when it ends.
- Windows r34 at 159f1c8 (r33 lost to a kiln timeout): referenced-content tests from bc6cc7ab1 (vfs tests/referenced.rs
  unwrap_in_result, slicing, cast; server vcs_store/referenced.rs cast) -> 6e5f6a53a, landed via 2d3f218cb; vfs
  referenced 6 pass, server referenced 2 pass locally.

## 2026-11-20 main's run requests (g226 / 599603559, slates-mem 80 -> 83, cross matrix)
- (2) `cargo xtask unsafe`: slates-mem 83/83 ok. The three new sites in mem/src/heap.rs (zeroed malloc_statistics_t,
  malloc_zone_statistics on macOS; mallinfo2 on glibc) each have a SAFETY line directly above.
- (3) Cross clippy matrix (ci.yml's crate set, pure-hash) on 51a061d36: x86_64/aarch64 linux gnu, x86_64/aarch64 linux
  musl, x86_64/aarch64 windows msvc all clean -- mallinfo2 compiled on both gnu triples, the None fallback on musl and
  Windows.
- (1) Native: Linux amd64 (r36) and jupiter Windows (r35) clippy + slates-mem heap test running in the background.
- Host wall on 606d0d0ec was broken by the day's landings; fixed in place: 60cc4073a (git learn/packed/answer, svn
  records/step, hg serve tests, server credentials test split, cli memory_lines, xtask's included process_wait.rs,
  Cargo.lock missing xtask's libc so --locked refused) and 111d561a0 (merge stack boxing + named_tests split, svn dav
  site no-op as_deref). recovery.rs's two sleeps (6ea16f4c5) sent to w-sizing (inbox/w-sizing-from-w-lint.md).
- xtask tests/helm_chart.rs the_render_equals_the_golden fails here; not from these changes; noted, not chased yet.

## 2026-11-20 round 8 at fe7cb5232 (landed 553e3d3ee, main a7df9ccf0)
- Host: db repository.rs apply_advanced (namespace_changes), vfs tests/pinned_chunks (pinned_extents_read),
  cli tests/v3_takeover flow test 17/10 (455fe41fb, a wip commit; split into seven helpers, and it was not
  rustfmt-clean -- fmt --check was failing on main). Literals: cluster credentials wire widths, hg tags radix,
  git store/learn level count and corpus shape. Tests here: db repository 34, vfs pinned_chunks 2, cluster
  credentials 3, hg tags 2, v3_takeover 1 under SLATES_TEST_CLI=1 (11 s). Merge conflict with d94410e74 (rustfmt of
  the same file only): kept the split version.
- Cross six targets, structural, literals, unsafe, fmt: clean. Still red: server tests/recovery.rs two sleeps,
  w-sizing's piece (inbox/w-sizing-from-w-lint.md).
- r35 (Windows) and r36 (Linux) for g226 lost to kiln timeouts; re-run on current main as r37/r38 in the background.

## 2026-11-20 between rounds (main 37c84db43 -> 0916cf5b4)
- api container_engine.rs (7e7e242f3), Windows-only `ask`: literals 4096 / 10 ms -> Shape lines (eb410fdac). Its
  10 ms `thread::sleep` poll loop is D-9-disallowed and Windows clippy (r37) refused slates-api and thus slates-cli;
  rewritten as a condition wait (pipe readers send on a channel, `recv_timeout` under the observe budget, kill past
  budget or cap, then wait) at 088e05d50 on agents/w-lint-winapi-check; jupiter check r41 before it lands on main.
- mem arena.rs (2dc414a84) walk test: needless_range_loop + 12/10 on every target -> 0916cf5b4 (arena 19 pass).
- xtask helm_chart golden fails here only because local helm is v4.1.0 and CI pins v4.3.0 (blank lines between
  documents); not a main breakage.
- r38/r39 (Linux) lost to a runner restart / no room; r40 running on the Linux runner.
- Landed on main (2e4415441): the api Windows `ask` condition wait (088e05d50; its jupiter check r41/r42 lost to kiln
  timeouts, landed per Ada's don't-block rule since Windows clippy was already red on that code), and git
  negotiation/shallow.rs mem::take + checked test casts in merge named_tests and server vcs_store/named_objects
  (git negotiation:: 30, merge named 22, server named_objects 1 pass here).
- g226 native: Linux r40 clippy on slates-mem/ipc/anchor clean on a7df9cc (mallinfo2 compiles natively); its heap
  test and every Windows run since (r37, r41, r42, r43, and a last pair) were lost to kiln: server timeouts, runner
  restarts on both 451397fc (Linux) and jupiter. Next round retries both lanes on current main.

## 2026-11-20 continued (main f727b9ec4 -> ebd9014c3): the wall clean on host and every cross target
- Landed: git negotiation mem::take + merge/server test casts; merge refine.rs (6035b016a: middle snake split into
  forward_round/backward_round over Snake, signed()/unsigned(), count_units, BaseAndPost, UnitCounts) + its tests and
  rewrite_ab example, cluster tests/sealed split (967fbee02); git store staging positions_unit + StagedRun, hg
  changeset_ab percentile in integers + helpers, git_push tests rustfmt (5c08e04e8); server attach merge_role_form,
  HashedSpan, boxed Progress::Done, mounted_work + session_log test splits, recovery.rs arena_figures Linux-only and its
  idle wait yielding (f25b946f7); cli tests without the removed CONTENT_PREFAULTED (cli test target did not compile)
  and log_view JSON test split (ebd9014c3).
- recovery.rs: w-sizing removed the sleeping test (21a306a04); the piece is closed.
- At ebd9014c3: host clippy --workspace --all-targets, six cross targets, structural, literals, unsafe, fmt all clean.
- Windows (jupiter) earlier run at 2e44154 reached the end for the first time today: blocked by merge refine.rs only
  (now fixed). Fresh Windows and Linux runs on ebd9014c3 started in the background.

## 2026-11-20 round 9 at ab760dc65 (landed, main 15d4602ea)
- db partition.rs check() 12/10 after bbc605ac9 -> nine check_* helpers; svn dav/site.rs decimal Format line and a
  needless lifetime; svn migration step test cast + match; svn examples replay_bench (slice, cast, sort_by_key) and
  changeset_ab (read_every_text); server tests/presentations superseded_refused. db lib 182, svn dav 92 / migration
  48, presentations 10 pass here.
- Host, six cross targets, structural, literals, unsafe, fmt: clean.
- Windows r54 (on ebd9014c3) still running on jupiter; Linux resubmitted (r56 had no room).

## 2026-11-20 round 10 at d4a5f9f43 (landed 35e4fd7de, merge follow-ups 3b2f12c1d, c968c7e2e; main 5313ab3cb)
- merge refine.rs (A-787 part 1: 34/30/24/15/13 of 10) split by phase into paced helpers, every charge! kept in place;
  the const byte-class table gone (class() calls classify()). Conflicted with w-rewrite-ops' 95e88882f (C2 renames):
  took their tail, re-split refine_rewrites_paced on it (lent_whole, refined_ops, spliced + others_kept, relaid_plan,
  ops_laid_into). merge lib 244 pass.
- rt (A-767): sync test's sleep -> yield, room test split, retirement Format/Derived lines, a stale From<SyncRefusal>
  the merge kept dropped. git encode_bench: Result + ? (no new allow). server: landing drop()s, svn DeltaV v1 arms,
  rebuild_mounted_slot, fleet holder-takeover split, svn_deltify + recovery waits yield. mem, svn, land, store clippy.
- recovery a_bursts_extents_go_back (A-789) fails on Studio-2 on main's own code (1,341 MiB after vs a 64 MiB bound),
  same with the original sleep: GAPS.md, not worked (Ada's order).
- store literals (56 sites, layout offsets + two Cited: consts) sent to w-store: inbox/w-store-from-w-lint.md.
- Windows r58 / Linux r60 (A-782, g226, A-786) still running/queued on kiln.

## 2026-11-20 round 11 at 5313ab3cb (landed 833144bd7)
- Host clippy, fmt, structural, six cross targets: clean.
- unsafe: slates-store had no budget entry -> `slates-store = 0` (833144bd7). slates-server 8 against 6: A-789's
  (362bd8c39) two content_pool.rs open_range sites, budget not raised -> w-sizing (inbox/w-sizing-from-w-lint-unsafe.md).
- literals: 56 sites in slates-store, w-store's (inbox/w-store-from-w-lint.md); none elsewhere.
- r58/r60 were collected by kiln's restart without results; relaunched on 833144bd7 as r1 (jupiter) and r2 (Linux).

## 2026-11-20 round 12 at 283d646b7 (landed e51d9adeb, main e79614bdf)
- merge head_under test (A-792) no longer a Result test that asserts; server work_files ChangedEntry alias + test
  initializer; verbs assemble_post_state's size is post_state_size. Tests: head_under 1, work_files 3,
  green_post_state 7.
- Host, fmt, structural, six cross targets clean. Still with owners: slates-store's 56 literals (w-store),
  slates-server's unsafe 8 > 6 (w-sizing, A-789).
- Windows: jupiter is offline; r4 asks kiln for any Windows runner. Linux r60 still queued.

## 2026-11-20 19:15: refine.rs table back (9f77462e4), main's order
- Whole refinement back at pre-round-10 cost; sliced ~1.05x left (GAPS D-O17 (f) updated). Plus refine CHUNK cast,
  refine test split, server refining.rs literals. Close note appended to inbox/main-from-w-lint-close.md.
- Windows after jupiter's return (19:37): r4 (queued by needs while it was away) started, then timed out at 90 min
  with no output at all (not even the clone's line: a hung clone or no output streamed). Resubmitted by needs with the
  warm CARGO_TARGET_DIR and a 600 s clone cap: r5 and r6 both refused "no room here now" on jupiter. Not resubmitting
  again until the next round. Linux r60 still queued.

## 2026-11-20 round 13 at e4cfbd38d (landed 7a9ace19f, 785e0a94f, 55038e039, part 3)
- main's five Windows reds: two fixed (winfsp HostFs, vcs_svn HOOK_PATIENCE_NS cfg(unix)), three already in e51d9adeb;
  `cargo xwin clippy` works on this Mac and ran the server and cli Windows check.
- Host: hg, mem, merge, rt (Rc -> mpsc), db, land, server (push boxed verdict, hg_push, content_ranges, verbs
  forward_placement, scratch-dir allows, svn_presenter), store tests and example, cli dead helper. slates-pace: no-panic
  attribute (+4 checked divisions), budget 0; literals derived/named.
- Correction to 785e0a94f's message: the rt pacing failure happened once, on my version only, under load; both versions
  then passed 3 of 3. content_ranges failed once at load 7, then passed 2 of 2 on both versions.
- To g228 (inbox/g228-from-w-lint.md): live_fleet's two time-waits, resident.rs's /proc read (structural), mem unsafe
  88 > 87. Still with w-store: 3 store literals; w-sizing: server unsafe (now ok? see next round).
- Windows run requests (main 20:05: w-sizing R2 at 20bef898c with its red-first at 20bef898c^, w-work-mount's WinFsp
  mounted-work test, w-realtools' real_tools over WinFsp): one combined run by needs "windows". r7 and r8 both ended
  "the kiln server did not answer in time" before any output came back; whether r8 runs on jupiter is unknown.

## 2026-11-21 round 14 (89269ab1b .. c8b77f817): through the network outage
- The Mac lost GitHub for hours (fetches died mid-pack, then no answer); the transport fix (encode_packet) waited and
  landed as 89269ab1b once it came back. kiln's server also dropped; r10 (A-789 Windows checks, red-first, real_tools)
  ran 5h21m with no output past its clippy stage and ended when jupiter's runner restarted: no results. Not resubmitted
  yet (main's 21:58 rule: once only, after checking).
- Landed: 4e32bd101 (db, vcs, store, merge, ipc, hg, svn literal), 5d555c7fa (server repository, svn landing,
  test_store), 6995ff750 (server recovery, ssh hg, git receive, verbs, tests), c22f468e3 (cli merge verbs, load_bench,
  merge_check, fleet). Tests run for each (listed in the commits).
- Sent: w-provision-gate (rt spin_kick time-wait; since fixed upstream), w-git (receive tests wide_connectivity),
  w-hg (push intake_tests intake): both still red on the host. w-store: 3 store literals.
- Windows run requests: r12 (the once-only resubmission, by needs "windows", each step capped) never started: jupiter's
  runner restarted while it was "starting". Not resubmitted again; main to say when jupiter is steady.

## 2026-11-21 round 15 at c8b77f817 (landed 7db3bdba6)
- merge history.rs overlay (554482004, A-803; 11/10, every target) -> unchanged_outside; history_block_tests
  int_plus_one. merge lib 253 pass. Host, fmt, structural, unsafe, six cross targets clean otherwise.
- Still with owners: git receive tests wide_connectivity (w-git), hg push intake_tests intake (w-hg), store's 3
  literals (w-store). Windows requests waiting on main's word about jupiter (r12 lost to its restart).

## 2026-11-21 round 16 at 30e89672f
- server git_derive never_loop -> one match; merge history_layers_bench Stream alias (every target); git oracle
  CommittedPush alias. server vcs_store:: 109 pass, examples and git lib tests build. Host, fmt, structural, unsafe, cross
  clean otherwise. Still with owners: w-git (receive wide_connectivity, which also blocks git's lib tests), w-hg (intake
  helper), w-store (3 literals). Windows requests held for main's word on jupiter.

## 2026-11-21 round 17 at 7e110566d
- merge push.rs (efe6d2763: deferred push records) on every target: decode split (DeferredReader::head), widths by
  size_of; push tests GreenReads alias + owed_history split; server push test split, green_ready Copy clone. merge lib
  258, server repository::push 7 pass. w-git's helper fixed upstream. Still with owners: w-hg (intake helper), w-store
  (3 literals). Windows requests held for main's word on jupiter.

## 2026-11-21 round 18 at 5b929b8d2 (landed b37c38d14, main f52934d65)
- server import_batch dead accessor removed; push tests GreenRead alias, replica_owing_the_record, staged_batch.
  repository::push 11 pass (the commit message says 9: wrong, it was 11), import_batch 2. Owners named in
  run-requests.md. r13 lost to a jupiter restart before it started (third time): asked main in
  inbox/main-from-w-lint-jupiter.md before resubmitting.

## 2026-11-21 rounds 18-19 and Windows (main 37c7a1691, landed 21ed9cd8d)
- Main 07:50: jupiter's restarts are R2 (the runner's link through the HTTP tunnel), not my runs; short runs, one check
  each, logs on jupiter, the shared target dir kept. Deleted ~/w-lint-src-1795240804 there (42 GB free after), cloned
  ~/w-lint-win at a693c49e4, ran Windows clippy: its reds were host reds of new landings (fixed in 21ed9cd8d) plus
  svn_presenter (w-reaper). `xtask unsafe` refused "no room here now" twice; the rest waits for room.
- Round 19 at 481d7abef: debug eprintlns removed from the hg replay; replay_sliced -> open_record; t_a729 split; svn
  examples handle Importing::Base; hg SEGMENT_VERSION Format; svn_commit PieceOf alias. give_up -> w-git.

## 2026-11-21 ~10:50 round on 37c7a1691 (main's 10:35 word)
- made_commit.rs give_up annotated as having no caller yet, naming w-git's C5 2b piece 3 (1cfbe14d9, main f917e1ce4);
  one-line note to w-rewrite-ops (inbox/w-rewrite-ops-from-w-lint.md). Host clean but for w-hg's intake helper;
  literals clean but for w-store's three; fmt, structural, unsafe, the six cross targets clean.

## 2026-11-21 08:20 (main's word): the wall green
- 13ded84f0 (main d612e3963): store texts.rs hunk widths derived (w-store's), hg intake helper split lint-only (w-hg's,
  told in its inbox). Host clippy and literals clean on the merged tree. Windows anchor tests (r23) running on jupiter.
- Windows A-789: anchor lib 26/26 (r23), handles_windows 9 + 10 ignored (r24) green; CLI extent tests refused for room
  twice (r25, r26), reported in run-requests.md. Stale r10/r13 stopped (not on jupiter).

## 2026-11-21 round at 8b23c6fdd (landed 689b710b8, e62705815; main 3538c014e)
- hg getbundle (AskedRows, checked divisions, Copy StoredPlace), git works test drop, merge SplitMix64 Format line;
  then the server reds hg's had hidden: svn restore push_children, commits Before::Landed boxed, svn_commit tests
  GreenTree + get(). Tests: hg getbundle 20, git works 9, merge byte_places 2, server svn_commit 30, commits 12.
  Host, fmt, literals, structural, unsafe, the six cross targets clean. real_tools (r29) still queued for jupiter.

## 2026-11-21 round at 6a6e35215 (landed dc5ab18f5, then the A-805 test fix; main a9a2393b8+)
- server judged_submission split (increment_held, chain_room_admitted, retention_secured, verdict_span_emitted,
  accepted_answer); hg ManifestWrites; merge chain_replay, server green_post_state, cli by-reference test splits; four
  literals. Then A-805's merge tests (copy_tests view_of `?` with `failed`, identity.rs harness unwrap_used + get +
  Viewed). Host, fmt, structural, literals, unsafe, the six cross targets clean. real_tools (r29) still queued.

- 10:40 main (01a1ff2e): give_up stays annotated until w-git C5 2b piece 3 lands; re-check give_up on the first round after it. r29: report to main only if red. w-sizing and w-reaper told by main.

## 2026-11-21 round at 67fa7f75a (landed 5216ce4e2, main 5216ce4e2)
- Reds (host clippy): hg intake.rs take 11/10, intake_tests.rs kept-text test 11/10, vfs volume.rs destroy 11/10 (A-827),
  then vfs tests/held_head.rs 19/10 + type_complexity. Split into note_logged, kept_bytes_are, kept_dead, imaged_as_held,
  held_bytes; HeldFile alias. Focused local: hg push:: 60/60, vfs held_head 2/2, vfs destroy 8/8.
- fmt, structural, literals, unsafe, six cross targets clean.
- w-sizing moved A-789's _on_windows CLI tests into content_extents_windows.rs (#![cfg(windows)]), so that item is closed. Host clippy clean.
- Windows-only (xwin, slates-cli's build): server verbs.rs:3020 run_recorded 11/10 -> w-hg; vcs_store/svn_import.rs:506
  14/10 -> w-svn. Owners told in their inbox files; not edited (goal-93 files under active edits; not on the CI cross set).
- Per the goal-93 focus I started no new Windows run. r29 still queued. I'm free for a goal-93 piece (inbox/main-from-w-lint-free.md).

## 2026-11-21 round at 156007dff (landed 869791f94 and 754d530e6; main 754d530e6)
- A lib error in slates-machine (A-831's division) stopped slates-server, so its own reds and every slates-cli test were
  hidden. Last round's "Windows-only" server reds were host reds too; I corrected the w-hg/w-svn notes.
- 869791f94: machine checked_div; unsafe-budget.toml slates-machine 97 -> 101 naming A-831's four macOS sites; rt test
  .first(); db derive_partition returns its Partition (dead fields after 96df5abcd); hg sha1_ab Way alias; literals in
  land os_windows (size_of::<u64>()) and server content_ranges (DAX_RANGES); server verbs serve_forward/run_recorded and
  svn_import land_owed split; content_ranges/live_ship test helpers.
- 754d530e6: svn_interop use test (22/10) in named steps; vcs_crash sweeps judge through Faults::add_each; vcs_concurrency
  hg scenarios split; git_interop/hg_interop small ones; fleet build-steps assert helper; hg take's pass-over helper;
  intake_tests PushedLine. Harness allows gained unwrap_in_result (the sanctioned crate-level form).
- Focused local: db --lib 183; machine time_constraint 1; rt time_constraint 1; server lib 81 + import 31; fleet hop
  test 1/1; hg push:: 61; cli five test targets build (61 listed).
- After the merge, the whole wall is clean: host clippy, fmt, structural, literals, unsafe, and the six cross targets.
- No new Windows run (goal-93 rule). r29 still queued.

## 2026-11-21 round at 8fb2e33db (landed a9be85e7a, main a9be85e7a)
- Reds: rt iocp.rs unformatted, and slates-rt unsafe 120/109, both from A-828's 98e150baf (Windows only). Fixed by
  cargo fmt and by raising the budget to 120 with its sites named. server upload.rs if_same_then_else: one condition,
  keeping the short-circuit.
- Focused local: server --lib vcs_http::git 20/20. Literals, structural and the six cross targets were clean.
- No new Windows run (goal-93 rule). r29 still queued.

## 2026-11-21 round at 9adb32044 (nothing to land)
- Clean: host clippy, fmt, structural, literals, unsafe, the six cross targets.
- Earlier this hour, from main's list: wire_order was red for w-svn's ImportFault::SubversionLanded; I published it in
  9adb32044. Server repository|vcs_store: 254/254 on macOS, so it doesn't reproduce.
- VCS suites lane: r30 (Linux) sat "starting" for over an hour, so I stopped it and resubmitted it once as r32 (sync false,
  its own clone ~/w-lint-vcs). r31 (Windows) is queued for jupiter. r29 is stopped on main's order; I'll re-queue it
  after w-work-mount lands.
- VCS suites per platform: none ended yet (Linux r32, Windows r31).

## 2026-11-21 round at 1a35ab091 (landed 348af0b12)
- Red: server vcs_store/commits.rs mem::forget (w-git's 926b951e6). Replaced with ManuallyDrop::new; vcs_store::commits|made
  40/40. w-git told. The rest of the wall was clean.
- VCS lane: Linux r32 running on adacachy2; Windows r31 queued, since jupiter's link is down. r29 waits for w-work-mount,
  which hasn't landed yet.

## 2026-11-21 round at d4c7e1f42 (landed 876f35ef0)
- Reds:
  - server store_tests.rs: A-835's sleep while waiting for the kill became a yield, still bounded by its deadline.
  - server vcs_ssh/git.rs: A-722's identical keepalive branch is one condition.
  - cli git_correctness.rs: read_chunked split out (12/10).
  - cli svn_interop.rs: use_slates_leg split out (11/10), plus unwrap_in_result in the harness allow.
- Focused local: server 72, git_correctness 2. Fmt, xtask and the six cross targets were clean.
- VCS lane: Linux r32 still running (adacachy2); Windows r31 queued, jupiter's link down. r29 waits for w-work-mount.
- VCS lane, round at d4c7e1f42: Linux didn't run (r30 sync stuck, r32 clone broke after 66 min; in run-requests.md).
  Windows: r31 dropped, resubmitted as r33, queued. No platform has a pass/fail count yet.
- VCS lane, Windows: r31 dropped and its resubmit r33 died when jupiter's runner restarted (in run-requests.md).
  No platform has a pass/fail count this round.

## 2026-11-21 round at ed4661d40
- Red: server git_push t_a836 at 11/10, split into tags_listed_peeled and fetched_ids; git_push 30/30. The rest of the
  wall was clean.
- VCS lane: Linux r36 dropped (adacachy didn't answer in 15 s), so I resubmitted it once as r38 (shallow clone).
  Windows r37 is queued for jupiter.
- Disk: 33 GB free after the round (other workers' builds), so I deleted my own debug target (19 GB, literal path): 51 GB free.
- VCS lane, Windows (jupiter, 19ad3d506): git/hg/svn/vcs didn't build (aws-lc-sys needs NASM, missing there; next try
  AWS_LC_SYS_NO_ASM=1). Server vcs_/git_/hg_/svn_: the process died with exit 4 when the watchdog wedged a shard under
  parallel load. The hg_push test it hit passes alone (r41), so it went to w-bugs. Linux: r38 still running.
- VCS lane, Linux: r38 (the resubmit of r36) died with its clone broken after 34 min; in run-requests.md. No Linux count yet.

## 2026-11-21 round at 39c36af9d
- Red: server tests/merge_check.rs (A-840): casts became try_from, the conflict loop is now resolved_until_rebased,
  and the harness allow gained unwrap_in_result. merge_check 11/11. The rest of the wall was clean.
- VCS lane: kiln refused the first submits ("not connected to the kiln server"). Resubmitted as Windows r45
  (AWS_LC_SYS_NO_ASM=1, server tests 4 at a time) and Linux r46 (synced from Studio-2).
- Landed e97604cbb (merge_check) and 33e41f583:
  - vcs mrc.rs: isolate_lowest_one.
  - server repository.rs: A-843's destroy test split, 24/10.
  - server idle_sweep.rs: A-838's test is cfg(unix), because it breaks slates-server's Windows lib test build.
- VCS lane, Windows (jupiter, 6cbbe5ee2, AWS_LC_SYS_PREBUILT_NASM=1; NO_ASM wants CMake): slates-git 589/0 (4 ignored),
  slates-hg 402/0, slates-svn 584/0 (3 ignored) + fsfs_view 5 + propfind_allocations 1, slates-vcs 110/0. Server
  vcs_/git_/hg_/svn_ didn't compile on Windows (A-838's test); fixed in 33e41f583, rerun next. Linux: r46 running.
- VCS lane, Windows server (33e41f583, 4 threads): the process died with exit 4 after 4 tests. The watchdog wedged
  t_a767_c5's git push task; alone it passes in 0.81 s, so it's on w-bugs (concurrency in one process, not load).
  No server count on Windows.

## 2026-11-21 round at 3a00d432d
- Reds:
  - server hg_push Landing::Unbuilt: boxed (large_enum_variant).
  - grants.rs and partition/tests.rs: precedence.
  - vcs content.rs and mrc.rs: two literals derived (size_of::<u64>(), 2^u64::BITS).
- Focused local: vcs --lib 125, server hg_push|vcs_share 36. Fmt, structural, unsafe and the six cross targets were clean.
- VCS lane: Linux r46 dropped (adacachy2 didn't answer the sync), so I resubmitted it once as r51. Windows: the counts
  from the last round stand (crates green); the server's are blocked on w-bugs' watchdog exit.
- VCS lane, Linux: r46 and r51 both failed adacachy2's sync; in run-requests.md. No Linux count today.
