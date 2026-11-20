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
  A-730 PART 2 LANDED: main 25494f2 (merged 8096c3b): svn:// lock/unlock hooks; svn serve lock verb = 4 calls
  (lock_verb -> LockHookCall, lock_judged(PreHooked), locks_landed -> post call, locks_finished). vcs_svn 10/10.
  A-730 PART 3 LANDED: main af59477 (merged 1533d52): svn+ssh:// tunnel runs the same hooks (vcs_svn::run_hook with
  HookSite::Tunnel; pre_lock_hooks/post_lock_hook shared). 135/135 focused. A-730 DONE; told main so w-svn can build
  A-702 part 2 (revprop change) on vcs_svn::run_hook (HookSite, Hooked::new, svn_pre/post_revprop_change builders).
  Owed (GAPS): SSH edge client-gone signal for a hook run; DAV's hooks; REPOS as A-645's FSFS view.
  NEW QUEUE (main): (1) DAV hooks -> A-733 LANDED main bad0841 (merged 431b110): DavCommit begin_hooked/resume_begin,
  merge_hooked/resume_merge; SvnStores::begin_commit returns Due; exchange POST/MERGE/LOCK/UNLOCK async with
  HookSite::Http(ClientGone, policy); post-lock/unlock failure counted, never told. Tests daemon_edge a_dav_* 3/3,
  focused 101/101 + svn 171/171, stock svn HTTP on Studio-1 4/4.
  (2) A-645 hook view: SCOPED, reported to main for a ruling (2026-11-20). Findings:
    - design (A-665 part 3 / GAPS): view = read-only volume whose base host reads the repo's content store, shown under
      the root mount (D-1). But: (a) the daemon publishes no root mount (SLATES_ROOT must be named; design 33585 owed),
      so no path to present at; (b) a HostFs is called inside the volume's borrow and cannot reach ShardState's
      vcs_stores, so a lazy view host needs either A-727 a2 (VCS content in the shard chunk arena: a RAM volume
      referencing the same chunks, content once) or a RAM copy of the view (doubles memory for the hook's life).
    - svn's view = FSFS format 6 (physical addressing, sharded): per-revision rev files (node-revs with node/copy ids,
      pred/count, copyroot; dir reps naming children by r<rev>/<offset>; changed-paths; trailer), revprops, current,
      format, uuid; plus transactions/<txn>.txn for `svnlook -t`. Offsets chain across history -> derived data per
      revision. Multi-part: (P1) FSFS rev/revprop generator + stock svnlook oracle on Studio-1; (P2) txn dir for -t;
      (P3) presenter (needs (a)+(b) decided); (P4) git presenter replacing Unpresented.
  (3) Linux run of hook tests: queued on adacachy (kiln run r15, "no room here now"); starts by itself.
  RULING (main): placement (b) now, then (a) after P4; bytes (a), wait for A-727 a2, never copy. Go P1+P2 now.
  A-738 P1 LANDED: main 210843f (merged): slates_svn::fsfs (FSFS 6 view: View::advance a revision/step under a
  node-revision budget, View::revision_file generated on read, texts streamed; revprops/fixed files/dirs), Index::created_in.
  Test crates/svn/tests/fsfs_view.rs 3/3; on Studio-1 (kiln on adas-mac-studio.local-2 = d1323898, stock 1.15.0):
  svnadmin verify ok, dump == full.dump, dump --deltas == deltas.dump, svnlook changed == changed.txt.
  A-738 P2 LANDED: main a75731c (9143016): fsfs::transaction_files from Index::trial (Trial::made/node_revision
  added); node.<n>.<c>(+.props/.children), props, next-ids, changes, txn-protorevs/<txn>.rev streamed. Oracle on
  Studio-1: each golden revision's work tried on R-1 -> svnlook -t == svnlook -r (changed/info/tree/dirs-changed/diff).
  NEXT: P3 waits for A-727 a2 (w-perf) + SLATES_ROOT placement (b); then P4 (git), then (a) root mount publication.
  URGENT (main) A-740 LANDED: spin reports name the task. slates_rt::label (TaskLabel Site via #[track_caller] at every
  spawn entry / Named via futures::doing guard; interned table 4096 cells), Pulse::polling (label+polls, one store per
  poll), watchdog lines "in task L, at its poll P", segment SUP_STUCK_* record -> anchor lapse kill line. Named steps:
  hg_push::{unbundle,record,rebase,take}, vcs_svn::{answer,land}. Tests: rt spin_label, anchor stuck record, liveness
  a_spinning_shard_stops_the_beat (red without the record). Seen: anchor a_table_past_the_old_exec_limit fails sendmsg
  ENOBUFS(55) here -- unrelated to A-740, left for main.
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
