# w-git progress (UI goal 93 "The git server built"; supervisor slates-7a)

## Piece 1: A-704 the large-push bug (briefs/g309.md item 2; harness state progress/g309-interop.md)
- Started 2026-11-19 (w-git, fresh). Worktree ~/slates-work/g309-a704, branch agents/g309-a704 (was clean at
  b63f8a05d, nothing of the predecessor's; ff to origin/main). ~/slates-work/g309-scale = A-699 harness wip (w-perf owns
  the A-699 record; keep).
- ~2026-11-20: red a2eacbaef (both tests red on Studio: git "eof before pack header"; server "unpack-objects abnormal
  exit"). Fix 4e7d0df5a + fmt: git_push::push_room = Store::admittable() for both edges; header count before record
  charge; band-2 "fatal: pack exceeds maximum allowed size". Studio run (clone ~/g309-work/a704, target
  ~/.cache/g309-a704-red-target): git --lib receive:: 45/45, server --lib git_push:: vcs_http::daemon_edge::
  vcs_http::git:: vcs_ssh:: 90/90, fmt patched by hand. Landing via worktree ~/slates-work/g309-a704-land (squash),
  message ~/slates-work/g309-a704-land.msg.
- Owed (GAPS): unpack into quarantine a slice at a time; whole git/git push e2e (main's run).
- A-704 LANDED: main f6308ccdc (piece c34de85e1). Next: queue item 2 (A-665 part 4 hooks).

## Piece 2: A-665 part 4 (relaunched w-git, 2026-11-19 late)
- Worktree ~/slates-work/w-git-a665p4 (branch agents/w-git-a665p4, ff to origin/main 73cbdfc42, clean). Old
  ~/slates-work/g293-a665p4 (b63f8a05d, clean, nothing of mine) untouched.
- PresentView as briefed found multi-day (view in git_push not the shard; read sites pass slot.host without vcs_stores;
  daemon doesn't know its root mount path; by-use test needs a real mount). Sent main options (a) volume in 3-4 pieces,
  (b) interim on-disk presenter under the state dir. Awaiting ruling; meanwhile building item 2b client-left signal.
- Main's ruling (~20:35 PST): (a) the view volume, in pieces (view table + &mut seam; slot-host wrapper at read sites;
  configured root-mount path + view volume naming; by-use test over the CLI's NFS loopback mount); (b) banned (R1, §2
  item 3/7). Do 2b client-left and 2c learned limits first. I am the only git worker.
- 2b (A-665 part 4a, HTTP ClientGone -> Exchange::body_end(gone), receive passes gone.gone() to HookCall::run) and 2c
  (A-665 part 4b, vcs_hooks::HookTimesTable on ShardState.hook_times; git_push PushHooks::{call,learn}) written in
  w-git-a665p4, uncommitted; 2b docs in design/GAPS (@@RUN@@ placeholder); 2c docs script ~/slates-work/msgs/w-git-2c-docs.py.
  Local run log ~/slates-work/msgs/w-git-2bc-run2.log. Plan: commit 2b (vcs_http files + docs), commit 2c, merge, push.
- Next after: 2d SSH client-gone (Command::step gets ClientGone over the SSH edge's TcpStream), then (a)'s pieces.
- Shared target mixed worktrees (reported; main's new rule: own target ~/.cache/w-git-local-target, JOBS=6, new test
  names must appear). Committed 068c18ca2 (2b) + ea969a52d (2c) on agents/w-git-a665p4 (pushed; messages hold @@RUN@@):
  land via fresh worktree from origin/main, cherry-pick, fill run lines, merge, push. Run log w-git-2bc-run4.log.
- Run5 (own target, w-git-a665p4 tree + test fix): 72/72, new test names present. Landing worktree
  ~/slates-work/w-git-a665p4-land (branch agents/w-git-a665p4-land from origin/main eb1d50a33): 97fdfedde (2b) +
  30fc43407 (2c, with test fix), run lines filled. Main touched slates-server -> re-run on land tree (w-git-2bc-land.log).
  Then push HEAD:main, amendments row, run-requests line, report, remove w-git-a665p4, start 2d in the land worktree.
- ~21:05 (A-704 session, told by Ada to continue): found a fresh w-git already on item 2 (worktrees w-git-a665p4 at
  ea969a52d with A-665 4a+4b, w-git-a665p4-land). Stood down; no code written there. Briefly overwrote its uncommitted
  git_push/tests.rs edit and restored it exactly from ~/slates-work/w-git-drafts/stray-git_push-tests.diff. Draft
  presenter host (unused, untested): ~/slates-work/w-git-drafts/view_host.rs.
- A-665 4a+4b LANDED main 0ea7a3304 (97fdfedde, 30fc43407 + merges 64d588796, merge2, merge3). amendments row +
  run-requests line filed. Next: TOP PRIORITY push-spin bug (main: stock git push of git/git all refs, 607 MiB pack,
  shard spins ~4 min in; repro ci/interop/git-matrix.sh on agents/w-interop-matrix, row push-all), then 2d SSH gone,
  then route A presenter (draft ~/slates-work/w-git-drafts/view_host.rs).
- A-713 (clone room bug) LANDED main 03dc601a5 (a2f0fb970 + merge). Worktree ~/slates-work/w-git-pushspin
  (branch agents/w-git-pushspin). Found while there: a 1,200-commit fetch's serve step ran >1 s in debug (upload-pack
  negotiation+plan unsliced) and a 3,000-commit push answered after 1.7 s (push-spin candidates). Next: push spin.
- A-719 part 1 (push checks sliced: receive::Checking, Session::check_turn, Answering::Turned) LANDED main 20ffba05f.
  Main's ruling: A-719 = whole streaming intake (store each object once on arrival in the content store, deltas
  against stored bases, bounded working set, admission by working set, §4.17.1 rule in the amendment); prove with
  w-interop's v2.0.0 cut then git/git. Coordinate with w-perf (A-718 lands first; tell w-perf before changing how a
  fragment's bytes are held; keep pushed deltas for pack reuse). Also owed: quarantine admission + verdicts sliced.
- A-719 part 2 (unpack::Keep keeper seam; §4.17.1 "A push's intake" rule incl. concurrency clause) LANDED 183150ab8.
  Main: no hold_bytes, no new store; keeping client deltas compressed in the EXISTING ContentStore is OK; convergence
  plan from w-perf decides blobs later. Next: part 3 = session/driver keep straight into the quarantine as the pack is
  read (removes reader + Checked in-RAM copies; admit_all gone), checks reading objects back from the store.
