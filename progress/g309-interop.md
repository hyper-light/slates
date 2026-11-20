# g309 interop (UI goal 93 part "Interop with stock git, hg and svn clients proven")

## Piece 1: stock git clone + fetch over HTTP against a real `slates anchor --vcs-http 127.0.0.1:0`
- Started 2026-11-19 ~20:30Z. Worktree ~/slates-work/g309-interop (branch agents/g309-interop) off the shared clone
  ~/slates-work/g066-fs-server, from origin/main 5a13886e4.
- Build host: Mac Studio d1323898 (sync false, GitHub clone of the pushed branch), CARGO_TARGET_DIR ~/.cache/g309-target,
  CARGO_BUILD_JOBS=2.

## Log
- ~20:45Z investigation for the interop clone test (no code written yet), on origin/main 5a13886e4:
  - The daemon's git site is `GitSite<RecordedCredentials, ShardRepositories, Unserved>`
    (crates/server/src/vcs_http/daemon_edge.rs): every git service answers 503; no StoreLender exists outside
    crates/server/src/vcs_http/tests/git.rs (`Lends`). A stock clone dies at its first info/refs.
  - A-664 part 3 (the server-made commit recorded and admitted to the git store) is not built: slates_git::derived has no
    caller in slates-server outside tests; only a git push (repository/push.rs, `sealed.admit`/`record`) fills the store.
  - Client surfaces owed: no client/CLI/api verb for CreateRepository, SetIdentities (proof
    slates_server::landing::identities_proof under the issuer secret; Issuer::secret is private), or a submit with a
    message (Client::submit sends Authorship::default(), so a repository's green refuses MessageRequired).
    Workable from a test: Client::call(&RequestBody::CreateRepository{..}) -> GreenCreated{id};
    RequestBody::submit(work, vec![]).with_message(..) via call (verdict walk is private: a one-page verdict only).
  - URL: repository `name` is served at /name/info/refs exactly (no .git second spelling).
  - On Linux the issuer surface is a descriptor only the anchor's children hold: CLI issuer flows skip loudly there.
  - Plan kept for the interop piece: new target crates/cli/tests/interop.rs with its own AnchorProcess guard.

## Piece 1 REPLACED by main (~20:50Z): the production StoreLender over A-662's map (state.vcs_stores)
- So the daemon's git HTTP site and the SSH site serve upload-pack from a repository's real store. Failing test first
  (upload-pack through the daemon's edge answers 503 today). Focused: server --lib vcs_http git site, vcs_ssh,
  vcs_store; cli --test cli vcs_http; fmt. One commit + merge, END. The interop clone test is the piece after.

## 2026-11-19 ~22:10Z relaunch (fresh g309): the production StoreLender (A-682, self-taken; A-680 is g303's)
- Found the predecessor's UNCOMMITTED draft in ~/slates-work/g309-interop (base 4dca63df7): vcs_store/upload.rs
  (ShardStores, lend_on, refs by the reader's Read right via new vcs_policy::permits, ShardPacks over Fragments),
  StoreLender split into lend((id, reader)) + lend_objects(id), DaemonSite = UploadServices<ShardStores>, Unserved moved
  to the git site's tests, UploadCommand takes the reader; `mod tests;` declared with NO tests file; docs cite A-680.
  Kept as commit 98d28d3ef, pushed to agents/g309-interop (branch only, never for main).
- Checked: Answer::write/Fragments::write read only served.repository/store (so lend_objects with no refs is sound);
  PackSource::begin already takes &mut Admission on main; g303-hg-view touches crates/hg fixtures only so far.
- Plan: worktree ~/slates-work/g309-lender, branch agents/g309-lender from origin/main: commit R = failing daemon-edge
  test (owner's v2 info/refs + ls-refs through the daemon's edge answer 503 today), then commit G = lender + unit tests
  (vcs_store::upload::tests over audit_on_shard + push_through) + SSH command over the real lender + docs (A-682).
  Studio: one clone, red run, then ff to green in the same clone (target ~/.cache/g309-target). Land by cherry-pick -n
  R+G onto origin/main as ONE commit.
- ~22:40Z state: worktree ~/slates-work/g309-lender, branch agents/g309-lender (pushed): 6236f3335 red test,
  9519004d7 draft+tests, dca39eaa0 trait fix (E0642: the draft never compiled), 22bc741ee rustfmt (r123's patch) +
  docs, f2ad56fa0 merge of main 2410011da (A-654 part 2) + refactor: the lender now names refs by A-654's
  git_push::ref_of and hides by git_push::PolicyHidden (both made pub(crate)); vcs_policy::permits dropped (no diff
  to vcs_policy.rs). Docs carry @@EVIDENCE@@ placeholders to fill at landing.
- Runs (macOS arm64, Mac Studio d1323898, ~/g309-work/clone, target ~/.cache/g309-target):
  r95 RED at 6236f3335: --lib vcs_http::daemon_edge 0/2 ([503,503,401,401,403] vs [200,200,...]; v2 adv 503).
  r118 failed to compile (E0642 trait pattern) + fmt diffs. r123 GREEN at dca39eaa0+fmt patch (= 22bc741ee byte for
  byte, blob hashes checked): --lib vcs_http::git:: vcs_http::daemon_edge:: vcs_ssh:: vcs_store:: vcs_policy:: 80/80,
  no rust warnings. r131 at 22bc741ee: fmt check clean; slates-cli --test cli vcs_http 1/1.
  r160 on f2ad56fa0 (merged + refactor): running.
- Finding for main (not mine): slates-server's NON-test build warns 19 dead-code items of lfs.rs (mod lfs; has no
  cfg_attr(not(test), allow(dead_code)) and no non-test caller; A-629 part 2b-ii) -- seen in the cli test's build.
- Land plan: worktree from origin/main, git merge --squash agents/g309-lender, fill @@EVIDENCE@@, one commit, push.
- ~22:45Z main's spellings for the NEXT piece (interop tests), no reply needed: repository made by
  `slates repository create <name> --tool git` (A-685, g305 building); SSH edge `--vcs-ssh <addr>` (A-684, g299);
  Basic with the token as password on the loopback `--vcs-http` edge, Basic's challenge first (A-686, g083) -- until
  then stock git sends `-c http.extraHeader="Authorization: Bearer <token>"`. A test for a piece still in flight is
  `#[ignore = "..."]` naming the piece and its agent; ignore nothing else.

## 2026-11-19 ~22:50Z relaunch (fresh g309, third): A-682 then the interop tests (main's brief)
- r160 was collected (result unknown). Merged origin/main b94ae6e90 into agents/g309-lender (worktree
  ~/slates-work/g309-lender): conflicts in lib.rs, vcs_http.rs, daemon_edge.rs (now `Halves<UploadServices<ShardStores>,
  ReceiveServices<ThisShard>>`), git.rs (Halves kept, Unserved/NoExchange dropped: test-only now), git/upload.rs,
  vcs_ssh/git.rs (A-653 (4)'s admission passed to write), GAPS, design (A-682 entry last). ShardPacks::write and the
  lender test's answer.write take the admission. Dropped my duplicate HTTP helpers in tests/daemon_edge.rs (main's kept).
- Added test a_branch_stock_git_pushed_over_the_daemons_edge_is_listed_and_fetched_whole_over_it (stock git's create
  push through receive-pack, then ls-refs + fetch through upload-pack on the same edge; pack ids == pushed pack ids).
- Next: commit the merge on the branch, push, Studio run of the focused filters, then land (squash onto origin/main).
- ~23:25Z A-682 LANDED: main f7684fc20..5206419be (piece 53b6aae3f; merges f436131d6, 3e7a018ba, e2c280dfc, 2a7cf1247,
  5206419be; worktree ~/slates-work/g309-a682-land, branch agents/g309-a682-land). Runs r247/r279/r297/r318/r355/r385
  all green (last r385: server --lib vcs_http:: vcs_ssh:: vcs_store:: 110/110, cli --test cli vcs_http 3/3, fmt).
  Reported to main; run requests filed. agents/g309-lender and agents/g309-interop are superseded (keep, never land).
## Piece 2: git interop tests (started ~23:00Z)
- Worktree ~/slates-work/g309-git-interop, branch agents/g309-git-interop (from origin/main 693bc8feb; merge main next).
- New target crates/cli/tests/git_interop.rs (self-contained, as svn_interop.rs): control round trip vs stock git
  (file://, runs now), no-credential refusal (runs now), HTTP round trip with Bearer extraHeader (A-685 + A-682 on
  main: NOT ignored), HTTP with Basic in URL (ignored: A-686 g258), SSH (ignored: A-684 g299 + A-683 g302).
- Credentials minted with the anchor's issuer exports (macOS); Linux skips those legs loudly.
- ~23:55Z git interop LANDED: main ba97c5ad5..be892650b (piece f64a6c2fd + merge be892650b). r426 (draft), r452
  (landing tree), r464 (main be892650b): git_interop 3 passed 2 ignored; --ignored fail at their waits. Reported.
## Piece 3 (main's ruling ~23:50Z): the issuer fix for the hg and svn interop tests
- Worktree ~/slates-work/g309-issuer, branch agents/g309-issuer (from main be892650b). cli.rs: anchor_with_edges now
  wraps anchor_with_edges_as(instance, edges, issuer); anchor_with_edges_and_issuer reads the issuer surface and awaits
  a stable bootstrapped daemon (as A-685's start); the two stock_hg round trips mint with run_as_issuer and skip loudly
  (NO_ISSUER_SURFACE) where none. svn_interop.rs: start_anchor returns the issuer surface; slates_as_issuer;
  minted_token(instance, issuer); http_leg -> Option<Leg>; svn:// and svn+ssh legs skip loudly without a surface.
- Proof owed: run each with --ignored on the Studio, past the credential step (SSH legs can't pass --vcs-ssh until A-684).
- ~00:05Z issuer fix LANDED: main c8be7c738..da3349308 (one commit). r471 proof. Reported. Worktrees cleaned.
## Piece 4: A-699 (claimed; main's piece ~00:00Z 2026-11-20): git at scale through --vcs-http, timed vs git daemon
- Worktree ~/slates-work/g309-scale, branch agents/g309-scale (harness crates/cli/bench/git_scale.sh, c6e565e9a).
- Studio scratch: ~/.cache/g309-scale (src.git = git/git bare at f7de81456940, 1027 refs: 8 heads 1019 tags,
  424,851 objects, 324.39 MiB pack, 86,352 commits; slates clone of main 2b86aaa; release build 6m17s),
  target ~/.cache/g309-target. Delete both at END.
- r526: first exploratory run (RUNS=1, BOUND=1800). Then RUNS=3 for the numbers if it works; bugs -> main + own piece.
## 2026-11-20 relaunch (fourth g309): A-699's first record, then A-704 (the push bound), then A-699's slates numbers
- Found: worktrees ~/slates-work/g309-scale (agents/g309-scale = a303f3474, pushed: harness wip x2) and
  ~/slates-work/g309-a699 (agents/g309-a699 at main a7b5e4292, clean, unpushed, nothing of its own).
- r526's output collected/gone; main's brief carries its findings: whole push 413 after 4.4 s; refs/tags/v0.99 alone
  (4,508 objects) 200 + "unpack eof before pack header was fully read" + remote rejected (unpacker error); one-commit
  push works; stock git daemon push 30.3 s, clone 13.5 s (fsck clean, refs 1027 and objects 424,851 identical).
- Studio: ~/.cache/g309-scale (src.git at f7de81456940, slates clone of main 2b86aaa, release build 6m17s, harness =
  a303f3474's), ~/.cache/g309-target (release slates binary).
- Claimed A-704 in amendments.tsv for the fix (next free now A-705).
