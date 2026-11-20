# w-interop -- progress (UI goal 93: interop with stock git, hg and svn clients proven)

Role: implementer (w-interop), supervisor "slates-7a". Rules: relaunch-2026-11-19.md + briefs/common.md + briefs/w-interop.md.
Studio = kiln runner adas-mac-studio.local-2 (`kiln run -q --on adas-mac-studio.local-2 --no-sync -- "<script>"`),
CARGO_TARGET_DIR ~/.cache/w-interop-target, clone ~/.cache/w-interop/src (delete at END). Scratch ~/slates-work/w-interop-scratch.

## Piece 1: git over SSH (git_interop.rs stock_git_clones_pushes_and_fetches_over_the_ssh_edge un-ignored) -- 2026-11-20 ~04:05Z
- Took over predecessor's uncommitted draft in ~/slates-work/g299-sshinterop (branch agents/g299-ssh-interop, at main
  b63f8a05d): ssh_edge_of (port + fingerprint from the daemon line), known_hosts via ssh-keyscan checked by ssh-keygen -l,
  StrictHostKeyChecking=yes. Committed 43c86824e, pushed as agents/w-interop-git-ssh.
- Run 1 (Studio, focused git_interop + fmt): cut off by the 04:06Z shutdown (signal 15) after fmt clean.
- Run 2 (relaunched agent, 04:10Z): kiln MCP r1 on d1323898 (same script as run1.sh), running.
- Run 2 r1 green: git_interop 5 passed 0 ignored; SSH leg alone 1/1 in 7.4 s, no skip; fmt clean.
- LANDED a0175d10b on main (2026-11-20 04:20Z): test + GAPS lines 433/708. Worktree g299-sshinterop removed.

## Piece 2: hg clone over ssh:// beside g083's stock-hg tests in crates/cli/tests/cli.rs -- started 04:20Z
- Worktree ~/slates-work/w-interop-git-ssh-land (reused for piece 2 on a new branch).
- No CLI import verb exists (ImportRepository has no client), and pushes wait on A-696, so the clone is of an empty
  repository: new test stock_hg_clones_over_the_ssh_edge (clone, empty, paths default, pull "no changes found"), strict
  host key. Commit 1900f5aca on branch agents/w-interop-hg-ssh-clone (over main a0175d10b).
- Studio: persistent clone ~/.cache/w-interop/src, worktree ~/.cache/w-interop/wt (re-added per piece), target
  ~/.cache/w-interop-target. Run r3 (fmt + `--test cli stock_hg_`) running.
- LANDED 630eb8d76 (merge dd614c59f) 2026-11-20 ~04:55Z: r3 stock_hg_ 2 passed 2 ignored, fmt clean. Reported.

## Piece 3: watch and un-ignore -- started ~04:55Z
- A-680 2b HTTP hg round trip already un-ignored on main by g303 (7c71c8048). A-692, A-695 on main.
- r4: svn:// leg run --ignored on main (Studio worktree ~/.cache/w-interop/wt) to see where it stops.
- Worktree now ~/slates-work/w-interop-hg-land (branch agents/w-interop-hg-ssh-land).
- svn:// leg on main dd614c59f (r4): checkout refused `E170001: No access allowed to this repository` (lookup now
  works). Reported to main (w-svn unreachable by SendMessage). Stays ignored.

## Piece 4: widen -- started ~05:10Z
- ci/interop/git-matrix.sh (branch agents/w-interop-matrix, bae36a13b): anchor with HTTP+SSH edges, push a real repo,
  ~26 rows (clone, log, blame, diff, branch, tag, merge, rebase, fetch/prune, shallow, partial, ssh). Prints PASS/FAIL.
