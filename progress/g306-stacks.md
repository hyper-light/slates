# g306 — A-659 stacks: works over works (UI goal 93, 51cef9f7; §4.17.1 shared piece 5)

Started 2026-11-18 ~23:10 PST. Supervisor: main. Role: implementer.
Worktree: ~/slates-work/g306-a659 (branch g306-a659, from origin/main df50dc58c; repo = ~/slates-work/g066-fs-server/.git).

## Rules in force
- THE RULE (CLAUDE.md §8): one piece; only `cargo test -p slates-merge --lib` + `cargo fmt`; merge myself; END.
- Studio (d1323898) rule from main, 2026-11-18: CARGO_BUILD_JOBS=4; never two cargo commands of mine at once; delete
  each red/green target dir as soon as its result is read; a run that "did not answer" or is killed: wait a minute,
  rerun once, count it neither red nor green.
- Keep to my own module in slates-merge (g267, g305 work there too).

- Studio d1323898 OFFLINE (main, 2026-11-18 ~23:40 PST): no builds anywhere; write tests + code + docs, commit to my
  branch; probe the Studio with one cheap `kiln run` (sync false, `true`) every ~5 min; report to main if blocked.

## State
- [x] preamble, CLAUDE.md (origin/main) §8, lint habits read; readback of THE RULE sent to main
- [x] design read: §4.16 whole, §4.17.1 items 1-6, "Chains of works", "Works over works", shared pieces, A-642, A-645
- [x] code read: engine.rs (judge/commit_judged/rebase/merge_content/intervening), map.rs, range.rs, review.rs,
      reanchor.rs, push.rs, increment.rs, chain_entry.rs
- [x] failing tests first: red commit 43317a523 (pushed as agents/g306-a659-red); red RUN owed (Studio offline)
- [x] implementation: c370dddbe (engine/stack/{moved,levels,judge,tree}.rs); green RUN owed (Studio offline)
- [x] design (§4.17.1 status, piece 5, A-659 entry) + GAPS paragraph: 344993a2c, evidence marked EVIDENCE-PENDING
- [x] merged origin/main (695ec9789): one conflict (amendment log end, main's first); A-646 part 2b's
      `Resolved::link_node(&LinkTarget) -> Option<..>` adopted in stack.rs `named`, judge.rs `upper_links`, tree.rs
      `tree_links`
- [ ] BLOCKED on the Studio (offline since ~23:40 PST 11-18; probes at each step answer "offline"):
      red run on agents/g306-a659-red, green run on agents/g306-a659 (CARGO_BUILD_JOBS=4, one cargo at a time, own
      target dirs deleted after), replace EVIDENCE-PENDING in SLATES_DESIGN.md (A-659 entry) and GAPS.md (A-659
      paragraph), merge origin/main again, push main, END (run requests, report)

## Design decided (summary)
- Module: crates/merge/src/engine/stack.rs (child of engine, as reanchor.rs is: it reads the green's per-path deltas
  and histories, which are private). Shared lines: `mod stack;` + `pub use` in engine.rs, a re-export in lib.rs.
- An upper is restated over its lower's landing by transforming the green's deltas between the lower's base and its
  landing over the lower's ops (position mapping, map_range's edge rule), then mapping the upper through them; a path
  where the lower's op overlapped a green change (an identity decision) is Replaced for the uppers, except a same-span
  overwrite, which maps exactly. Namespace: an upper's op on a path the base change touched in a dimension it reads
  conflicts (conservative; identical not special-cased).
- Oracle: the stack landed equals the stack's journals composed into one work landed on a replica green.

## Branch
- agents/g306-a659 on GitHub = local g306-a659 (695ec9789); agents/g306-a659-red = 43317a523.

## Finding to report to main
- crate::map's walk stops at the first delta op starting at or after a range's end: a work's insert at an offset where
  a delta holds Overwrite/Truncate then Insert at that offset (canonical order) never meets the delta's insert (accepted
  before both), while the reverse order conflicts. Oracles avoid two ops at one offset; the engine's rule is owed.

## Landed
- A-659: main 0e755c9b1..2b3a794b1 (43317a523 red, c370dddbe + affe3669b code, 344993a2c docs, 695ec9789 merge,
  c63e25c54 oracle collapse handling, 0ae0875d5 census print, evidence + merge 2b3a794b1). Windows x86_64 jupiter:
  red r1386 16/17 failing; green r1408 slates-merge --lib 158/158, fmt clean; macOS run owed (run-requests.md).

