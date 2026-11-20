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
- S3 claimed A-722 (ledger c112181). Plan sent to main: (1) A-719 part 3 now, (2) verdicts+seal sliced, (3) S3 after
  w-perf's S2 (blob-before-tree: keep compressed entry in index charged to window, typed refusal past it).
- PART 3 DESIGN (in progress, worktree w-git-pushspin, branch agents/w-git-pushspin):
  slates-git: unpack: split `Contents` (content(at,id)) out of `Keep`; receive: `Pushed{objects:&[Placed], by_id,
  contents:&dyn Contents}`, object() -> (Kind, Cow); Checked/Received.unpacked = Option<Result<Kept,PackRefused>>;
  check()/decide() take contents; Checking fsck + Walk read via contents; Session takes a combined site
  (Keep+Contents+Repository) in *_into variants, old methods use session's own Held; receive() uses Held.
  server: git_push keeper over (&mut ContentStore, &GitStore, &mut GitPush) implementing Keep+Contents+Repository
  (+Outside via Bases); GitPush held from pack start; prepare = seal only; abandon when all refused.
- Part 3 slates-git side done (local wip commit on agents/w-git-pushspin, not pushed): Contents/Keep split,
  finish_into(K: Keep+Outside), Unpacked::into_kept, Pushed{objects:&[Placed],contents:Option<&dyn Contents>},
  Checked/Received.unpacked = Kept, Session input_into/end_into/check_turn_with/decide_with/answer_with + kept(),
  works::PushedBytes (PushedObjects impl). slates-git --lib all green. Next: server git_push queue keeper.
  Land: fresh worktree from origin/main + cherry-pick -n + real message (wip must not reach main).
- Part 3 squashed as 3105843fe on agents/w-git-a719p3-land (worktree ~/slates-work/w-git-land), merged origin/main
  (conflicts with A-723 S1 session pack retention + A-724 vcs_share room/Live: both kept; prepare keeps the pack on the
  sealed push). Merged tree: git 159/159; server focused 153 pass, my t_a719 bound fixed for A-723's kept pack (passes),
  vcs_ssh::tests::hg::stock_7_2s_push_over_ssh fails "a step that waits" -> checking on clean origin/main (worktree
  w-git-maincheck, log msgs/w-git-maincheck-hg.log). Merge commit not yet made (changes staged in w-git-land).
- main/w-balance seam for A-722 accepted: vcs_share::room(state, repo) -> Room{bytes,bound}; keep vcs_share::Live.
- A-719 part 3 LANDED main baf0fd76a (3105843fe + merges). Worktrees: w-git-land (landed, clean); w-git-pushspin
  removed. Target trimmed to 20G (incremental + stale .rcgu.o). Reported: hg ssh push test fails on clean main.
  NEXT: verdicts + seal sliced (A-719 part 4), then S3 (A-722) on main/w-balance's vcs_share::room seam.
- Measured append (9k objects debug): commit 159ms, moves admit 54ms, seal 26ms, verdict 2ms. Main APPROVED folding
  into A-722: done = index rows staged in slices as objects land (append only makes visible), moves' admission + seal
  sliced, landing O(changes), push on vcs_share::room seam. Proof: w-interop v2.0.0 cut (36,430 commits, 56MB) then
  git/git, no watchdog kill, RSS per push. Design note (~20 lines) to main after the read (Explore agent surveying
  merge push/work + git store APIs). Worktree ~/slates-work/w-git-land at origin/main 2fb7d49b3 (clean).
- S3 survey (Explore): blockers for "blobs only in the green": (1) upload-pack/encoding/sparse/hook views read blob
  bytes via ContentStore::object_bytes(row ContentId); (2) Repository::commit/locate/fresh need every object in Push
  quarantine; (3) merge has no op setting a path from a held identity (w-perf slice 1b owed); build copies blob bytes
  into Increment.post_state; one work per ref, not per commit; (4) held table not checkpointed (1c owed); (5) recovery
  replays recorded pack into ContentStore; paths::put_file copies blobs into ContentStore. Design note owed to main.
- TOP PRIORITY (main): git/git push spin. Doing: slates-git receive::Deciding (one command a step; decide() = same
  machine whole) DONE (builds); next server AnswerState::Deciding + hooks path; then seal sliced; then staged commit.
- A-719 part 4 (decision a command a step) LANDED main c9100be04 (e4f2aeaf7). A-645 hook view moved to w-reaper; SSH
  client-gone to w-k90-vendor. Stay on A-722: staged index rows as objects land (append only makes visible), moves'
  admission + seal sliced, landing O(changes), vcs_share::room seam. Design note (~20 lines) owed to main.
- A-722 design note SENT to main (staged index in a per-push Index::fork; positions reserved at pack header
  base+pack-index (holes question); non-commit rows staged at keep; commit rows + generations in sliced pass after
  pack; moves' admission sliced pre-append; landing = new Index::adopt (O(runs)) + staged values; stale fork re-base;
  admission window+post-state on vcs_share::room; blobs-as-green-ops later, blocked on w-perf 1b/1c + green blob
  source). Awaiting objections; meanwhile checking readers' tolerance of position holes (Positions, upload runs).
