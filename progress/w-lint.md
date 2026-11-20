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