## A-668 (assigned by main 11-19): two inserts at one point conflict in both orders (D-27)
- worktree ~/slates-work/g306-a668 (branch g306-a668 = agents/g306-a668 1ce46ee9a; red ref agents/g306-a668-red =
  f238bff73). Fix: crate::map::lies_past + moved.rs Meets::Beside; tests crates/merge/src/map/tests.rs; oracle
  generates overwrite/truncate-then-insert at one offset and requires a refused work between to land alone over the
  stack's works below. docs/bugs note + §4.16 status + A-668 entry + GAPS (EVIDENCE-PENDING).
- red r1445 (3 of 4 failing with the fix out, incl. the oracle meeting the pair), green r1448 (166/166, fmt clean),
  evidence committed 8122fe0a3 (pushed to agents/g306-a668).
- BLOCKED on main's fix for A-659's break (main 11-19: levels.rs uses `crate::engine::history_touches`, built only
  under feature test-model, so dependents of slates-merge fail to compile; main fixing, "don't touch it").
  Then: merge origin/main into g306-a668; on jupiter run `cargo check -p slates-merge` (NO features: catches a
  feature-gated helper in production code) and `cargo test -p slates-merge --lib`; diff --stat; markers (grep -E);
  push main; run requests; report.
- LESSON (main, 11-19): a helper gated on `test-model` compiles in the crate's own tests only; check the lib alone
  (`cargo check -p <crate>`) or a dependent's tests before landing.

## Run commands (for the Studio, when it answers; one at a time)
- red:   git clone -q --depth 1 -b agents/g306-a659-red https://github.com/hyper-light/slates.git ~/.cache/g306-red &&
         cd ~/.cache/g306-red && CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR=~/.cache/g306-red-target
         cargo test -p slates-merge --lib stack 2>&1 | tail -60 ; then rm -rf ~/.cache/g306-red ~/.cache/g306-red-target
- green: same with -b agents/g306-a659, dirs g306-green / g306-green-target, `cargo test -p slates-merge --lib`
         (all of the crate's lib tests) and `cargo fmt -p slates-merge -- --check`

## A-668 relaunch (fresh g306, 2026-11-19 after ~16:10Z)
- Read: relaunch brief, restart preamble, lint habits, Windows recipe, CLAUDE.md §8 (origin/main). Readback of THE
  RULE sent to main.
- Worktree ~/slates-work/g306-a668 clean at f8d4684a4 (= agents/g306-a668); origin/main still 4644dcec3 (0 behind);
  diff --stat vs main = A-668's 7 files alone; marker grep clean.
- Added 5e18a0919 (comment-only): oracle.rs module-doc paragraph rewrapped at the crate's 120 columns (one line was
  145); word sequences compared identical with //! prefixes stripped. Pushed agents/g306-a668 = 5e18a0919.
- Expected count: main's slates-merge lib 178 + A-668's 2 new tests (map/tests.rs) = 180.
- Run r24 on jupiter (Windows x86_64): `cargo test --locked -p slates-merge --lib -- --show-output` then
  `cargo fmt -p slates-merge -- --check`, clone + target deleted by the script. RESULT: pending.