- MAIN APPROVED A-722 points 1-7; point 2 change: readers tolerate position holes (fix any dense-assuming reader,
  e.g. hook_view units.rs cover over Positions::span(0,next_position)); NO compaction at landing. Point 8 reframed
  (ruling B): index resolves oid -> (pack identity, offset); upload/encoding/sparse decode from stored pack in place;
  head files from the green; ContentStore::object_bytes inflated copy + put_object go away. Build 1-7 first, then 8.
  Piece order: (a) slates-vcs Index::adopt (+ parent epoch / stale check) ...
- A-722 part 1 (slates-vcs Index::adopt) LANDED main de9591c70 (adfe74896). Next A-722 part 2: positions taken as each
  object arrives + readers tolerating holes (hook_view units.rs cover, text.rs depth loop, any 0..next_position user);
  then part 3: rows staged into a per-push fork at keep time (git store Push/Repository API), commit rows + generations
  sliced after the pack; part 4: moves' admission sliced; part 5: landing adopts fork (O(changes)) + stale re-base.
- Main additions to A-722: (1) after part 3, intake on the ARRIVAL shard (fork there; decode/hash/check there; only
  the landing's adopt on the owner via cross-shard message); placement-policy fn from w-balance (kiln id
  01a1f7b42ec27702a195501719d3f919 / inbox) — agree the seam. (2) Concurrency proof: 19 then 100 concurrent pushes to
  one repository (v1.5.0 cut kills daemon at 19 today; run-requests.md), no watchdog kill, every push answered.
- A-722 part 2 in progress (w-git-land, uncommitted): rows lose green/version; Repository::landings map +
  bringer_of(position); upload + hook_view units switched; store tests fixed. Running git tests.
- A-722 part 2 (rows w/o green/version; landing map bringer_of) LANDED e42c53886 (c1aa1a5b1). Next part 3: stage rows
  into a per-push fork as objects land (positions at arrival; commit rows + generation sliced after pack), landing
  adopts the fork. Then concurrency (19/100 pushes) and arrival-shard decode (w-balance seam).
- TODO after part 3: t_a665 hook tests wait on push's own answer with deadline derived from shard step time x steps (like w-shared da4dc2ff0), not fixed PUSH_WAIT_NS.
- A-722 part 3 in progress (w-git-land, uncommitted): Push.Staged (fork, positions, commits, whole); Repository::stage
  / stage_commits / stage_room / commits_room; commit adopts fork when adoptable else legacy (holes); hook_view cover
  skips holes; vcs Index::adoptable + fork_room; server GitPush admit_all stages (lends rows room), seal stage_commits.
  slates-git store tests green incl. 2 new; server tests being run.
- A-722 p3 design refined: tentative positions (base = next_position at fork, no holes); ONE landing path: commit
  adopts the staged fork when adoptable, else restage() (fresh fork, arrival order) then adopt -> repository index
  keeps no tail, so fork timing never changes tables and log replay matches. Replay stages too (InOrder keeper keeps
  resolution order); intake Queue keeps resolution order. Removed legacy fresh/positions_for/rows_for/insert.
  A-639 by-type positions given up (arrival order) -> flag to main to measure fragment-set runs.
- REGRESSION found before landing part 3: long-history push 6s/fetch 15.5s vs main 1.9s/4.1s (arrival-order positions
  break A-639 by-type runs; per-object staging slow). REWORK: stage the whole push at SEAL (positions by type then id
  as before, rows via insert_all into a fork), landing adopts (or restages = stage again). Revert per-object stage,
  arrival order, intake ordered Queue, replay InOrder. Part 4: slice seal+stage pass and make record replay the same
  sliced intake path (main 2026-11-20: replay of a big pack record runs in ONE turn -> crash-loop; failing test first:
  replaying a many-object pack record takes many turns).
- A-722 part 3 LANDED 82ff3bc79 (main 7b93d68c3), 2026-11-20 ~03:15: seal stages whole push into index fork (by-type
  positions), landing adopts or restages; push 2.0s/fetch 3.7s vs main 1.9/4.1. Ledger + run-request done.
- A-743 LANDED f6ca826e6 (main 4d9de40c9), ~03:45: CreateBranch records new green's git ref (no-object push record,
  vcs_store::commits::branch_made); git_interop stock_git_sees_the_branches un-ignored. Finding sent: assert_made_again_whole
  fails after plain pushes (content holders differ).
- NEXT: A-722 part 4 in worktree ~/slates-work/w-git-refs (reuse after fetch): (a) failing test first: replaying a
  many-object pack record takes many turns; (b) slice the seal+stage pass and moves' admission; (c) remaining O(objects)
  in commit; (d) sliced restage on stale. Then concurrency proof 19/100, arrival-shard decode (w-balance seam),
  t_a665 derived deadline, ruling B.
