# w-reaper progress (event-driven reaper goal). Supervisor "slates-7a".

## RESUME HERE (2026-11-20, Mac Studio relaunch)
- GOAL 93 (svn over HTTP), main's order: (1) DAV reports svn blame/diff ask, (2) pre-lock/post-lock hooks (A-693 p2 owed).
  A-726 PART 1 LANDED: main c1eaea6 (merged 749a56a). file-revs-report (reads::DavFileRevs via file_revs writer enum;
  dav/report_file_revs.rs), get-locations (reads::trace_node_locations; dav/report_locations.rs); site.rs REPORT arms
  only. E2E crates/cli/tests/svn_http_reports.rs (own copy of svn_interop harness -- w-interop owns svn_interop.rs,
  locking.rs, site.rs method match, commit.rs, server vcs_http/svn.rs, vcs_store/svn_view.rs). Stock svn 1.15 lives on
  Studio-1 d1323898 /opt/homebrew/bin; run e2e there via kiln run (on d1323898, PATH=/opt/homebrew/bin:$PATH,
  CARGO_TARGET_DIR=~/.cache/w-reaper-target). svn sources (1.15.0) fetched to ~/slates-work/svn-src.
  A-726 PART 2 LANDED: main 4511d1d (merged eae99ac): dav/report_mergeinfo.rs, reads::repository_mergeinfo, OPTIONS
  mergeinfo yes. Stock svn merge/blame -g/mergeinfo over HTTP == svnserve (svn_http_reports 2/2 on Studio-1).
  PIECE (2) RE-SCOPED by main: ra_svn runs ALL svnserve hooks (svn:// + svn+ssh://) = A-730. Not blocked: svn hooks
  run in "/" (HookPoint::directory), view_root unused; REPOS = "/<name>" until A-645's FSFS view (design item 9).
  A-730 PART 1 LANDED: main e2a65fe (merged b8e4591): svn:// start-commit/pre-commit/post-commit (vcs_svn::run_hook,
  answer_hook + land async; svn Pending::lock_tokens). Tests vcs_svn 7/7 (+2), svn_hooks/vcs_hooks/hook_driver/vcs_svn
  58/58, svn commit::/serve:: 90/90. Test hook scripts are warmed once (macOS assesses a new executable 0.16-0.29 s at
  its first exec -- NOT a driver latency; an earlier "finding" withdrawn).
  NEXT part 2: svn:// lock hooks: pre-lock per path (token from stdout), post-lock once; pre-unlock per path,
  post-unlock once -- split serve/locks.rs lock_verb/locks_landed so the server can run hooks between judging and
  answering. Then part 3: svn+ssh:// (vcs_ssh/svn.rs). Worktree ~/slates-work/w-reaper-a730 (branch w-reaper-a730).
- State branch lives in ~/fanout-state-w-reaper (own folder, main's rule 4d39b11; ~/fanout-state is shared, untouched). An
  A-number is mine only once my push of its amendments.tsv row succeeds (pull, append next free, bump, push at once).
- Piece A (A-709, base hints by readiness + delegation lapse) LANDED: main 9b93fd77c (merged ea05b2ad6). Tests macOS
  arm64, Mac Studio, local: base_hints::tests 1/1 (drained 10.4 ms), deadlines::tests 10/10, slates-base hint_set 2/2,
  bridge-nfs earliest_unreturned 1/1, fmt. Branch agents/w-reaper-hints deleted (landed).
- Studio setup: clone ~/slates-work/w-reaper-hints (its own checkout on local main; land from there), target
  ~/.cache/w-reaper-target (CARGO_PROFILE_TEST_DEBUG=0 to save disk; disk ~16 GB free, under the 20 GB guard from
  other users). After a merge, `touch` the tree's .rs files: mtime fingerprints reuse stale artifacts otherwise.
  Commits need `git -c commit.gpgsign=false` (kiln signer refuses non-kiln sessions); push via
  git@github.com:hyper-light/slates.git (https has no credentials here).
- Pieces B+C (A-715) LANDED: main f70230cc8 (merged 60e23101a). crate::idle_sweep runs the idle sweep at its own
  deadline on the keeper (armed by Store::sweep_owed, arena allocation/allocated-bytes change, deferred frees; re-armed
  while a pass leaves work); daemon::reap_loop deleted -- no reap tick remains. Tests macOS arm64, Mac Studio, local:
  idle_sweep 2/2 (an idle shard ran 0 passes in 3 intervals), give_back 1/1, purge 2/2, deadlines 10/10, base_hints
  1/1, idle 5/5, vfs seal 5/5, fmt.
- Tick-name cleanup (main's last piece, no A-number) LANDED: main 4c6445f (merged ae70481). Docs/names in bridge-core,
  vfs, server, cli; test renamed the_formation_check_allows_every_idle_sweep_event; bench IDLE_TICKS -> IDLE_PAUSE_NS.
- Follow-up (own initiative, no A-number) LANDED: main 00b9e79 (22345f7): deadlines/landing/nfs keeper tests no longer
  sleep out the deleted reap loop's first look (PAST_THE_FIRST_LOOK_NS gone); 4/4 in 1.55 s.
- QUEUE DONE: 1 (A-475, on main from g255), 2 (A-522..A-570 g255; A-709, A-715 w-reaper), 3 (A-715 deleted the tick).
- Finding (read, not chased): a start's free-content/recovery give-back (3.2 GB on the Studio) is interrupted by the
  control shard's 100 ms heartbeat every pass, so it takes ~10 passes (one an interval) -- same pace as the old tick.

## State found at start
- On main already (g255): A-475 (dead clients by hang-up), A-522, A-536, A-554, A-570. Queue item 1 DONE on main.
- reap_loop on main ran process_base_hints + delegation::drain and seal_idle_content at LIVENESS_BUDGET_NS.
