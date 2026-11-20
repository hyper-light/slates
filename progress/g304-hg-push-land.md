# g304 — Mercurial pushes landed (A-657 = A-641 parts 3 and 4; kiln UI goal 93, 51cef9f7; §4.17.3 piece 5)

Supervisor: main. Role: implementer (Claude). THE RULE read back to main first (2026-11-19): only `cargo test -p <crate>
--lib` for crates touched + `cargo fmt`, on the Mac Studio (adas-mac-studio.local-2, sync false, GitHub clone of the
pushed branch, own CARGO_TARGET_DIR under ~/.cache per copy, deleted after); merge = fetch, merge origin/main, read
`git diff --stat origin/main HEAD`, grep conflict markers, `git push origin HEAD:main` alone; no checkout/switch/stash/
reset/force/commit-tree; never ~/Projects/slates, never /tmp; END with this file, run-requests.md, 3-line report.
Policy read: restart-preamble-2026-11-18.md, CLAUDE.md origin/main df50dc58c §8, lint-habits.md, design §4.17.3 + A-635,
A-641 parts 1-2, A-642, A-645 entries; g301's progress; push.rs, vcs_policy.rs, hook_driver.rs, vcs_hooks.rs.

Worktree ~/slates-work/g304-hg-push-land, branch agents/g304-hg-push-land (from origin/main df50dc58c, via
`git worktree add -b` on g066-fs-server's .git).

## 7.2 facts read (Studio, uv's mercurial==7.2 wheel, 2026-11-19)
- phases.advanceboundary: walk down from the given revs while above target; delroots = changed revs whose phase differs
  from both parents'; new roots = unwalked revs (scan from the smallest changed to the end) above target with a parent
  in changed and no unchanged parent in their own phase.
- changegroup.apply, srctype serve: publishing -> registernew(public) + advanceboundary(public, dups + added);
  non-publishing -> registernew(draft, added) (+ advanceboundary(draft) = no-op here). pretxnchangegroup runs with
  node/node_last = first and last ADDED changesets, only when changesets > 0.
- return = len(heads_added) - len(heads_removed) - closing added heads, then +1 / -1 (never 0); dagop.headrevsdiff
  counts the null revision as a removed head of an empty changelog (first push with one head answers 1).
- bookmarks part: applied as is (its race is check:bookmarks'); a divergent name (`@` not at the end) aborts
  "cannot accept divergent bookmark %s!"; node None deletes.
- pushkey bookmarks: existing != old and != new -> 0; new '' deletes; unknown new -> 0. pushkey phases: current == old
  and new < old -> advance, 1; current == new -> 1; else 0. A mandatory pushkey that fails -> error:pushkey.
- phase-heads: per phase in allphases, heads not already in that phase advanced.
- external hook failure: error:abort "<name> hook exited with status <n>" / "killed by signal <n>"; observing hooks warn.
- branchmap: a branch head is dropped when it is an ancestor of a new changeset of its branch through ANY path (checked
  with stock 7.2: default 0 -> stable 1 -> default 2 gives default heads {2}). SIBLING (not mine, report to main):
  slates-hg serve::discovery::branch_head_revs is child-based and would answer {0, 2}.
- Fixture texts reproduced locally (python3 -I hashlib): base ad120869, second 59d5dc23 (manifest a539ce0c), other
  f2563910, plain 048741fa (manifest 8b52510d): every node equal to 7.2's.

## Decisions (to record in §4.17.3 and the amendment)
- Namespace (prefix-free): a line `branch:<label>:<n>`, a bookmark `bookmark:<label>:`; label = the name's bytes, valid
  UTF-8 kept, `:` `%` C0 controls DEL and bytes not UTF-8 as `%XX`. Kind tags differ at their second byte and end in `:`;
  the label never holds `:`, so `branch:<B>:` lists exactly B's lines and `bookmark:<M>:` names exactly one bookmark
  (a Names check with that prefix is an exact presence/absence check). A new line takes the lowest free number of its
  branch. A Mercurial repository's default branch is `branch:default:0` (its root, an empty green).
- Lines matched to new heads by a maximum bipartite matching (Kuhn, heads in arrival order, lines in number order):
  moves never touch the names log; only unmatched new heads (Create) and merged-away lines (Delete) do.
- Race checks as positions: check:heads -> Names `branch:` + Head of every line + markers log; check:updated-heads ->
  Head of each named head's line + Names of its branch; check:bookmarks -> Head + Names `bookmark:<M>:` (or absence);
  check:phases -> phases log position (stricter than 7.2: any phase move since the read races; only a push whose read
  and append are separated, a hook's run, can meet it). Every branch the push touches: Names of its lines; every phase
  change: phases log position.
- Refusals: append's Conflict (a moved line: the head attribute), Moved, AlreadyExists/NotFound of a name, and the db's
  Moved/CheckFailed/StateChanged -> error:pushraced RACED; anything else error:abort in slates' words.
- The push's revisions committed into a fork of the index before the append (all or none), swapped in after it.

## Plan
Part 3 (slates-hg + slates-server): held.rs (Held over HgIndex+ContentStore), classify.rs (pure: lines, bookmarks,
phases, pushkeys, heads count), commit.rs (index fork), verify.rs (passed-over changeset nodes); server hg_push.rs
(names, read, prepare -> BranchChange + EntryParts, commit through RepositoryLogs::append, reply). Golden: base+second
landed from reconstructed texts, then stock push-plain/push-race/push-phase/push-bookmark replies byte for byte.
Part 4: vcs_policy::admit before the verdict; hook_driver at pretxnchangegroup (before append) and changegroup (after).

## Log
- 2026-11-19: policy read, readback sent; code and 7.2 read; plan above.
- 2026-11-19: main's load rule for the Studio (CARGO_BUILD_JOBS=4, one cargo at a time, target dirs deleted as soon as
  read, a "did not answer" run retried once after a minute and counted neither red nor green); then main: the Studio
  is OFFLINE (out of memory) -- no builds anywhere else, keep writing, probe it with `true` every ~5 minutes.
- Committed on agents/g304-hg-push-land (pushed to origin, 3664936ba): d1d7d21da (slates-hg: classify, held, commit,
  Verified::held_changesets, branch_of crate-visible; tests classify_tests.rs, commit_tests.rs) and 3664936ba
  (slates-server repository/hg_push.rs + hg_push/tests.rs; Cargo deps slates-hg + slates-vcs, Cargo.lock by hand;
  docs: §4.17.3 status + piece 5, A-657 part 3 amendment, GAPS, research §24). NOT YET COMPILED (Studio offline).
- Next when the Studio answers: clone the branch, `CARGO_BUILD_JOBS=4 cargo test --locked -p slates-hg --lib`, fix,
  then `-p slates-server --lib hg_push`, fix; rustfmt diff applied from the Studio; red by mutation (classify's
  matching, the publish walk, the entry's position checks, the race mapping); then fill the amendment's evidence, merge,
  push. Part 4 (admit + hooks) after part 3 lands; read acl.py's words on the Studio first.
- Part 4 written on its own branch agents/g304-hg-push-admit (worktree ~/slates-work/g304-hg-push-admit, 7d343ca33,
  pushed, on top of part 3): classify::Plan::firsts; hg_push Ask, admitted (acl's words), Hook, admission_hooks,
  observing_hooks, run_hooks (cfg(unix)), Prepared::with_output; tests (policy deny/not-listed/admitted; hooks run
  in order through the driver with the test binary as hook_driver's roles). Docs for part 4 after it runs.
- Waiting on the Studio (offline); nothing more to do without a build. Reported to main.
- Main (2026-11-19): stop the Studio monitor (each wake re-reads the context and burns the allowance); main watches the
  Studio and messages when it answers -- that message is the signal to run. Monitor bxsyzl3ci stopped; no watcher armed.
  Ready to run: ~/.cache/g304-scratch/studio-hg.sh (branch, crate, filter), first agents/g304-hg-push-land with
  slates-hg then slates-server `hg_push`, then agents/g304-hg-push-admit (merged b4aacea10, at fdf9bc44e).

## Relaunch 2026-11-19 ~16:20Z (fresh g304; Studio offline, builds on jupiter, Windows x86_64)
- Policy re-read: relaunch-2026-11-19.md, restart-preamble, CLAUDE.md origin/main 4644dcec3 §8, lint-habits.md;
  readback of THE RULE sent to main. ~/.cache/g304-scratch was wiped (studio-hg.sh gone): jupiter commands inline.
- State: agents/g304-hg-push-land at aecf38440 (merges main 4644dcec3 = current main; pushed);
  agents/g304-hg-push-admit at fdf9bc44e (part 4 over b4aacea10, not yet merged with aecf38440). A-662 not on main:
  Held over the index stands.
- A-667 recheck: classify's new_heads and place_branch already use 7.2's rule (any-path descent); its oracle
  Whole::branch_heads states the rule too. Adding a direct cross-check: lines read = branch_head_revs(view), lines
  after = branch_head_revs(view after), plus A-667's histories and a fixed-seed non-vacuity count.
- r25 (jupiter): first build of part 3, `cargo test --locked -p slates-hg --lib`, clone ~/.cache/g304-land, target
  ~/.cache/g304-target (kept across fix rounds of this session, deleted after the final read; log ~/.cache/g304-hg.log).
- A-667 cross-check written in classify_tests.rs (land worktree, uncommitted until r25 reads): lines_after_a_push_hold_
  the_served_branch_heads (proptest), three named A-667 histories, fixed_pushes_end_a_line_through_another_branch
  (256 BLAKE3-drawn samples, non-vacuity). rustfmt +1.98.1 runs locally (formatting only): diffs only in part 3 files.
- Part 4 hook env: recorded stock 7.2's own hook environment on this MacBook (Homebrew hg 7.2, formatting-free run,
  scratch under ~/.cache, removed): crates/hg/fixtures/hooks/make-hook-fixtures.sh + {pretxnchangegroup,changegroup}.
  {draft,public}.env.txt + SHA256SUMS in the admit worktree. Both hooks, both modes: HG, HGPLAIN=1, HG_BUNDLE2=1,
  HG_HOOKNAME, HG_HOOKTYPE, HG_NODE, HG_NODE_LAST, HG_SOURCE=serve, HG_TXNID (same both hooks), HG_TXNNAME=serve,
  HG_URL (+ HG_PENDING at pretxnchangegroup). No HG_PHASES_MOVED. Slates lacks HG, HGPLAIN, HG_BUNDLE2, HG_TXNID,
  HG_TXNNAME. Plan: red = test vs fixture with Changegroup.transaction field added only; green = builders set them
  (HG=hg, HGPLAIN=1, HG_BUNDLE2=1, HG_TXNNAME=serve, HG_TXNID from the push's name via BLAKE3).
- Admit branch: merged land aecf38440 (4bdce48a1, clean), then 33e01aba0 (pushed): vcs_hooks Changegroup.transaction,
  changegroup_env + HG HGPLAIN HG_BUNDLE2 HG_TXNID HG_TXNNAME, test mercurials_hooks_get_the_environment_stock_7_2_
  hands_them (vs fixtures), hg_push Prepared.transaction + transaction_id (BLAKE3 derive_key of the push name) +
  Added<'a> for admission_hooks/observing_hooks, tests a_push_names_its_transaction_as_7_2_does + unix hooks test
  checks one HG_TXNID. Red plan: in the jupiter clone, delete the five env lines and run vcs_hooks filter (expect the
  new test failing), restore with git show, rerun (expect pass). Formatting owed: hg_push.rs + tests (part 3/4 code).
- Waiting on r25 (slates-hg part 3 first build).

## Relaunch 2 (2026-11-19, fresh g304; main e3234e618; no readback per main's brief)
- Policy re-read: relaunch-2026-11-19.md, restart-preamble, lint-habits.md, windows-build-recipe.txt; CLAUDE.md as loaded.
- The land worktree held the predecessor's uncommitted A-667 cross-check + rustfmt layout of part 3's hg files; part 3's
  server files formatted with rustup 1.98.1 rustfmt --edition 2024 (format only, no build); committed fc0244d95, merged
  origin/main e3234e618 (slates-merge + GAPS only, clean, no markers) as 04bec948b; pushed agents/g304-hg-push-land.
- r85 (jupiter, Windows x86_64): first slates-hg build, `cargo test --locked -p slates-hg --lib` + `cargo fmt -p slates-hg
  -- --check`; clone ~/.cache/g304-land, target ~/.cache/g304-target (kept across fix rounds), log ~/.cache/g304-hg.log.

## Relaunch 3 (2026-11-19 ~19:20Z, fresh g304; origin/main cd84e3811; Studio back, jupiter retired for agents)
- Policy re-read: relaunch-2026-11-19.md (Studio d1323898, sync false, CARGO_BUILD_JOBS=2, target ~/.cache/g304-target),
  restart-preamble, lint-habits.md, CLAUDE.md origin/main §8. No readback (main's brief).
- Land worktree clean at 04bec948b = origin/agents/g304-hg-push-land. Admit worktree: 010cf2e6f + uncommitted GAPS and
  design edits (part 4's, kept untouched).
- Plan: Studio clone ~/.cache/g304-land (depth 1 of the pushed branch), log ~/.cache/g304-hg.log there;
  `cargo test --locked -p slates-hg --lib -- push::`, fix rounds pushed to the branch and fast-forwarded in the clone;
  then `-p slates-server --lib hg_push`; fmt; land as ONE commit (fresh worktree from origin/main + merge --squash of
  the branch) + merge of origin/main if main moves.
- r3 (macOS arm64, Mac Studio, rustc 1.98.1, 04bec948b): `cargo test --locked -p slates-hg --lib -- push::` compiled clean
  (0 errors, 0 warnings) and passed 36 of 36 (261 filtered out), the classifier's 10 and the commit fork's 2 among them.
- Integration targets: none exercise hg push landing (hg_push has no caller outside its module; no crates/*/tests
  target names Mercurial), so the filtered lib runs are the focused tests (main's correction 2026-11-19 noted).
- r5: fmt --check of slates-hg + slates-server, then `cargo test --locked -p slates-server --lib hg_push -- --test-threads=4`.
- r5 (Studio, 04bec948b): fmt clean; slates-server lib test refused hg_push/tests.rs's use of slates-hg's crate-private
  node::write_hex/from_hex. Fixed d710ff022 (crate::verbs::hex_of + slates_hg::pushkey::hex_node; slates-hg API unchanged).
- Merged origin/main 5c0a522f1 (eb9966662): one conflict, the design's amendment log end -> main's A-656..A-672, then
  A-657 part 3; GAPS clean; no markers.
- r21 (Studio, eb9966662): hg push:: 36/36 green; fmt diff (chain split) in hg_push/tests.rs; server lib test does NOT
  compile on main itself: vcs_http/tests/git.rs:490,500 `PackRefused::Stalled`, removed by main's 188314131. Sibling,
  reported to main (not fixed in my piece).
- bb1e39f44: rustfmt chain split + part 3's six >120-char doc/comment lines rewrapped.
- r29 (Studio, bb1e39f44): fmt; hg push::; server hg_push with the two Stalled lines stubbed todo!() in the Studio
  clone ONLY (restored after), to run part 3 on the merged tree.
- r29 (Studio, bb1e39f44, stub in clone only): fmt clean; hg 36/36; server hg_push 7/7 but the test build warned:
  Prepared's `added`/`asked` and `Prepared::asks` never read (part 4's inputs). Removed (with `asked`, `policy_name`,
  the vcs_policy import) on the branch (dead-code commit before 5aa274153); GAPS/amendment no longer name `Prepared::asks`.
- Main fixed its slates-server break (71a43cd97) and asked for evidence on the merged tree. Merged origin/main
  b1926d201 (5aa274153; one conflict, amendment log end: main's A-653 first, A-657 part 3 last).
- r47 (macOS arm64, Mac Studio, 5aa274153 = the code that lands, no stub): fmt clean (slates-hg + slates-server);
  `cargo test --locked -p slates-hg --lib -- push::` 36 passed 0 failed (classifier 11, commit fork 2, parts 1-2 23);
  `cargo test --locked -p slates-server --lib hg_push -- --test-threads=4` 7 passed 0 failed, build warning-free.
- Evidence written into §4.17.3 status, the A-657 part 3 amendment and GAPS (56b9bb8db, docs only).

## LANDED 2026-11-19: a6306963e on main (b1926d201..a6306963e), ONE commit (squash of agents/g304-hg-push-land at
   56b9bb8db, staged tree identical to it; main had not moved, so no merge commit was needed). 17 files, part 3's alone.
- Cleaned: Studio ~/.cache/g304-land, ~/.cache/g304-target (5.3 GB) and logs deleted; landing worktree removed.
- Run requests filed (run-requests.md, "g304 main a6306963e"): other platforms + clippy/xtask/docs/lint matrix
  (part 3 never met clippy or xtask), and mutants M1-M7 with exact substitutions.
- Sibling reported to main (fixed by main in 71a43cd97): vcs_http/tests/git.rs used PackRefused::Stalled after 188314131.

## Part 4 (next, when main starts it): carrying the admit branch forward
- agents/g304-hg-push-admit (worktree ~/slates-work/g304-hg-push-admit, 010cf2e6f + UNCOMMITTED GAPS/design edits,
  part 4's) shares only e3234e618 with main now: part 3 landed as a squash, so do NOT merge main into it (add/add
  conflicts on every part 3 file). Instead: `git worktree add -b <new> ~/slates-work/g304-<x> origin/main`, then
  cherry-pick part 4's own commits oldest first: 7d343ca33, 521a9fde7, 33e01aba0, 1c20c3ee9, 975becc20, 010cf2e6f
  (NOT part 3's d1d7d21da, 3664936ba, b4aacea10, fc0244d95), then carry the admit worktree's uncommitted doc edits.
- Expected conflicts: hg_push.rs (landed part 3 has no Prepared.added/asked/asks, asked(), policy_name(), vcs_policy
  import: part 4's versions return, and must be read by part 4's tests or the test build warns again);
  hg_push/tests.rs (keep the landed hex helpers: crate::verbs::hex_of and slates_hg::pushkey::hex_node; slates-hg's
  node::write_hex/from_hex are crate-private); docs (the evidence lines now filled; part 4's "pending" lines).
- Main's brief for part 4: the hook environment (33e01aba0) with its failing test first.

## Part 4 (main 2026-11-19 ~20:30Z: "Go with part 4, alone: hg 7.2's hook environment ... red first, then green")
- Admit worktree's uncommitted docs committed + pushed: 5c557efe8 on agents/g304-hg-push-admit.
- Net of part 4 = diff(04bec948b, 5c557efe8): squashed onto 04bec948b in scratch worktree ~/slates-work/g304-p4-net
  (branch agents/g304-p4-net, 3a65d9ff3, local only), then cherry-picked onto origin/main 2e4433040 in
  ~/slates-work/g304-hg-push-hooks (branch agents/g304-hg-push-hooks): conflicts resolved -- hg_push.rs (four hunks,
  ours empty: part 4's side taken; Prepared doc restored), GAPS (part 4's side), design status (part 3's evidence +
  part 4's owed list + part 4's status) and amendment log (main's A-626 part 3 first, A-657 part 4 last).
  a0e3e4512, pushed. 13 files over main.
- Integration targets reaching the hook driver's hg path: none (the hg hooks run in hg_push's lib tests through
  hook_driver::tests::a_hook_role).
- r67 (Studio, a0e3e4512, fresh target): fmt; `-p slates-hg --lib -- push::`; `-p slates-server --lib -- hg_push vcs_hooks`.
- Red plan (after green compiles): remove the five changegroup_env lines (HG, HGPLAIN, HG_BUNDLE2, HG_TXNID, HG_TXNNAME)
  in the Studio clone, run `vcs_hooks::tests::mercurials_hooks` (expect FAILED), restore with git show, run green.
- r67 (Studio, a0e3e4512): fmt clean; hg push:: 36/36; server: 3 x E0603, part 4's non-test hg_push called slates-hg's
  crate-private node::write_hex (transaction_id, acl words, hex_text). Fixed 1dc8c3ca0 (crate::verbs::hex_of; hex_text gone).
- r80 (Studio, 1dc8c3ca0): RED confirmed -- five env lines removed in the clone: mercurials_hooks_get_the_environment_
  stock_7_2_hands_them FAILED (left 7 names, right 12: HG HGPLAIN HG_BUNDLE2 HG_TXNID HG_TXNNAME missing). Green: fmt
  clean, hg 36/36, server 45 ok incl. the env test, but hooks::admission_hooks_run_in_order... HUNG 29 min (stopped).
- Diag (Studio, the test alone, sampled): hangs in drive's recv(); shard run_until_idle (crates/rt/src/shard.rs) returns
  at its first zero-timeout poll when no timer is armed, so the hooks' I/O was never awaited. Fixed 631606f9e: drive
  runs under futures::within(TEST_BOUND_NS = 10 x daemon::LIVENESS_BUDGET_NS) as hook_driver/svn_hooks tests do.
  (Gotcha worth CLAUDE.md: a LocalRuntime driven by run_until_idle needs a timer armed for I/O-only waits.)
- r166 (Studio, 631606f9e): red then green again on the final tree.
- r166 (Studio, 631606f9e): red again (env test FAILED, 7 vs 12 names); green: fmt clean, hg 36/36, server hg_push +
  vcs_hooks 46/46 (the hooks test passes now) BUT the test build warned: Prepared's `added`, `changegroup` and
  `with_output` never read. Fixed 97cc365b3: platform-free test a_hooks_words_follow_the_changegroup_reply_naming_its_part
  (prepare push-plain, check the hooks' changesets, with_output a hook's words, land, read the reply as a client:
  7.2's push-plain reply with an `output` part naming the changegroup part right after reply:changegroup).
- r175 (Studio, 97cc365b3): red (FAILED, 7 vs 12); green: fmt clean, hg 36/36, server 47/47, warning-free.
- Evidence docs written (amendment, §4.17.3 status, GAPS). Merged origin/main 7d020df47 (3e2fb9569): code clean; docs:
  status (part 3 owed as landed, part 4 status, then main's A-650 part 1), amendment end (main's first, part 4 last),
  GAPS (main's entry + part 4's owed change). hg_push still uncalled outside its module (A-650's site judges unbundle's
  method and policy only), so no integration target reaches it.
- r184 (Studio, 3e2fb9569, the merged tree that lands): red then green.
- r184 (3e2fb9569, main 7d020df47 merged): red FAILED 7 vs 12; green fmt, hg 36/36, server 47/47 warning-free.
- Main moved twice more: merged 031e04337 (924888d14; r192 red + green alike) and 1c6ac5006 (A-673, slates-hg's
  compression bound; d49e5484f; r202 green: fmt clean, hg 36/36, server 47/47, both crates recompiled, no warnings).

## PART 4 LANDED 2026-11-19: 5a13886e4 on main (1c6ac5006..5a13886e4), ONE commit (squash of agents/g304-hg-push-hooks
   at d49e5484f + the evidence lines; code identical to the tested d49e5484f). 13 files, part 4's alone.
- Evidence (macOS arm64, Mac Studio): red r192 (changegroup_env without HG HGPLAIN HG_BUNDLE2 HG_TXNID HG_TXNNAME:
  mercurials_hooks_get_the_environment_stock_7_2_hands_them FAILED, 7 names vs 7.2's 12); green r202 on the landing
  tree: `cargo test --locked -p slates-server --lib -- hg_push vcs_hooks --test-threads=4` 47/47 warning-free,
  `cargo test --locked -p slates-hg --lib -- push::` 36/36, fmt clean. No integration target reaches the hg hook path.
- Cleaned: Studio ~/.cache/g304-hooks, g304-target (9.1 GB), logs, sample; worktrees g304-hooks-one, g304-p4-net,
  g304-hg-push-hooks, g304-hg-push-admit, g304-hg-push-land removed (all clean; branches pushed; agents/g304-p4-net is
  a local scratch ref only). Old scratch files ~/slates-work/g304-{fmt-*.diff,msg.txt,scratch/} left (applied).
- Findings for main: (1) LocalRuntime::run_until_idle returns at its first zero-timeout poll when no timer is armed, so
  a test driving I/O-only waits must arm one (futures::within), as hook_driver's and svn_hooks' tests do -- worth a
  CLAUDE.md gotcha; (2) part 4's hook path is cfg(unix) (Hook, HooksRan, run_hooks, admission/observing_hooks,
  hook_words): Windows runs no Mercurial hook yet (owed), and Windows clippy should confirm no dead code there.

## A-679 (main 2026-11-19 ~21:00Z): Mercurial on the daemon's SSH edge
- Brief: the SSH site runs `hg -R <repo> serve --stdio` through slates-hg's serve::stdio (A-650 part 2) over the
  repository's A-662 hg store, refusals in 7.2's words. Failing scripted hg SSH session first; focused unit tests
  (vcs_ssh site, slates-hg serve) + any integration test reaching the SSH site + fmt on the Studio; land one commit +
  merge. g294 is in vcs_ssh (A-651 part 3, key holders): merge origin/main before starting and before landing.
- A-679 taken in amendments.tsv. Worktree ~/slates-work/g304-a679-hg-ssh, branch agents/g304-a679-hg-ssh.
- STOPPED (context short) 2026-11-19 after the restart, before any A-679 code. State: worktree
  ~/slates-work/g304-a679-hg-ssh clean at origin/main 901be4f52 (branch agents/g304-a679-hg-ssh, local only, nothing
  committed; no kiln run started). A-679 row in amendments.tsv (line 222).
- RESUME for a fresh g304: fetch, merge origin/main into the worktree (g294 lands A-651 part 3 in vcs_ssh: keep both
  sides). Read: crates/server/src/vcs_ssh.rs (module docs), vcs_ssh/site.rs, vcs_ssh/git.rs (A-651 part 2: git's
  upload-pack behind the SSH edge, the pattern for a command over a session), vcs_ssh/tests/git.rs (the scripted-session
  test to model the hg one on), tests/site.rs; slates-hg serve::stdio (A-650 part 2, the SSH entry point) and
  crates/hg/src/ssh.rs; crates/server/src/vcs_store.rs (A-662's Mercurial store) + vcs_store/hg_import.rs (A-675);
  GAPS' Mercurial owed "(4, rest) read serving: the SSH edge's loop over serve::stdio ... A-662's stores handed to the
  HTTP site and that loop"; §4.17.3 piece 4. Failing test first: a scripted `hg -R <repo> serve --stdio` session
  (hello, capabilities, heads, a refusal for a repository not held, in 7.2's words) against the site; then the site's
  Mercurial arm through serve::stdio over the repository's HgStore. Runs on the Studio as before (sync false, clone of
  the pushed branch, CARGO_BUILD_JOBS=2, own target): `cargo test --locked -p slates-server --lib vcs_ssh`, slates-hg
  `--lib serve::` if touched, any integration target reaching the SSH site (search crates/*/tests for vcs_ssh), fmt.
- Gotchas carried: run_until_idle needs a timer armed for I/O-only waits (futures::within(TEST_BOUND_NS)); slates-hg's
  node::write_hex/from_hex are crate-private (crate::verbs::hex_of, slates_hg::pushkey::hex_node); a test build with a
  dead-code warning fails main's clippy, so every new field/method is read by a test.

## A-679 relaunch (2026-11-19 ~21:30Z, fresh g304; worktree ff to origin/main bb535bd4d, no code yet at start)
- Policy re-read: relaunch-2026-11-19.md, restart-preamble, lint-habits.md, CLAUDE.md origin/main §8.
- Design: vcs_ssh/hg.rs HgCommand<S: vcs_http::hg::HgStores> over slates_hg::serve::stdio::Stdio (the HTTP site's
  store seam, so g303's A-662 lender fills both sites, as git's two edges share StoreLender); site.rs ToolServices<L, H>
  + ToolCommand enum; Mercurial step 0 = 7.2's unsafe-invocation refusal (repository arg starting `--`); `--hidden`
  answered as 7.2 answers an unauthorized user (warning naming the principal's record, as acl's words do, then the
  served view); a store gone mid-session ends in the site's not-served words. Tests: vcs_ssh/tests/hg.rs as a child of
  vcs_ssh::tests (served_by written identically to g294's part 3 so either order merges clean): the probe golden
  (handshake answered by the session itself + 7.2's probe.server bytes) at several cuts, hidden, vanishing store,
  window bookkeeping, services start, and the scripted session over the real edge through ToolSite + not-found.
- Stock 7.2 recorded on this MacBook (Homebrew hg 7.2, HGPLAIN=1 HGRCPATH=, scratch ~/.cache/g304-scratch, removed):
  `hg -R --x serve --stdio` -> stderr "abort: potentially unsafe serve --stdio invocation: ['-R', '--x', 'serve',
  '--stdio']\n", 255, stdout empty; odd bytes render '--y\'\xff' with '--hidden' appended; `serve --stdio --hidden` on a
  repo -> "ignoring request to access hidden changeset by unauthorized user: adalundhe\n" then answers, 0.

## A-679 relaunch 2 (2026-11-19 ~22:00Z, fresh g304)
- Policy re-read: relaunch-2026-11-19.md, restart-preamble, lint-habits.md; CLAUDE.md as loaded. No readback (brief).
- Worktree held the predecessor's uncommitted A-679 start (hg.rs HgCommand; site.rs docs/imports; vcs_http `pub(crate)
  mod hg`; vcs_ssh channel_window): committed as is, 0f957cd3f, pushed agents/g304-a679-hg-ssh. No test yet; the site
  does not start it yet. origin/main moved to 3612d1ef9 (A-644 part 2 landed: vcs_ssh tests' RepositoryPolicy literals).
- Merged origin/main b5231f0ff into the branch (4e7997c8b, clean; main marked GAPS' SSH loop "A-679, running").
- Recorded stock 7.2 (Homebrew 7.2, python 3.14, HGPLAIN=1 HGRCPATH=, empty cwd ~/slates-work/g304-scratch/hgcwd,
  script ~/slates-work/g304-scratch/record_unsafe.py): `hg -R <p> serve --stdio [--hidden]`, all 255 + stdout empty:
  --x -> ['-R', '--x', 'serve', '--stdio']; +--hidden -> ..., '--hidden']; --y'\xff -> '--y\'\xff';
  --a\tb\nc\rd\\e\x7fh\x01"g -> '--a\tb\nc\rd\\e\x7fh\x01"g' (codecs.escape_encode; " bare); --é -> '--\xc3\xa9';
  -- -> refused alike; -x (one dash) -> "abort: repository -x not found" (hidden or not). Hidden on a scratch repo:
  stderr "ignoring request to access hidden changeset by unauthorized user: adalundhe\n", then hello+between answered, 0.
- Test plan: crates/server/src/vcs_ssh/tests/hg.rs as a CHILD of vcs_ssh::tests (declared in tests/serve.rs below its
  use block, away from g294's hunks), served_by byte-identical to g294's part 3; HgCommand unit tests (probe golden
  over a store from crates/hg/fixtures/serve/state-probe.txt + bookmarks-probe.txt, hidden, vanishing store, window),
  ToolServices start, site's unsafe refusal (the recorded cases), and the scripted session over the real edge.
- Code + tests written: site.rs (ToolServices<L, H>, ToolCommand, not_served, python_escaped, unsafe_invocation in
  dispatch), hg.rs (predecessor's HgCommand, docs rewrapped to 120), tests/hg.rs (child of vcs_ssh::tests, 7 tests:
  probe golden at cuts 1/7/1024/whole, hidden warning, store gone, window bookkeeping, services start, unsafe refusal
  table from the recording, the edge session plain/--hidden/not-found), tests/serve.rs (served_by = g294's text, `mod
  hg` below the use block), tests/git.rs (NoStores). rustfmt 1.98.1 clean locally (format only).
- Merged origin/main 2410011da (152c9ab02, clean). Branch: red 30dc4ca18 (stubs: no Mercurial start, no unsafe check,
  step answers nothing) then green 9e7e1b054; pushed.
- r128 (Studio d1323898, sync false, clone ~/.cache/g304-a679 depth 2, target ~/.cache/g304-target, logs
  ~/.cache/g304-a679.{red,fmt,green}.log): red = HEAD~1's site.rs+hg.rs, `--lib -- vcs_ssh`; green = fmt + `--lib --
  vcs_ssh vcs_http::hg --test-threads=4`.
- r128 RESULT (macOS arm64, Mac Studio, over main 2410011da): RED (30dc4ca18 stubs) `cargo test --locked -p
  slates-server --lib -- vcs_ssh --test-threads=4` 25 passed, the 7 new failed, each at its assert_eq (lines 522, 538,
  573, 599, 654, 695, 762); GREEN (9e7e1b054) fmt clean, "Compiling slates-server" seen, `-- vcs_ssh vcs_http::hg
  --test-threads=4` 39 of 39, 0 rustc warnings.
- Docs written (3072ec14c): §4.17.2 status, §4.17.3 status + piece 4, GAPS §4.17.3 Built line + owed list (A-680),
  research §26 (7.2 code read in Homebrew 7.2: dispatch.py 329-353, stringutil escapestr/pprintgen, commands.py serve
  6215-6230), amendment A-679 at the log's end.
- Main moved to 225d7eead: 4a74c1187 (harness fix: Transcript.ended, client waits for the edge to end the connection).
  Merged (1586bec9f): one conflict, the amendment log's end (main's A-636 part 2a, A-658 part 2, A-625 part 3 cont. (2)
  first, A-679 last); tests/hg.rs's three expected Transcripts gain `ended: true`. Pushed. diff --stat vs main: my 10
  files. Needs a green run over 1586bec9f before landing (r128 predates the Transcript change).
- Studio run over 1586bec9f refused at launch: "adas-mac-studio.local-2 is not connected" (~22:00Z). Retried once a
  minute later: r179 (same clone/target/filters; logs ~/.cache/g304-a679.{fmt2,green2}.log).

## A-679 relaunch 3 (2026-11-19 ~22:40Z, fresh g304 after an interrupt)
- Policy re-read: relaunch-2026-11-19.md, restart-preamble, lint-habits.md, CLAUDE.md origin/main §8. No readback.
- Predecessor's last state: branch at 0c972ba6e (merge of main 3b0b95379, pushed); its Studio run over that tree had
  ENDED green (~22:09Z): fmt3 empty (clean), green3 `--lib -- vcs_ssh vcs_http::hg --test-threads=4` 39 of 39,
  "Compiling slates-server" seen, the only "warning" lines aws-lc-sys build-script notices.
- Main moved to b94ae6e90 (A-678 import verb; A-653 continued (4): PackSource::begin/write take the request's
  admission, vcs_ssh/git.rs keeps the admission). Merged: 8790ea4a8 (one conflict, the amendment log's end, main's A-678
  and A-653 cont. (4) first, A-679 last, union checked; tests/git.rs auto-merged: NoPacks' admission + NoStores). Pushed.
- No integration target reaches the SSH edge (vcs_ssh private; no crates/*/tests file names it): filtered lib run only.
- Next: Studio run over 8790ea4a8 (fresh depth-1 clone ~/.cache/g304-a679, target ~/.cache/g304-target, logs
  ~/.cache/g304-a679.{fmt4,green4}.log); then evidence line in the A-679 amendment, merge main if moved, land as ONE
  commit (squash onto origin/main in a fresh worktree) + push HEAD:main; clean Studio; report.
- r240 (Studio, 8790ea4a8): fmt clean; test build REFUSED E0050 at vcs_ssh/tests/hg.rs:319 -- my test NoPacks' write
  lacked A-653 cont. (4)'s new `admission` parameter (a semantic merge conflict). Fixed e851b76ad (pushed).
- r269 (Studio, e851b76ad, clone fast-forwarded with --depth 2, same target; logs ~/.cache/g304-a679.{fmt5,green5}.log).
- Ada 22:45Z (relaunch file): completeness first; a finding by reading -> main in one line, never chased.
- r269 (e851b76ad): fmt clean, 39/39, no warning. Main then landed A-651 part 3 (g294, vcs_ssh key proofs): merged
  3452f7b68 (log-end conflict only; code auto-merged). r280 (3452f7b68): fmt clean, 42/42 (A-651 part 3's key tests +
  my 7), slates-ssh + slates-server compiled afresh, no warning. Evidence docs ac3c7ba1d.
- Main then landed A-655 part 2b + stock hg interop CLI tests (126eb8a83): merged 40ddcaa85 (conflicts: log end; GAPS
  interop line vs my Built line, both kept, interop's A-679 clause now names A-680's lender; cli.rs SSH round trip's
  ignore reason + doc: A-680 in A-679's place). Pushed.
- r292 (Studio, 40ddcaa85): fmt server+cli; `-p slates-server --lib -- vcs_ssh vcs_http::hg --test-threads=4`;
  `-p slates-cli --test cli stock_hg_clones_pulls_and_pushes_over_the_ssh_edge` (expect 1 ignored, new reason).
  Logs ~/.cache/g304-a679.{fmt7,green7,cli7}.log.
- r292 (40ddcaa85): fmt server+cli clean; lib 42/42 (731 filtered), no warning; cli SSH round trip 1 ignored with the
  new reason. The cli build's non-test slates-server lib warned 21x, ALL main's (lfs.rs 20 dead items; vcs_http/git.rs:57
  unused re-export UploadServices, used only by tests) -> finding for main, one line. Evidence docs 8b8565e04.
- Main landed A-680 part 1 (getbundle::Store reads take the admission, Result<Option<Cow>>, file_node gone) + A-665
  part 2: merged 24b2eee2c (my Probed store adapted like main's Empty; research §26 collision -> mine renumbered §27,
  my refs moved; status/piece 4/log end/GAPS resolved). Then main 693bc8feb (A-681 part 1 svn DAV, A-640 p4 s3, svn
  interop): merged 2a6407244 (vcs_http.rs mod list: pub(crate) mod hg + mod svn; log end). NOTE: worktree refs are
  shared with other agents' worktrees: origin/main moves under a merge; audit against MERGE_HEAD, not origin/main.
- r332 (Studio, 2a6407244): fmt8/green8/cli8 logs. Then: evidence lines, land by squash in a fresh worktree.
- r332 (2a6407244, main 693bc8feb): fmt server+cli clean; lib 42/42 (740 filtered); cli SSH round trip 1 ignored, new
  reason; cli build's 21 warnings all main's (lfs.rs 20, vcs_http/git.rs:57 1). Evidence docs 5f245b0bb.
- This MacBook's disk stalled a 2-line sed for ~3 min (process state U, 27 GiB free): use long timeouts for git/edits.
- Main landed 2f73eb075 (A-651 part 4: SSH edge moves a connection to the repository's owner shard; SshSite::locate):
  merged 531d41f1d (vcs_ssh.rs owed list, site.rs imports, log end). ToolSite::locate parses hg execs too (same asked()).
- r359 (Studio, 531d41f1d): fmt9/green9. Then evidence line (r359), then land by squash onto 2f73eb075.
- r359 (531d41f1d): test build E0277 -- A-651 part 4's served_by maker is now Fn + Clone + Send; my Lends' gone flag
  was &'static Cell<bool>. Fixed 353c598ae (AtomicBool, Release/Acquire). r363 (353c598ae): fmt clean, 44/44, no warning.
- Main landed A-685 (repository create --tool) + A-664 p3 piece 1: merged ffab4d9d7 (log end only; none touch my items).
  FINDING for main: cli.rs hg interop ignore reasons still name A-685 after it landed (c3f8ac6fb).
- r378 (Studio, ffab4d9d7): fmt11/green11. Then evidence (r378), then land: worktree at 0462bd922 + merge --squash.
- r378 (ffab4d9d7, main 0462bd922): fmt clean, 44/44 (746 filtered), no warning. Evidence f338e4393.
- Main landed A-680 part 2a (9a919b371, main b288b412e; main's message 23:1xZ confirmed: adapt to HgStores::lend(id,
  &mut room, |view, room| ..) over ServedViews, drop NoStores): merged 496cbff1d -- HgCommand::step lends with
  Admission::new(room) and hands Stdio::step left.left(); tests' Lends on the new trait; tests/git.rs uses ServedViews
  (lends nothing off a shard); cli.rs SSH reason now "A-684 + pushes over the stdio session"; GAPS adds the SSH edge's
  pushes to (5, rest); my vcs_http.rs change is now identical to main's (no diff).
- r400 (Studio, 496cbff1d): fmt12/green12/cli12. Then evidence (r400), land by squash onto b288b412e.
- r400 (496cbff1d, main b288b412e): fmt clean; 44/44 (748 filtered); cli SSH round trip ignored with the new reason;
  plain-lib warnings all main's (lfs.rs 20; vcs_http/git.rs:57 UploadServices; vcs_http.rs:74 ShardRepositories).
- Main f7684fc20 (A-690 divergent bookmarks, A-689 --vcs-svn): merged 6fe628d60; r422: fmt clean, 44/44 (750), no warning.
- Main 5206419be (A-682: git ShardStores; StoreLender::lend takes (id, &Principal) + lend_objects; UploadCommand::new
  takes the reader): merged e1dcee5b7 -- NoGit adapted; A-682's vcs_store/upload/tests.rs ssh_session now
  ToolServices::new(ShardStores, ServedViews, ROOM); site.rs step 3 names both daemon lenders.
- r438 (Studio, e1dcee5b7): fmt14/green14 with filters vcs_ssh vcs_http::hg vcs_store::upload.
- r438 (e1dcee5b7, main 5206419be): fmt clean; `-- vcs_ssh vcs_http::hg vcs_store::upload` 49/49 (752 filtered), the
  A-682 SSH session over both daemon lenders among them, no warning. Evidence ce3e3e816. Main 2c3fa2bcc (A-636 part 2b,
  db/fleet/state logs; RepositoryRecord unchanged; reaches nothing of A-679): merged 54d285529, r438 stands.
- This MacBook's disk fell to 120 MiB at ~23:40Z (27 GiB at ~23:00Z); a fetch failed "Out of diskspace" (my
  tmp_pack_mNbUie removed); it recovered to ~5 GiB by 23:45Z. Not mine; told main.

## A-679 LANDED 2026-11-19 ~23:48Z: 075e36b52 on main (2c3fa2bcc..075e36b52), ONE commit (squash of
   agents/g304-a679-hg-ssh at 54d285529 in a fresh worktree at 2c3fa2bcc; staged tree identical to the branch tip).
   11 files: vcs_ssh/{hg.rs new, site.rs, vcs_ssh.rs, tests/hg.rs new, tests/git.rs, tests/serve.rs},
   vcs_store/upload/tests.rs (A-682's SSH session over both daemon lenders), cli/tests/cli.rs (SSH round trip's reason),
   GAPS, design (§4.17.2/§4.17.3 status, piece 4, amendment A-679), research §27.
- Cleaned: Studio clone ~/.cache/g304-a679 (288 MB), target ~/.cache/g304-target (18 GB), logs; worktrees
  g304-a679-hg-ssh and g304-a679-land removed (clean; branch agents/g304-a679-hg-ssh pushed at 54d285529).
- Run requests filed (run-requests.md "g304 main 2c3fa2bcc..075e36b52"): other platforms, clippy/xtask/docs, red-checks.
- Findings for main (one line each, not chased): the plain (non-test) slates-server lib warns 22x on main -- lfs.rs (20
  dead items), vcs_http/git.rs:57 `UploadServices` and vcs_http.rs:74 `ShardRepositories` (re-exports only tests use).
- Left for A-679's line: the daemon starting the SSH edge with ServedViews (A-684); pushes over the stdio session
  (`unbundle` in slates-hg's session, then hg_push), now in GAPS (5, rest); main's live hg clone/pull over SSH.
- Main's disk-care ask (after landing): checked 075e36b52 read-only -- every touched file grew (SLATES_DESIGN.md
  5,315,289 -> 5,326,747 B; GAPS 2,402,152 -> 2,403,548 B; largest deletion 24 lines in site.rs, intended); main
  9ca6e3883's design doc 5,334,674 B. No worktree or uncommitted work of mine remains. END.

## A-696 (main 2026-11-19 ~23:50Z): Mercurial pushes over the SSH stdio session (fresh g304)
- Policy read: relaunch-2026-11-19.md (completeness first), restart-preamble, lint-habits.md, CLAUDE.md origin/main §8.
  Not on main (git log origin/main | grep stdio/A-679: only A-679 075e36b52). A-696 claimed in amendments.tsv.
- Worktree ~/slates-work/g304-a696-hg-ssh-push, branch agents/g304-a696-hg-ssh-push (from origin/main fc5f4ea98).
- g303's A-680 part 2b is unlanded wip (origin/agents/g303-a680-p2b dd88f2d30: slates-hg push::read_bundle + Outcome,
  serve::http::pushed, hg_push::unbundle driver + land's store arm, HgStores::push, ServedViews::push, daemon test).
  It is exactly the driver the SSH side calls, so: merged it into my branch as a SCRATCH base (f4b36e44f, never lands;
  daemon_edge.rs doc + tests imports resolved as unions). My own commits go on top; landing = fresh worktree from
  origin/main after g303 lands + my diff only.
- Golden: crates/hg/fixtures/wire/ssh-push.{client,server,stderr} (stock 7.2 SSH push recorded by make-wire-fixtures.sh:
  server base+second draft (root base), bookmark main at base, publishing; client pushes "third"; reply
  reply:changegroup return 1; stderr adding changesets/manifests/file changes + added 1 changesets with 1 changes to 1
  files; final listkeys phases -> publishing only).
- Code + tests written (95c225f64, 00fe4ab97 on agents/g304-a696-hg-ssh-push; merged main e72f211f6 = A-684, A-688,
  A-691, A-665 p3 in): slates-hg serve::stdio (unbundle read + heads judged "preparing changes" -> pusherr before the
  go-ahead; go-ahead `0\n`; chunks taken into Push{heads,bundle,admission} as Progress::Took; take_push/pushed framing:
  reply raw, Unprocessed as pusherr string, Bundle1 abort+bundle2required on stderr + `0\n1\n0`; step_push/taking_push
  so chunk steps make no view); session.rs: SSH SERVED + unbundle, one BUNDLE2_SERVED (push caps) for both transports
  (g303's BUNDLE2_SERVED_HTTP folded in); server vcs_ssh/hg.rs: HgCommand carries the principal, lands a whole push via
  HgStores::push (g303's), site passes (id, principal), hidden: bool. Tests: stdio_tests (5 new), getbundle_tests
  check_hello + unbundle, vcs_ssh/tests/hg.rs (Lends::push echo; a_push_reaches_the_lender...; golden
  stock_7_2s_push_over_ssh_lands_through_the_daemons_lenders... replaying wire/ssh-push over a daemon shard with base+
  second drafted by a non-publishing prepare/land).
- r543 (Studio, 00fe4ab97): red (push path off by sed) hg serve::stdio + server vcs_ssh::tests::hg; green fmt, hg
  serve::, server vcs_ssh vcs_http::hg hg_push. Logs ~/.cache/g304-a696-logs on the Studio.
- Progress words decision: none printed over SSH (as g303's HTTP omits them); stock stderr asserted to hold only them.