- r5: git/git mirror (~/.cache/w-interop/git-git.git) through the debug slates binary, STEP_LIMIT 1800. Running.
- NEXT: record results (GAPS line), report failures to main/w-git; then hg (hg's own repo) and svn matrices.
- r10 (main 9f51d75, own Studio target): full git/git push over HTTP: shard 0 spins 3559 ms, daemon killed, git hangs
  (rewind rpc post data); over SSH closed after 225 s. r11 with PUSH_REFS=v1.0.0 cut (3.4 MB): push PASS both edges;
  clone/partial/ssh clone FAIL "upload-pack: the request needs more room than the server admits"; shallow PASS;
  deepen FAIL "possible repository corruption". 4/27. All reported to main (w-git unreachable).
- Landing: script (same-heads fixed, PUSH_REFS documented) from w-interop-scratch/git-matrix.sh into worktree
  ~/slates-work/w-interop-matrix-land (branch agents/w-interop-matrix-land), one GAPS line, push.
- Rules now: local runs only for landing, own target ~/.cache/w-interop-local-target JOBS=6; svn not on this Mac
  (not installing without Ada); svn legs on the Studio.
- LANDED 4a: 003936b3e (git-matrix.sh + GAPS line). Reported.
- 4b: ci/interop/hg-matrix.sh (25 rows) on branch agents/w-interop-matrix-hg (b6e76c1d0, in worktree
  ~/slates-work/w-interop-matrix-land, over main 003936b3e). r14 on the Studio: hg's own repo (~/.cache/w-interop/hg-src)
  cut at 1.0, then whole, binary at main 9f51d75. NEXT: land script + GAPS line, report failures; then svn matrix.
- LANDED 4b: 478585893 (hg-matrix.sh + exec-failure fix in both scripts + GAPS line). Went in as 3 branch commits
  (b6e76c1d0 "(scratch)", ca4e8a157, the landing) plus merge -- not squashed; told main.
- hg findings (main 9f51d75): HTTP push 413 from 189 changesets; no `lookup` (identify URL, clone -r); SSH push waits A-696.
- NEXT (piece 4c): svn matrix (ci/interop/svn-matrix.sh) run on the Studio (no svn on this Mac; not installing without
  Ada): svn:// is refused E170001 (reported), so HTTP/DAV legs + svnserve control; then re-run git/hg matrices as
  w-git/w-hg fixes land (A-704 follow-ups, 413s, lookup, A-696). Worktree ~/slates-work/w-interop-matrix-land.
- Studio state: clone ~/.cache/w-interop/src, wt ~/.cache/w-interop/wt (main 9f51d75), target ~/.cache/w-interop-target,
  sources git-git.git, git-src, hg-src; matrices' scratch matrix-git, matrix-hg*. Delete at END of goal.
- 2026-11-20 ~06:00Z: never END for context (common.md). Main has 7bedc3a88 (ra_svn commits, svn:// + svn+ssh) and
  5c6eb940d (`slates repository import NAME PATH`). Running svn_interop ignored legs --ignored on the Studio (piece 3).
- LANDED piece 3 part: 41c2e662e svn:// round trip un-ignored (passed --ignored at main 4785858, r17). svn+ssh
  E210002 closed; HTTP legs '/project' path not found (A-681 DAV reads) -> stay ignored; reported.
- 4c: ci/interop/svn-matrix.sh written (uncommitted in w-interop-matrix-land), seeds by `repository import` of a
  git_to_svn_dump.py dump of git/git (LIMIT 2000). NEXT: push branch, run on the Studio, land with a GAPS line.
- LANDED 4c: merge 4ac4c6526 (svn-matrix.sh + GAPS). svn:// 22/27; lock E170003 refused; svn+ssh "svnserve -t is not
  served yet"; HTTP path not found (A-681). Reported.
- QUEUE NOW: watch main; on each fix (w-git: large push spin / upload-pack room / deepen; w-hg: 413, lookup, A-696;
  w-svn: lock, svnserve -t on SSH edge, A-681 DAV reads) re-run the matrix / un-ignore the stock test, land, report.
  Remaining ignored: cli.rs stock_hg ssh round trip (A-696); svn_interop http round trip + locks (A-681), svn+ssh.
- 2026-11-20: checked with merge-base --is-ancestor: a0175d10b 630eb8d76 003936b3e b17700bb0 41c2e662e 2d8f041c6
  4ac4c6526 all on origin/main. Scratch branches agents/w-interop-git-ssh, -hg-ssh-clone, -matrix hold only superseded
  drafts of what landed (checked file by file); nothing finished off main. Monitor on main running (re-arm on expiry).
- 43551dc93 (A-711 hg push admission) and 3691deb5c (A-696, hg SSH push; w-hg un-ignored the SSH round trip) on main.
  r22: hg matrix re-run (cut 1.0 and whole) on a fresh build of main. NEXT: land a GAPS line with the new count.
- hg re-run (r20, main bc2803d): 17/24; push of hg history (cut 1.0 or whole) now spins shard 0 and the daemon is
  killed (`bad HTTP status line`); reported for w-hg.
- LANDED 168ddfa96 (on main, checked): svn+ssh round trip un-ignored (passed --ignored at main a146359, r21); svn
  matrix 24/27 (lock E170003, HTTP A-681 remain). Reported.
- r22: git matrix re-run for A-713 (a2f0fb970) with cuts v1.0.0 and v2.0.0. NEXT: GAPS line + report.
- Rule (main): an A-number is mine only after my push of its amendments.tsv line on agents/fanout-state succeeds
  (pull, append next free, bump "# next free", commit, push; on reject repeat). My pieces so far need none.
- r22 v1.0.0 cut on main 168ddfa: git matrix 26/27 (A-713 fixed clones); checkout-tag fails only because the cut push
  has no tags (script) -> change row to check out an old commit.
- r22 v2.0.0 cut (56 MB): push spins shard 0, daemon killed (both edges). Reported.
- LANDED 78eea35b9 (merge a304ec440; on main, checked): git matrix checkout-old row + GAPS line (git 26/27 at v1.0.0
  since A-713; v2.0.0 push spins; hg 17/24 since A-711/A-696). Monitor b69za9xas on main (re-arm on expiry).
- Gotcha: never pgrep for 'git fetch' in a wait loop: the monitor's fetch loop matches forever.

## Piece 5 (main ~07:30Z): A-681's remainder -- stock svn over http(s). Handed from w-svn.
Done when: svn_interop.rs HTTP round trip un-ignored + passes with stock svn 1.15, svn matrix HTTP rows pass.
State on main: svn DAV site (crates/svn/src/dav/site.rs: OPTIONS, PROPFIND, GET/HEAD, REPORT log-report; others 501),
server site crates/server/src/vcs_http/svn.rs (SvnSite, SvnStores) and lender vcs_store/svn_view.rs ServedRecords --
but SvnSite is NOT composed into the daemon: ToolSites<G,H> (vcs_http.rs ~349) routes hg wire else git -> '/project'
404 for svn. Sources: ~/slates-work/g297-sources/{dav-1.15.0,svn-1.15.0,httpd-2.4.66}; g297 progress lines 235+.
Plan: A) compose: ToolSites gets svn, routed by shape (method not GET/HEAD/POST, or a `!svn` segment) and daemon_edge
builds SvnSite over ServedRecords; B) update-report (checkout/update: request::UpdateReport -> report::drive -> dav::Drive,
`me`/vcc resources, OPTIONS capabilities); C) commits (HTTPv2: POST me create-txn, PROPPATCH txn, PUT txr svndiff,
MKCOL/DELETE/COPY, MERGE -> repository::svn_commit::commit_work). Each part its own landing. A-numbers: claim via
agents/fanout-state amendments.tsv only if a design change needs one (A-681 parts are A-681's).
Also: re-run git matrix for A-719 part 1 (81c36bd99) and hg matrix when w-hg's push fix lands.
- Part A written: svn::dav_request + test a_request_is_subversions_by_its_method_or_its_special_segment;
  ToolSites<G,H,S> with Svn arm; daemon_edge DaemonSvnSite over ServedRecords. Scratch b8903064f pushed as
  agents/w-interop-a681. MISTAKE: ran `git checkout -b x` in w-interop-matrix-land (rule breach; no file change) -- the
  worktree is now on local branch `x` (same commits); left as is, told main. Mac disk 33 GB -> tests on the Studio.
- LANDED part A: 04da3ae92 (merge 7b46fbdcd; on main, checked). Unit 7/7 (Studio r24-ish), stock svn over HTTP:
  info/ls/cat/log pass. Worktree ~/slates-work/w-interop-a681-land (branch agents/w-interop-a681-land) for B and C.
- Part B next: REPORT on `!svn/me` (ra_serf report target under HTTPv2, util.c svn_ra_serf__report_resource) is
  Resource::Unserved -> "requested report is unknown". Need: resolve `me` for REPORT; Asked::Update(UpdateReport) ->
  report::drive::Drive (new(report, uuid)) -> dav::drive::Drive writing; streamed as Report variant like log.
- Part B written (scratch 3e4dfd7de, pushed agents/w-interop-a681b; worktree w-interop-a681-land): report.rs
  WholeCommand/WholeItem/WholeRefusal/whole_report (reuses Reporter checks); dav/update.rs UpdateWalk (src/dst URL ->
  repos path, depth, items, streamed resume via dav::Drive::take_written + Body::drain); site.rs Resource::Me (!svn/me),
  Walk {Log, Update}, Answer/Report now Debug-only derives, OPTIONS SVN-Allow-Bulk-Updates Prefer; test
  an_update_report_on_me_streams_a_checkout_and_refuses_a_revision_past_the_youngest. r25 on Studio running.