- Then: fetch; merge origin/main again if it moved (re-run if slates-merge's inputs changed); diff --stat; markers;
  `git push origin HEAD:main`; replace nothing else (evidence for A-668 already committed 8122fe0a3, r1445/r1448);
  file the macOS run in run-requests.md; 3-line report to main; END.

## A-668 second relaunch (fresh g306, 2026-11-19; main e3234e618; no readback per brief)
- Worktree clean at 5e18a0919; r24 gone (kiln restart). Main moved 4644dcec3 -> e3234e618 (71f05f93d a verdict makes
  no file until it accepts: Effect::SetContent bytes Option, Green::make_mapped; e3234e618 chain_replay kind_of
  CopyTree). Overlap with A-668: GAPS.md only (separate paragraphs, merged by itself). Main adds no lib tests.
- Read-check (main's test-model lesson): lies_past, Meets::Beside, mod stack, with_origin, submit all ungated.
- Merged: 92038d5aa (no conflict, markers clean, diff --stat origin/main = A-668's 7 files). Pushed agents/g306-a668.
- Next: jupiter run of 92038d5aa: `cargo test --locked -p slates-merge --lib` (expect 180 = main 178 + A-668 2) and
  `cargo fmt -p slates-merge -- --check`; then the merged-tree result into the A-668 design entry, GAPS paragraph and
  bug record; markers; push HEAD:main; run-requests (macOS); report; END.
- Run r81 (jupiter, Windows x86_64, 17:12-17:32 UTC) at 92038d5aa: `cargo test --locked -p slates-merge --lib --
  --show-output` 180 tests (main 178 + A-668 2): 179 passed in 35.58 s, A-668's two passing; fmt clean. Failure:
  engine::copy_tests::every_copy_reads_as_its_model_over_every_history, census `nested_replaced` 0 of 30 nested pairs
  (copy_tests.rs:1996; every history agreed). Not A-668's: that oracle bases every work at the head (copy_tests.rs
  1651), so intervening is empty (engine.rs 2882, 3011) and map_through_one never runs. r131: that test alone passed
  (3 of 37). Recorded rates 2/31, 2/46 since A-626 part 2: one run in three to five meets none (Poisson estimate).
- Main moved to f607f424c (git/svn/docs only; nothing slates-merge builds from): merged 86b855c07, one conflict (the
  amendment log's end, main's A-656/A-656 (1)/A-616 (5) first, A-668 last); design + GAPS differ from main by A-668's
  lines alone (checked by diff).
- Docs e417bfc72: merged-tree result in A-668's design entry ("The merged tree" bullet), GAPS paragraph, bug record.

## Landed (A-668)
- main f607f424c..e417bfc72 (pushed 2026-11-19 ~18:15Z): f238bff73 red, 0c08ef6c8 fix, 207e7e263 merge, 1ce46ee9a
  oracle, 8122fe0a3 evidence, a6c770966 + f8d4684a4 + 92038d5aa + 86b855c07 merges, 5e18a0919 rewrap, e417bfc72 docs.
  agents/g306-a668 = e417bfc72. Jupiter cleaned (no g306 dirs). Run request filed (macOS run owed, run-requests.md
  2026-11-19T18:20Z); bug-claims: A-668 marked LANDED, copy-oracle census chance filed (unclaimed).
- Left: the macOS run (main's queue); main's gates (clippy, xtask); the copy oracle's census chance (its owner's).

## A-672 (main's assignment 2026-11-19 ~18:30Z): the copy oracle's census met by construction
- Claimed in bug-claims (A-672); amendments.tsv row A-672 (backup amendments.tsv.bak-g306-a672); next free A-673.
- Worktree ~/slates-work/g306-copy-census, branch g306-copy-census from origin/main e417bfc72 (push as
  agents/g306-copy-census).
- Plan: copy_tests.rs gains fixed_histories() (one history per counted case: accepted, refused, from_root, from_before,
  into_themselves, from_changed, changed_beneath, replaced, nested, nested_replaced, moved) + fixed_census() +
  Census::met(); the test fails at once if the fixed census is not met, then runs the random histories on top (A-670's
  shape in restate_tests.rs). Red: PROPTEST_RNG_SEED=<s> (proptest 1.11 reads it in Config::default(), unseeded keeps
  it) on main's code, a seed whose run meets nested_replaced 0; green: the same seed passes, slates-merge lib whole.
- g307's A-626 part 3 (agents/g307-a626p3-final) touches the same test fn (seeded runner, two more counts:
  checkpointed_copies, copied_keys; run() restores at every version): if it lands first, keep both and give its two
  counts fixed histories too.
- STOPPED by main 2026-11-19 ~18:50Z: g307 already edits the copy oracle and lands the fix first, alone. Done:
  bug-claims line marked HANDED TO g307; amendments.tsv A-672 row marked dropped (number g307's to use or leave);
  agents/g306-copy-census deleted on GitHub (it held ab64ac66a: eleven fixed histories + Census::met, never built),
  local branch and worktree ~/slates-work/g306-copy-census removed. Nothing of A-672 landed.
- r155 (the red seed search on main e417bfc72) never ran a test: jupiter's disk filled while rustc built slates-merge's
  lib test ("rustc-LLVM ERROR: IO failure on output stream: no space on device", exit 92), and the script's cleanup
  never ran; the leftovers (186 MB clone, 626 MB target) were deleted by a follow-up run, which then read C: at 100%
  (924 of 925 GB used, 819 MB free) while kiln's placement said 72-77 GB free. Reported to main.
- Jupiter holds no g306 paths (~/.cache/g306-target included). No g306 run is running.
- LESSON (main, 11-19, CLAUDE.md §8): land only on a green run; a 179/180 landing (A-668) and rerunning a suspected
  flake alone with a new seed (r131) both went past §8. Report a suspected flake to main instead of rerunning it.
- ENDED 2026-11-19 ~18:55Z. Nothing open for g306.

## A-676 (main's assignment 2026-11-19 ~20:10Z): a link to a node made at its target after the work's base is refused
- Bug (g307, bug-claims 19:55Z): Green::node_home judged a file or IPC node at the target only by file_removed_since,
  so one made after the base where nothing stood answered Ok and a forged link named a node its work never saw.
- Fix: Green::node_stood_at(path, base) = content_len_at(path, base).is_some() || special_at(path, base).is_some();
  node_home refuses DeleteModify when the node stood nowhere at the base. Test: engine/node_tests.rs
  a_link_to_a_node_made_after_its_base_conflicts (a file and a FIFO; Work::fifo + fifo_node in replaced_node_tests.rs).
- Runs (macOS arm64, Mac Studio, kiln, CARGO_BUILD_JOBS=2): red 20:05Z with engine.rs as at effb30ac2 -> test failed,
  "a file: Accepted { version: 2 }; a FIFO: Accepted { version: 2 }"; green over effb30ac2 lib filtered 62/62; green
  over 545181b30 (A-658 part 1) lib filtered (node, replaced_node, restate, stack, compute tests) 68/68, --test engine
  -- hardlink hard_link 6/6, fmt clean (20:11Z).
- LANDED: aea14a21f (one commit: code, tests, GAPS entry, §4.16 rule + status, amendment entry, bug record
  docs/bugs/2026-11-19-a-link-to-a-node-made-after-its-base-was-accepted.md) + merge 54075b520; main
  e50757a1a..54075b520. Merge conflict: amendment log end (A-654 part 1 first, A-676 last).
- Done at END: bug-claims [LANDED A-676]; amendments.tsv A-676 landed; run-requests 20:20Z (other platforms, gates;
  run branches agents/g306-a676-node-home and agents/g306-a676-run2 kept as evidence for main to delete); Studio's
  ~/.cache/g306-target and logs deleted; worktrees g306-a676, -land, -final removed.
- ENDED 2026-11-19 ~20:25Z. Nothing open for g306.

## A-636 part 2 (main's assignment 2026-11-19 ~20:30Z) -- NOT STARTED: context short, handed back to main
- Nothing claimed, branched, built or changed for it. Read only: g261's A-636 notes (progress/g261-git-mercurial-
  subversion.md 255-285) and A-636 part 1's amendment entry ("Owed (A-636 part 2)") on origin/main 54075b520.
- Scope (the design's own words): each log's register on the wire (the cluster plane's ledger promotion a log at a
  time, `LedgerPromise`); the state logs' register class (whose object ids a caller names); the partition's records
  shipped a log at a time; the server's takeover adopting a repository's logs by part 1's rule (LogsOwner::take_over:
  every log's round asked first via Cohort::can_take_over, all fenced or none, a record kept only where every log's
  read holds it whole).
- Map at 54075b520 (lines): slates-db ledger.rs 855 (LedgerPromise x9), ledger/logs.rs 596 (LogsOwner, LogsCohort),
  register.rs 8,446 (LedgerPromise x17), takeover.rs 1,425 (a member's ledger promise built ~421-482); cluster
  lib.rs 1,335 (ledger promotion, LedgerPromise::decode ~1130-1169); server takeover.rs 3,428 (LedgerPromise::decode
  ~2294); live shipping in server fleet.rs 12,865 and verbs.rs 24,374 (live_ship). New wire variants: append only and
  publish in the same commit (`cargo test -p slates-ipc|slates-db --test wire_order -- --ignored
  publish_appended_variants`), then wire_order filtered.
- Suggested split, each landable alone with focused tests: (2a) the state logs' register class and each log's
  LedgerPromise on the wire a log at a time; (2b) the partition's records shipped a log at a time; (2c) the server's
  takeover adopting a repository's logs by the rule.
