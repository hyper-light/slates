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
- A-722 part 4a LANDED cafd1058f (~04:30): vcs_store::git_replay (record decoded once, pack read in chunks
  archive_slice_bytes_within(ns), objects admitted one a unit, deadline = step quantum; record_turn in rebuild turns
  and recovery units; Lent guard per turn since A-727 a2). Next 4b: slice seal+stage pass and moves' admission (live
  GitPush::seal + admit), then landing's O(objects) passes, stale restage; thin-pack resolve waits on w-balance's slicing.
- A-722 part 4b LANDED 8ceff213f (main 5014be90a, ~04:40): slates_git::store::Staging (generations, fresh via
  Quarantine::object_after cursor, positions by per-kind counts, rows inserted per object, Index::seal_step), live
  AnswerState::Sealing + SealingPush::turn, replay seal through Staging, Drop gives back Sealing/Deciding.
  NEXT 4c: moves' admission (admit_moves / SealedPush::admit walk) + landing O(objects) passes + stale restage sliced;
  then by-use proof 10/19 concurrent pushes after w-balance resolve-in-turns lands.
- A-722 part 4c LANDED 51fff3e6f (main d435c91b7, ~05:15): slates_git::store::Admitting (walk/descent/base/beyond units),
  Repository::admit = units to end; receive::Judge::ready (default true) called before verdict in each_step/atomic_step;
  ServerJudge::ready → SealedPush::admission_turn keeps Move in sealed.prepared; SealedPush::admit takes it when
  to + current_head match. Measured & sent: work build + apply dominate (works/merge).
  NEXT 4d: store commit passes (brought_by, landed_runs, plan/members_positions, release_holds) + stale restage sliced;
  by-use 10/19 proof when w-balance-fix1 (76070d077) lands on main.
- A-722 part 4d LANDED a8aea3990 (main 14a85fbd2): replay thin-pack resolve in turns (Reader::resolve_turn_with).
- BY-USE PROOF in progress: release build on adas-mac-studio.local-2 (kiln run r2, target ~/.cache/w-git-target.noindex,
  CARGO_INCREMENTAL=0) of main d435c91b7 (has w-balance 032617278). Then: python3 crates/cli/bench/multi_repo_load.py
  <slates> ~/.cache/g309-scale/src.git v0.99 v1.0.0 v1.5.0 --steps 100 --per-repository-cap 10, then without cap (19).
- 2a49cf8a2 LANDED 4b race fix (staging forks + reads base at its first unit). ffa5be0f6 LANDED A-753 (creates made from
  the green holding the commit; resent objects read from index in connectivity; cap checkpoints members).
- BY-USE (Studio .local-2, release; harness logs ~/.cache/w-git-runs/, daemon logs ~/.cache/w-balance/daemon-logs/):
  (a) multi_repo_load R=100 cap10 77/77 ok longest 72.8ms peak 4046MiB; (b) uncapped 86/86 longest 76.5ms 4390MiB;
  (c) one repo N=19 19/19 1337-1607MiB; (d) N=100 80/100 (green_chains cap) -> A-753; rerun r19 after fix pending.
  Studio clone ~/.cache/w-git-clone has DIAG commit merged in: next time use a fresh worktree from origin/main there.
- NEXT (main's order): (1) N=100 rerun; (2) name the 72-76ms longest step (answer phase) -> 4e store commit passes if
  so; (3) memory per push attribution, identical objects stored once, bounded per-push working set; then two
  load-only tests (daemon_edge long-history push, t_a665 update hook), Windows restart spin; record (a)-(d) BENCHMARKS.
- Local target is now ~/.cache/w-git-local-target.noindex, build with CARGO_INCREMENTAL=0.
- A-722 part 4e LANDED a986d5f1d (main b832652b0): restage in Judge::ready turns; empty stage never stale; each_step ready
  fix (4c's admission turns were inert live before). Merged with w-engine's A-752 (ready = restage, admit, build work).
  BENCHMARKS "A-722's proof" section records runs (a)-(e).
- NEXT: (1) slice connect.rs propagate (check phase 57.9 ms); (2) load-only edge tests: edge sends nothing until the
  answer -> move answer loop into reply + git side-band keepalive "0005\1" every receive.keepAlive (5 s, git default)
  (ClientGone needed for hooks: driver hands it only to body_end); (3) memory per push attribution; (4) store commit
  passes; (5) t_a665 derived deadline; Windows restart spin check. Note: w-balance's t_a724_a_rebuild_reads... is
  load-flaky (passes alone).
- 2026-11-20 ~11:15: LANDED 4f 061f4956a (propagate a stack step a unit), 4g 6737b04a7 (replay moves admitted an object
  a turn: r110 restart spin), keepalive 61b178d42 (main 85d66d9cb). Local target DELETED on main's urgent order (Mac
  disk); NO cargo on this Mac: builds/tests via kiln run with no `on` (CARGO_TARGET_DIR=~/.cache/w-git-target.noindex,
  CARGO_INCREMENTAL=0). Post-merge confirm run r20 in flight.
- NEXT: ruling asked on http-edge status-at-head (streamed routes) for the two long-history edge tests; then memory per
  push (attribution, identical objects stored once, bounded working set); then Repository::commit's O(objects) passes;
  t_a665 derived deadline. Findings sent: svn ssh test fails after w-svn 28e57ac18.
- ~11:50 LANDED A-722 head at once 51158a4fb (main bbc6aa614): StatusMode::AtHead in http-edge; git smart services.
  Told w-interop/w-hg/w-svn/main. Build route now: push a work branch, then kiln run (needs macos,tool:cargo,cpus=4,
  sync false) in ~/.cache/w-git-clone worktree ~/.cache/w-git-wt on the Studio (kiln's repo sync times out).
  r119 (w-interop): git cut 24/27 with zero spins/kills/restarts thanks to 4g.
- NEXT: memory per push (attribution: pack buffer, fork, index rows, objects; identical objects stored once by
  identity across concurrent pushes; bounded per-push working set), then Repository::commit O(objects) passes, then
  t_a665 derived deadline.
- ~12:40 LOAD CONFIRM sent: long-history edge tests 12/12 at load 55-59 (release, .local-2, 60 PID burners).
- MEMORY PER PUSH (Ada's MEMORY SIZING rule, briefs/common.md): ATTRIBUTION (diag branch agents/w-git-memdiag, test
  git_push::tests::diag_memory_of_one_push, request file ~/.cache/w-git-runs/v150.req on .local-2): v1.5.0 push 9.4 MiB
  -> admission peak 34.4 MiB transient; ContentStore 309 MiB after = every object inflated (git: 304.6 MiB inflated:
  blobs 231, trees 69, commits 4.5). Plan = ruling B, sent to main:
  B1 rows name object by (pack ContentId, entry offset, kind, len); pushed pack held once by identity.
  B2 reads (Place::Indexed, IndexedObjects, bytes_of, PushedObjects, hook_view text, graph, sparse, revision, derived
     trees/commits, paths) inflate from the pack in place; delta bases via a per-shard scratch sized by EWMA of the
     working set, given back idle; no per-object alloc; turned read for long chains.
  B3 quarantine keeps entry places, not inflated objects (intake admit_queued -> GitPush::admit_all -> admit_object
     -> Push::admit -> Quarantine::admit put windows).
  B4 remove object_bytes/put_object inflated copies (upload/encoding/sparse read the pack). No hold_bytes, no new store.
  Red per step: resident <= k*pack + base, k derived. Worktree ~/slates-work/w-git-packed (from origin/main 86e23c4ef).
- ~13:20 LANDED memory B1 ea5f0d6da (main a011645e6): store::packed (PackSource seam, read in place, Scratch). Seam note
  to w-store sent. Builds/tests: kiln run with `on: adas-mac-studio.local-2`, sync false, clone ~/.cache/w-git-clone,
  worktree ~/.cache/w-git-wt, CARGO_TARGET_DIR=~/.cache/w-git-target.noindex CARGO_INCREMENTAL=0 (kiln once placed a
  needs-only run on this Mac: never again). NEXT: B2 rows by (pack, offset) + all readers via packed::read; B3
  quarantine by entry place; B4 drop inflated copies; red: resident <= k*pack + base.
- ~14:10 LANDED eea6b39f6 (main ab049942c): Connection::interim + Exchange::interim; receive-pack keepalive while the
  pack is read. Debug-load rerun filed in run-requests.md. NEXT: measure the push input decode per byte/object
  (release, cold/warm; main's push-back: 820 KB should be ms) and fix if it's the gap; then B2 in-flight half
  (quarantine reads the pushed pack in place while judged; landing via w-store's seam, no pack store; Ada's changeset
  ruling: history = changesets; git objects derived & cached by identity).
- ~14:40 decode measured (diag branch agents/w-git-decodediag): release cold input 25.9 ms (2.9 us/obj, 31.6 ns/B), answer
  71 ms/8.5k steps; debug cold input 300 ms, answer 1.42 s/29k steps -> decode not the gap; reported to main.
- B2 IN-FLIGHT HALF DESIGN (Ada's changeset ruling: what lands is changesets via w-store's seam; no pack store):
  * The pushed pack is already kept whole in RAM (session take_pack -> GitPush::keep_pack -> RecordWriter.pack). Make it
    the in-flight PackSource (a RAM pack: slice source over that Vec, identity = ContentId::of / pack checksum).
  * Quarantine (slates_vcs::arrival::Quarantine Staged{kind, content, len}) gains an in-pack place for git objects:
    the intake keeper (git_push::intake::Intake::keep) names each resolved object (SHA-1) then keeps only its entry
    offset, dropping the inflated bytes; Contents::content / Quarantined::content read through store::packed::read.
  * Landing (Repository::commit) still needs object bytes in the content store until w-store's changeset store: put
    them at the commit from the pack (turned), so the in-flight peak drops; landed size unchanged until w-store.
  * Red: in-flight resident of a v1.5.0 push before landing <= k*pack + base (today the quarantine holds inflated).
  * Worktree ~/slates-work/w-git-packed has a DIAG commit at HEAD: make a fresh worktree from origin/main first.
- B2 STEP LIST (worktree ~/slates-work/w-git-inflight, fresh from origin/main 62e5e5009):
  A. slates-git Push owns the in-flight pack (Vec, appended per run by the server before session.input_into) and is
     a PackSource; Push gains `placed: BTreeMap<id,(Kind, offset, len, identity)>` beside the vcs Quarantine; Push
     accessors (object, held, held_after, len, is_empty, abandon/release_holds) cover both.
  B. Repository::locate gains Place::InPack{kind, offset, len}; bytes_of / references / tag reads go through
     store::packed::read (Bases = index + push placed); PushedObjects view the same.
  C. Staging's Rows unit puts a fresh in-pack object's bytes into the content store (put_object) for its row contents,
     a unit a turn (landed size unchanged until w-store's changeset store).
  D. Server intake admit_queued records places (identity via ContentStore::object_id, no put); Quarantined::content
     reads in place; the session stops keeping its own pack copy (take_pack -> the push's).
  E. Record writer names by identity (unchanged); its pack is the push's.
  F. Red: in-flight resident of a v1.5.0 push before its seal <= k*pack + base (diag measured 16.9-34 MiB admission +
     content 14 MiB+ during input today).
- ~13:20 main's HEAD-after-restart report (Windows v1.0.0, 16/27): diagnosed as harness, not slates — jupiter's
  git-src HEAD is main, git-matrix.sh creates the repo with --default-branch from the source HEAD, PUSH_REFS pushed
  master only; stock git answers the same. Main routed the script fix to w-interop.
- B2 in-flight half WRITTEN (branch agents/w-git-b2, 6c33bc0ca; worktree ~/slates-work/w-git-inflight): Push keeps the
  client's pack + placed map, Push::bytes reads in place (packed::Recent cache, budget = pack len), staging puts fresh
  objects (put_placed), hook view from staged contents, record writes push.pack(), session keeps pack_len only.
  Studio (kiln .local-2, own target): check clean; slates-git --lib 579 passed incl. both b2_ tests; server --lib
  git_push/vcs_store/git_hooks/vcs_http/git_protect 198 passed (pre-merge). b2 bound test: 4 MiB inflated -> held in
  store 4,194,304 B before, in place 41,568 B (the pack). v1.5.0 diag before/after running (target-rel, delete after).
- 14:00 B2 in-flight half LANDED 5518fb2bc. 14:15 LANDED 00e6da185: t_a665 derived deadline (Progress per step,
  TEST_BOUND_NS each) + commit's landed runs by position (no positions clone, no id map/sort). Main recorded both.
- v1.5.0 diag (release): before 14.4 MiB content after input, 173,409 steps, 6.71 s; after 0.0 MiB, 229,619 steps,
  9.73 s (+45% CPU). Suspect: staging reads in id order -> chains re-inflated. Fix drafted (agents/w-git-order
  cf0941c6e: Push.order = admission order, staging Step::Putting before Forking). Profile run r42 (sample) pending.
- Replied to w-store's changeset-log ask (inbox/w-store-from-w-git-changeset-log-2026-11-20.md): names section.
- 14:40 LANDED 47dc24b72 (staging puts in resolution order: 229,619 -> 168,761 steps). 15:00 LANDED 15917f29a
  (walk notes staged positions; one move one run). 15:10 LANDED ccd07b58c (holds given back in turns after the
  landing). v1.5.0 store commit 14.0 -> 0.63 ms; longest step 35.8 -> 23.4 ms, of which the entry's apply is 19 ms
  (pack in record; asked main: home the pack as content, or wait for w-store's changeset payload). Main's socket
  slates-9a is gone; reports are in inbox/main-from-w-git.md.
- Diag kit on the Studio: ~/.cache/w-git-diag/{v150.request, diag2.rs, phase_patch.py, phase_patch2.py}; release
  target ~/.cache/w-git-target-rel.noindex (delete when done).
- NEXT: git_replay reads a record's pack whole (Unpacked) -> read in place as the live push does.
- 15:35 LANDED fc8b29c27 (RECOVERY BUG: thin push record diverged on replay -> repo refs lost; names compared as
  sets). 16:05 LANDED 5c6327b8e (replay reads its pack in place: v1.5.0 rebuild RSS +189.6 MiB -> +0.1 MiB).
- Waiting on main: pack homing as content before the append (19 ms apply). Studio release target
  ~/.cache/w-git-target-rel.noindex still there for diags; delete when the memory work ends.
- 16:20 landing step split further: apply = greens commit 1.7 + recorded_entry (work encode) 3.3 + record_all (db
  write of the 11 MiB entry) 18.3 ms. Took main's 14:20 note (landed half stays with w-store) as the answer to the
  pack-homing question; sent w-store the numbers (inbox/w-store-from-w-git-entry-apply-2026-11-20.md).
- 16:40 MAIN'S ORDERS (inbox w-git-from-main-priority, 15:55): 1 partial clone regression -> LANDED 7b66e7225
  (kind_at from landing kind runs; size only when filter reads it; daemon CPU/partial 740 -> 300 ms). 2 v4 pack
  homing (inbox w-git-from-main-pack-homing): IN PROGRESS. 3 commit 14 ms: done earlier (ccd07b58c).
- v4 PLAN (interface sent: inbox/w-store-from-w-git-v4-payload-2026-11-20.md):
  B. rows by pack place first: staging puts the push's pack as one content (a window a unit, before census);
     object rows: contents [pack identity] + payload offset (ObjectRow.offset); Repository::read_object(store,row,
     content) via packed::read (Repository-owned Scratch/Recent, budget learned); readers: views IndexedObjects,
     graph bytes_of, store/upload whole objects + hook_view Object{content}, encoding.rs, derived tree/commit;
     Push::put_placed/Step::Putting go away. Store per v1.5.0 push 309 MiB -> ~10 MiB + rows. Record unchanged.
  A. then record v4: record names pack (identity,len) + names (identity,count) homed via vcs_store::homes; replay
     reads pack via content store PackSource; v3 removed (no second path).
- 17:40 v4 part B LANDED 92f7a756b (rows by pack place; v1.5.0 store 309 -> 14.1 MiB). Reported to main inbox.
- v4 PART A PLAN (in progress, worktree ~/slates-work/w-git-land9):
  A1 record.rs VERSION 4: head, moves, names (identity,count), payload (identity,len); no inline pack. v3 writer AND
     reader removed (main: no second path); v3 tests -> v4.
  A2 SealedPush: packed: Option<(ContentId,u64)>; record() writes v4 when names and pack both homed, else v2.
  A3 git_push Homing: Due -> names Asked -> homed -> PackDue -> pack Asked (push.pack() bytes) -> homed -> Done;
     edges (vcs_http git receive, vcs_ssh receive, git_push tests driver) call take_homing/homed in a loop.
  A4 replay v4: names from content; pack read from the content store by identity (GitReplay chunks from windows;
     replay_pack whole via object_bytes); record holds the pack content as v3 held names.
  Measure: v1.5.0 landing step (entry apply 18 ms -> ~0) and record bytes; red: no step past the quantum landing v1.5.0.
- 18:35 v4 PART A LANDED 7a52ef882 (merge 33f17ba0d), tested at c20861789 over ed021dc42 (git 582, server 166 ok).
  Close note: inbox/main-from-w-git-close.md (P4 7b66e7225, P5 ccd07b58c+15917f29a closed; C1 part done, C5 with
  w-store left). Worktrees land9/land10 removed; branches v4a, v4a-land, memdiag, decodediag, a665p4 deleted; Studio
  release target and diag worktrees deleted. Mac worktree now: ~/slates-work/w-git-state (state branch) only.
- R2 (edge tests under load) IN PROGRESS: repro r139 at load 78-83, 8x the three tests: 19/24 fail, all in the PUSH's
  chunked body (daemon_edge.rs:571 -> chunked_body), not the fetch. Suspects: Receive::answer_until awaits that skip the
  keepalive deadline: turns::take, home_in_region (names + pack), names_committed_within. Diag r140
  (~/.cache/w-git-diag/r2_patch.py, never for main) times each.
- 2026-11-21 00:45 C5 part 1 LANDED origin/main 0e7120bdd (record v5 + changeset landing + store recovery; red test
  t_a767_c5_a_git_push_lands_as_one_changeset_and_a_restart_replays_it_from_the_store; r13 green 211 passed, 6 A-664
  failures that also fail on main). Close note in inbox/main-from-w-git-close.md. Next: C5 part 2 (store-less git
  creation refused + git leg of w-hg's test; v2 and sequence 0 removed; git tests onto test_store; StorePacks replay),
  then R2 (edge tests under load: push answer_until awaits skip the keepalive deadline; r139 repro 19/24).
  Studio-4: my old target, clone and worktree deleted (58 GiB free there after).
- 2026-11-21 01:05 C5 part 2a LANDED cf6b3c98a: git refused without --vcs-store; 76 tests onto test_store; edge suite
  StoredDaemon; v3_takeover/git_interop anchors with --vcs-store. Next: C5 part 2b (v2 + seq 0 removed, homing/landing
  mandatory, StorePacks replay), then R2 (keepalive across answer_until's awaits: carry homing/landing,
  names_committed_within; the edge tests fail under load; landing's flush makes it longer).
- 2026-11-21 01:00 R2 CLOSED 970950081 (first keepalive after the body was 2 intervals = 10 s; r28 red 18/18 -> r29
  green 18/18 at load 29-33). Next: C5 part 2b piece 1 (pushes v5 only; carried shortfalls refuse; names race
  refuses; moves-only v5; the v4 test switch removed; v2 left for made commits and sync test drivers' untaken asks),
  then 2b-2 (sync drivers async), then made commits onto w-svn's deferred-submit hook.
- 2026-11-21 01:10 C5 2b piece 1 LANDED 3ce898533 (pushes v5 only; shortfalls Unlanded; switch gone). Next: piece 2,
  the tests' synchronous push drivers (answered/pushed/pushed_from_the_fixtures and the in-test loops, ~30 sites)
  made async on stored daemons, and an untaken homing ask refusing; piece 3 waits for w-svn's deferred-submit hook.
- 2026-11-21 C5 2b piece 2 LANDED f5a14f6a8 (untaken asks refuse Unlanded; carry helpers; region holds apart in
  replay). Piece 3 (made commits onto the deferred-submit hook, record_of/v2 removed) waits for the hook on main.
- 2026-11-21 01:31 PRIORITY, the push cut-off (w-interop: git/git whole-history push over HTTP cut at ~60 s, curl 18).
  Diagnosed by a DIAG build (Studio-2 ~/.cache/w-git-relwt, patches ~/.cache/w-git-diag/{cut,phase_cut}_patch.py,
  binary slates-phasediag, repro push_repro.sh into ~/.cache/w-git-diag/pushrepro). The cause: an answer step refused
  PastAdmission after the 200 head, so the edge aborted. Phase: the checks (`unbegun/checking`) asked 485,230,824 B
  with 300,809,433 left; it is the connectivity walk's whole-pack edge table (slates_git receive::connect begin_edges).
- 02:20 Layer 1 LANDED 379087733 (merge df349a9fe): refusals after the body go out as band 3 words / unpack+ng
  report, the reply ends clean, counted; git_push::words shared with SSH. Red 2/2 at 94ff89fe5, green 2/2; focused
  51/51 + git 49/49 on the merged tree. main and w-interop told (inbox *-push-cut*).
- NEXT (main 01:56): layer 2 before C5 2b piece 3. The checks a slice at a time over the pack held once, charging
  only the slice (no whole edge table); red = the 611 MB push refused today, then landing within its room; A/B vs
  stock git (release, interleaved, all N). Merge w-shared format 6 piece 1 when it lands.
- 02:40 Layer 2 piece 1 LANDED b295ea7b3 (merge b7d2ae37e): connectivity check charges per object, no edge table
  (red asked 134,176 B / 2,254 left). The 611 MB push no longer refused; it lands its changeset.
- 04:10 Layer 2 piece 2 LANDED 8baa1cf16 (merge 339f7e73d): stage_room from counts; restage_turn skips when nothing
  due (decision step 0.5 ms -> ~30 us). Focused on merged tree: git store 115, receive 50, server 53.
- Measured (Studio-2, git/git push): checks 16.0 s, seal 3.7 s, homing+landing 1.3 s, decision >350 s unfinished.
  The decision wall: a create walks its whole history for its green's reached set (A-614), refs x history. Options
  (A) lazy reached sets, (B') generation order + peeled-commit head map; asked main (inbox
  main-from-w-git-layer2-decision.md), holding. Next on my side meanwhile: the checks' 16 s.
- Diag kit on Studio-2: ~/.cache/w-git-diag/{cut,phase_cut,timeline2}_patch.py, push_repro.sh, slates-l2diag.
  Removed there: w-git-wt, w-git-wt2 (worktrees), w-git-target.noindex (8 GB).
- 05:10 A-614 amended, (A), LANDED (40d961807 + docs 36883edbd, merge 30e89672f): creates, rewrites and fast-forwards
  over sets never walked are admitted with no walk; sets derived on first read (store::Deriving, sliced, memoized, base
  by search back); policy by git's two-colour walk; server derives before fetch and encode (vcs_store::git_derive);
  replay walks nothing. git 587/587, server git set 188/188 on the merged tree.
- NEXT (main 04:42): (2) the >1 s unit in the push decision (watchdog kills the daemon: shard 10 spinning 1,142 ms,
  vcs_http.rs:1454): name it with a unit-length counter, red, slice it. r42 times each part of a decision step.
  (3) path maps per created ref derived on first read (A-664/A-752), oracle + sliced/memoized + replay flat; red =
  git/git push lands; then the A/B vs stock git. w-engine told (inbox/w-engine-from-w-git-path-maps.md).
- 06:20 A-238 amended LANDED 4f6242707: a push makes the chains room before its append, a green a turn (red: 73
  append checkpoints -> 0). The >1 s unit was repository::push::admit folding ~1,000 member greens in one append
  (r44's stack samples). Each fold whole until w-engine lands 507110661. NEXT: path maps derived on first read.
- 07:10 r52 (branches only on ca444739c): push 72.5 s ok, mirror clone 465 s ok (fsck clean); derive turns 3.0 M,
  encode units past budget 844 k.
- 07:55 derivation pacing LANDED 2b2fc8d80 (A-783 pace; red 2,404 turns -> <= 8). Finding: t_a752 flakes under load
  on main too (1/2 at load 12.8).
- 06:30 wide_connectivity clippy fix LANDED ca444739c (w-lint told).
- P2 (main 05:12): created refs' greens unmaterialized, paths derived on first read. Engine half: w-rewrite-ops
  (interface agreed: PushObjects as source, DeferredPush, green_ready, Applied::Deferred). My git half waits for it.
- NEXT while blocked on the engine half: C5 2b piece 3 (made commits onto the deferred-submit hook, A-804 landed
  5a21c11dc; record_of/v2 removed in the same series), per w-rewrite-ops' made-hook note.
- 08:15 C5 2b piece 3 STARTED (main 07:05: go now). Plan, from reading made_commit.rs (A-804) and svn_made.rs:
  1. made_commit::Tool::Git; made_repository_of recognises a git repository's green (shares svn's landing Turn type).
  2. vcs_store::commits gains make_deferred(state, (repository, turn), pending, judged): the version's trees built from
     a preview of the held verdict (Greens::preview/with_preview, as svn_made::revision_held), the commit's objects into
     a GitPush, sealed; a task carries Home (names, pack) then Land (changeset at the next sequence), then
     made_commit::commit(state, pending, |state| guard entry; store commit of the sealed push; append the register
     entry with a v5 record naming the sequence). Store refused -> made_commit::abandon.
  3. Remove the in-step path (commits::make from verbs::judge_submission, record appended in the submit's transaction)
     and git_push's record_of / v2 writer (and v2 replay) in the same series.
  Reds (w-rewrite-ops' list): answered only after both (the durable head 0 in the verb's step); a kill between the land
  and the commit recovers to before, and the retry lands once. Open question: the version's trees from a preview need
  vcs_store::paths' map/trees of the preview's post-state (A-664).
  Piece 3 design refinement (08:30): paths::stepped(&engine, ...) takes an engine ref, so the version's map/trees can
  be stepped from the preview (state.greens.with_preview(&preview, |engine| stepped(...)) as svn_made::revision_held
  does). It KEEPS the map as the green's git head at the version before the version is durable: acceptable as git's
  derived data, but an abandoned made commit must drop that kept head (restore the one before). A green needing a whole
  build (needs_whole) gets it sliced in the made task before the landing (A-664's whole_build machinery), never in the
  submit's step. Then commits::made's objects from Head::trees -> GitPush sealed -> task: carry Home, carry Land ->
  made_commit::commit(tool = guard + store commit + append v5 entry). The owed path (staged/make_owed) folds into
  this; record_of/v2 removed after.
- 08:50 piece 3 RED committed locally (w-git-c5b, not pushed): vcs_store::commits::tests::t_c5b_p3_a_git_made_commit_is_answered_only_once_its_changeset_landed_and_committed: in the verb's step (true, 1, 0) answered+durable. Next: Tool::Git in made_commit, commits::make_deferred from a preview (paths::stepped over with_preview), the landing task, then the kill test, then removing record_of/v2.
- 09:45 main ruling (b): piece 3 after P2's engine half. Piece 3's red kept on agents/w-git-piece3-red (cf289fb7d);
  old worktree w-git-c5b removed (its only unpushed commits were the red and its revert). New worktree
  ~/slates-work/w-git-p2 from main f52934d65.
- P2 engine half LANDED by w-rewrite-ops (A-812, 7e110566d): BranchChange::CreateDeferred, DeferredPush, green_ready,
  with_owed_push_objects hook. My git half in w-git-p2 (uncommitted, not compiling yet):
  * works.rs: Entry::Create { name, from, push: DeferredPush } (push_of of the eager create); prepare builds nothing
    for creates.
  * vcs_store::with_owed_push_objects: git arm (PushedObjects over an empty Push).
  * git_push::created: CreateDeferred; the unborn default -> BranchChange::UpdateDeferred { green, push } (ASKED of
    w-rewrite-ops, inbox/w-rewrite-ops-from-w-git-update-deferred.md; not on main yet).
  Left: git crate works tests (Entry::Create increment), server tests reading created greens' files directly (route via
  the door / lend), the oracle (derived map = eager build, branches and tags, before/after replay), red = git/git push.
- 2026-11-21 ~11:40 PST: P2's git half LANDED, A-821 (main b2fbe6bf3; docs renumbered 955d6d645, w-svn took A-820).
  BranchChange::UpdateDeferred built by me on CreateDeferred's pattern (main 08:10). The materialization's turn reads
  inside vcs_store::with_room (red: build refused Malformed, pack read Unread{at:166}: windows live in the lent chunk
  store). A push reaches greens only through the door (ShardGreens notes the owing green; build turn / delete wait,
  bounded: a refused build declines the command). A create passes over an owing non-default holder (t_a719: 40 creates
  at one commit waited on each other's makes). Tests: t_p2 unborn default (red on main 14,273 quanta -> <=1 turn,
  made once), t_p2 branch+tag oracle (one owed push byte for byte across a replay before/after make; made = eager files).
  Focused 298/298 single-threaded (server git_push repository::push green_ready vcs_store vcs_ssh vcs_http), git receive
  50/50. Load-sensitive (not this change): t_a752 `quanta > FLAT_FILES` (3 of 4 alone pass, routed before); t_a614a
  (mine, 2b2fc8d80) 15 turns vs 8 under the parallel suite, 3/3 alone pass — owed: bound its turns by wall time/ceiling.
  Line to w-rewrite-ops: inbox/w-rewrite-ops-from-w-git-p2-git-half-landed.md.
  NEXT: red = git/git's 611 MB push lands (Studio, push_clone_repro.sh), then A/B vs stock git 10.77 s (main's runs);
  then C5 2b piece 3 on A-816 (red on agents/w-git-piece3-red). MacBook target trimmed 5.7 -> 5.4 GB; removed
  ~/.cache/w-git-wt, w-git-clone, w-git-diag (clean, nothing unpushed).
- 2026-11-21 ~12:00 PST: main 08:20: r61 (git/git push red, release) queued by main; A/B after. Fold t_a614a's
  wall-time bound into piece 3's series. PIECE 3 STARTED in ~/slates-work/w-git-p2 (from main 30d2e4100; red test
  applied from cf289fb7d, uncommitted). Design:
  * made_commit: Tool::Git (made_repository_of via vcs_store::paths::git_repository_of); defer -> commits::make_deferred.
  * make_deferred: hold_preview(judged); spawn task holding the turn.
  * task turn 1 (map): paths::stepped(&preview_engine, (store, vcs_stores), ...) through a new made_commit lend giving
    the preview engine + &mut store/vcs_stores (disjoint fields). Change step if the head before has a map; whole over
    the preview for a root's first version; else whole_build::start(head version) sliced first, poll, then the change.
  * then commit_objects -> GitPush::admit_all (already a pack of whole objects) -> seal -> sealed.admit(moves).
  * homing in the task: names_now/put_names + keep_for_region, pack_to_home/hold_for_record + keep_for_region,
    landing_of + disk.land(...).await, sealed.landed_as(seq).
  * step: made_commit::commit(state, pending, |state| sealed.record (v5) -> check -> super::commit(store) -> mutate).
  * abandon before the step: drop the kept head (paths) so a retry builds again; release homed; sealed.abandon.
  Later sub-pieces: the kill test; removing record_of/v2 and after_submit/staged/owed; t_a614a wall-time bound.
- 2026-11-21 ~13:30 PST: (1) P2's daemon-test regressions FIXED and LANDED alone (main's order): fa318e929. A git
  green made through the door derives its map (paths::derive_once_made); t_a727 windows test asserts both halves
  (records' windows back from the image; main's map back, same version and tree, after first read); A-727 a3b submits
  its file; new t_p2_a_pushed_creates_object_is_had_by_its_identity_on_first_read_across_a_warm_restart.
  (2) C5 2b piece 3 part 1 LANDED: 8b23c6fdd, A-822. Recorded git submits take the made_commit hook (Tool::Git):
  map over the held preview (offset: preview v0 = green head), commit sealed as a pack, homed + landed, v5 record in
  the one step. Red t_c5b_p3 green; t_a664 parked test now a clone oracle; t_a614a wall-time bound folded in.
  Focused: daemon::tests verbs merge_service vcs_store made_commit git_push repository green_ready vcs_* identities
  544/544. Flake seen once: daemon::tests::the_pairs_canonical_dialer_keeps_a_peer_that_dialed_it_and_only_it.
  NEXT (piece 3 part 2): the kill test between landing and step; remove record_of/v2 with after_submit's owed path,
  non-recorded submits, push_through; then the change step over the preview sliced. r61 (git/git push) is main's.
- ~14:30 PST: part 2a (kill test) LANDED 946b60b2d (note: pushed after a merge that touched commits.rs before
  rerunning; reran after: 276/276 on that main). Part 2b/2c WIP on agents/w-git-p2-part2 (not compiling yet):
  * commits.rs: make/make_owed enqueue MadeJob on VcsStores.made_after (bounded by client seats; landing turn
    take_now at enqueue else awaited by driver); drive_made_after -> made_after: sealed_of -> home_and_land (shared
    sync steps first_ask/next_ask, async via git_push::carry, blocking via git_push::tests::carried_blocking) ->
    recorded_after (own txn, v5). Test drain: commits::made_after_now (called in paths::tests::version_saying).
  * vcs_store.rs: SealedPush::record v2 arm -> Err(Unlanded); record_of removed; record.rs finish/finish_as/
    whole_pack removed. vcs_store tests: appended/push_unsettled land via home_and_land_blocking.
  * LEFT: vcs_store/tests.rs Live oracle (commit_part uses record_of -> must write v5: names_to_content + pack by
    identity + finish_packed), hostile-record tests (move to v5 bytes), assert_a_head_nobody_sent (finish -> v5);
    then reader drops v1/v2 (record.rs decode; Record.objects/pack fields; replay_objects inline branch; git_replay
    pack_at; stepped()); then compile, focused tests, docs (A-822 + GAPS), land. Then the sliced change step.
- 2026-11-21 evening: renumbered A-823 (P2 git half) / A-824 (piece 3) by main's ledger, LANDED 010fd1fd3. Part 2
  LANDED 1a35ab091 (A-824): MadeAfter queue for committed versions' commits; v2 writer gone; v1/v2 refused
  RecordRefused::Retired at replay (red t_a824_…); oracle on v5; queue turns forgotten on state drop (abort fix);
  record holds kept through free (release_record_holds test-only until a destroy verb). Focused 557/557 + git 50/50.
  Main is now kiln agent 01a1ff311fc972a8a0f720e354891538 (agents_send). NEXT (main's order): (1) refs to blobs and tag
  objects refused (junio-gpg-pub, v1.0rc1..rc6), red first; (2) whole-clone "possible repository corruption"; (3) push
  50-52 s vs 9.6-10.2 s, shallow clone 3.82 vs 0.90 s, A/B all rounds. Then the sliced change step (A-824 owed).
- 2026-11-21 late: A-836 LANDED b61cdf8d1 (refs to any object: tag chains peeled, blob refs at the empty tree; t_a836).
  A-824's sliced change step LANDED 9b46d364f (PreviewChange a slice a turn; t_a824_a_made_commits_map_…; counter
  vcs_commits.made_map_turns). Piece 3 series DONE.
  (2) whole-clone abort: w-sessionlog's red by use (inbox/w-git-from-w-sessionlog-clone-corruption.md): master + many
  annotated tags of master commits aborts; master alone / + one tag clones. Not the refused refs. My unit red
  t_goal93_a_whole_clone_of_a_line_with_an_annotated_tag_of_every_commit_is_whole (uncommitted, w-git-p2): 40 tags of a
  line, encode to idle, fetch all: PASSES with flat blobs (41 versions in 5,191 turns: each tag's first version encodes
  everything it reaches, O(tags x history) -- a perf/RSS finding to report); now trying delta-heavy content.
- 2026-11-21 ~22:30 PST: main 01a20182 (session uds 23415.sock; kiln 01a201824d307ecc9e135c64ae3cb848) ruled bug (2)
  YES as A-850 (claimed, pushed). Bug (1)'s cause (w-sessionlog rerun): Negotiation(PastAdmission{asked 7912, left 283})
  -- the never-finished encoding holds the room. (1) after A-850; agree file split with w-balance (piece 3, resumable
  admission) in writing first. Landed meanwhile: 39c36af9d, 03c0e2e14 (upload refusal counted+logged), 3e910809f.
  A-850 RED written (uncommitted, w-git-p2): t_a850_tags_of_held_commits_bring_their_tag_objects_alone: 2,620 brought
  vs 160 pushed. Design:
  (a) store admitting.rs `unwalked`: `from` also found by peeling a tag chain (push or index bytes) to its target and
      taking the green+version heading it.
  (b) deriving.rs: a first version (!before) with a base brings indexed_alone(walk) (relative), reached = base u walk
      (today brought = reached).
  (c) record v6: each move carries its base (green key, version); SealedPush::record writes it; replay hands it to the
      admission (no search); v5 refused typed (Retired like v1/v2? or Version). Recovery test replaying the base.
  Runs (negotiation/one_round) start after a common version >= 0, so D(0) never enters a run: no plan change needed
  for runs; fragments of v0 shrink to the tag object.
- 2026-11-21 ~23:59 PST: A-850 LANDED a34a40909 (merge b099adee4). Main 01a20182's 22:40 ruling taken whole: a
  create's or a rewrite's admission finds its base (A-753's, through the commit its tags peel to, else the search back
  by generation to the nearest commit a committed version peels to: the peeled map, a commit a unit, counted
  base_searched); D(0) relative to it (base less its own tags); record v6 names each move's base, v5 refused typed
  `Version`, replay takes it (`Repository::based`, `StoreRefused::Base`); a push without atomic decided oldest commit
  first (`receive::Judge::rank`, outcomes in the order sent). Red r16 shape t_a850_r16_… (1,012 commits, main + 506
  tags sent by ref name, empty repo, one push): 3,542 brought = objects, each once; encoding 517 turns; fetch whole.
  Red with ranking off: 9,557. Recovery: t_a850_a_recovery_replays_each_moves_base_from_its_record.
  Focused: slates-git store (163+), receive/negotiation/upload 170/170; slates-server vcs_store/git_push/vcs_http::git/
  daemon_edge/repository/git_hooks/git_protect/daemon t_a767 all pass but 3 not in git's path (repository::tests::
  each_verb_moves_the_names_log…: "doubling rule's inputs say fold, yet no checkpoint was taken"; vcs_http daemon_edge
  a_mercurial_push_larger… and a_mercurial_stream_clone_larger…), failing before and after merging main.
  Left (GAPS): atomic pushes' moves meet no base among each other; a fetch's plan does not follow a base. NEXT: bug (1)
  (a fetch cut short while the encoding runs), after the file split with w-balance in writing.
- 2026-11-22 ~01:40 PST: A-855 (bug 1) LANDED 51e280aba (merge 81dc7ca3b). Cause measured: each edge admitted an
  answer min(request room, shard room), the request room one client region = 32 slots x 2 x 4 KiB = 256 KiB (Studio-2
  anchor log), shards with GiBs free; the walk's tables are ~148-165 KB for 506-1,019 tags of a line (unit probes), more
  for git/git's history, so its clone outgrew 256 KiB -> PastAdmission after `packfile` -> abort. Fix: the answer is
  admitted its repository's shard room (HTTP and SSH; the request body still the edge room); a refusal past it before
  the pack begins is held (Answer::holding/held/written/skip_written/give_up), the HTTP edge makes it again in a wider
  room passing over what it wrote (counted vcs_http.git.answered_again), gives up only past OBSERVE_BUDGET_NS.
  Reds: daemon_edge a_clone_of_git_gits_many_refs… (quarter region, red: cut), a_fetch_past_its_room_waits… (red: ERR);
  slates-git upload::tests::clone::a_clone_held_past_its_room…. Focused: slates-git upload+negotiation 120/120;
  slates-server vcs_http/vcs_ssh/vcs_store::upload 166 pass + the 3 hg daemon_edge tests failing before this change too.
  Left (GAPS): SSH edge's wait (gives up at once); a refusal after the pack began still cuts. NEXT (main 00:31 ruling):
  encode_behind's memory (2.1 GB with no client), red first.
- 2026-11-22 ~03:10 PST: LANDED (merge 0209973f7): 14974cfd3 (clippy: Sections::Died boxed, reach.rs's useless
  conversion; GIT_CONFIG_COUNT=0 in git_correctness Git::run + CGI, winfsp_semantics, make-lfs-exchange.sh) and A-856
  53f717ae7 (encoding parts charged; encode_behind only with a fetch live and within the learned grant; idle give-back).
  Grant = slates_vcs::partition: min(room/repositories, curve usable) + marginal-utility lookahead; curve counts content
  windows read (262,144 B here), so 262,144 = one window read, 786,432 = three: learned. Focused: slates-git store+upload
  186, upload+reach 95; slates-server vcs_store/vcs_share/git_push/vcs_http::git 191, git_encode 3/3.
  NEXT: item 1 (deferred build PastRoom regression + master lock, red: one-commit branch push), one wait mechanism with
  w-hg (shared piece in vcs_share); then the clone pack-size gap (4 GiB vs ~300 MB); deflate reuse.
- 2026-11-22 ~03:45 PST: LANDED 2d9626f25 (record version read from VERSION_LANDED in t_a727b/t_a767/t_c5b_p3/t_a724;
  PushedHistory type alias) and A-857 61c15790b (DeferredPush keeps no room, record v2; a build runs within
  vcs_share::room as built; past it waits via vcs_share::wait::RoomWait, shared with w-hg; red a_deferred_build_past_its_
  room_waits_for_room_and_is_made). Focused: merge push 21, git receive 51, server repository/green_ready/vcs_share/
  git_push 149, daemon t_a727b/t_a767 2. NEXT: main's matrix + git/git rerun (master "cannot lock ref" judged there);
  then the clone pack-size gap (4-5 GiB vs ~300 MB); then deflate reuse; A-855's fetch wait onto vcs_share::wait owed.
- 2026-11-22 ~04:35 PST: A-859 part B LANDED 6e795cf41 (merge 1721d8c52): pushed pack entries sent on as they lie
  (fragments and the stream's loose stage), checked against the object's name first; deltified 17,299-byte push clones
  in 20,858-20,916 (89,439 before). Also clone.rs A-855 test split (clippy). Focused: slates-git store+upload+
  negotiation 217, slates-server vcs_store/git_push/vcs_http::git 171. NEXT: A-859 part A (a fetched byte priced at the
  network, §4.11), then the recent-objects cache given back when idle (main 04:24), then deflate reuse.
- 2026-11-22 ~05:05 PST: clippy splits LANDED f142f5853 (encoding.rs reused, upload.rs stream reused, clone.rs A-855
  test); A-860 LANDED b51098de3 (merge 4af9997f8): an idle repository lets go of its recent-objects cache from
  give_back_idle. Focused: git 217, server vcs_store/vcs_share/t_a859 143. NEXT (main 04:53): git_interop whole-repository
  test prints each clone edge's pack bytes (slates, stock) and reuse counters; then the stored-pack order if it refuses
  reuse; then A-859 part A; deflate reuse.
- 2026-11-22 ~06:40 PST: A-859 stream counters LANDED 6ac1ee4be (vcs_store.git_upload.* on the shard; git_interop's
  whole-repository round prints each edge's clone time, pack bytes beside stock's, and reuse counts, from a guard that
  speaks on a killed clone too). A-859 order+reuse LANDED 3fa634049 (merge 2acbba249), per main's 05:38 sample/ruling:
  a stored entry always reused (would_try gate removed); a delta's still-wanted base placed first (write_one) in the
  stream and the encoding; the part's base lookup a map; Positions::next_from on a bit table from from's word (was a
  scan from the table's start per loose object); reuse_no_entry and reuse_unread counted; StreamCount alias (clippy).
  Reds: deltified clone 20/39 deltas then 117/160 encoded afresh -> 39/39, 0; encoding base-first 0/7 -> 7/7.
  Focused: server 197, git 195. NEXT: main's git/git round reads the counters; A-859 part A; deflate reuse; the
  upload/encoding hook for w-shared's read-through (Away windows).
- 2026-11-22 ~08:20 PST: A-859 part A LANDED 52411acd9 (merge f2fdfa2a9): a fetched byte priced at its link (HTTP
  writes that waited, per shard), cold at deflate's measured break-even p0 = d6/s6; written_bytes counted. Goal 93
  item 2a driver hook LANDED 41b1e2b49 (merge c207cb0b1): packs made resident before a fetch and an encoding, a turn's
  waiting window brought back; 2b(a)'s let-go must wait for no fetch live (told w-shared). NEXT: A-864 (claimed),
  CRC32 reuse per main 07:31; then deflate reuse.
- 2026-11-22 ~09:40 PST: A-864 LANDED 5f3ced8ea (merge 33797d4f7): rows keep each pushed entry's raw length, CRC32 and
  base (git's pack index); reuse checks the CRC32 and copies, no object read; a miss rebuilt (reuse_crc_missed,
  reuse_rebuilt). Reds: a864_a_clone_sends_each_... (3/3 rebuilt -> 0), a864_an_entry_whose_crc_misses_... Focused:
  server 277, git 199. NEXT (main 07:31+): one-turn look-ahead residency in place of whole-pack resident, gauge of
  peak resident windows; then deflate reuse.
- 2026-11-22 ~11:30 PST: A-864 piece 1 LANDED 8dfa22349 (merge 03dfc4544): loose stage in pack order (88 backward reads
  -> 0; clone = pushed pack's 17,299 B in 175 turns vs 403). Piece 2 LANDED 7a5ea782a (merge 27cae9bf7): a turn meeting
  a let-go window stops cleanly, the next turn's two windows brought back ahead; a fetch no longer makes packs resident
  (only a due encoding). NEXT: piece 3 (let-go behind the cursor, peak_windows gauge) on w-shared's let_go_read with
  2b(a); then the encoding's look-ahead; deflate reuse.
- 2026-11-22 ~13:35 PST: A-864 piece 2's encoding side LANDED f37113d45 (merge 1930df65d): an encoding unit meeting a
  let-go window stops cleanly, its window brought back alone; no whole-pack residency remains. git_interop reports each
  heavy step (main's r28: the plain clone killed past 120 s said nothing). Freed 2.8 GB of leftover .rcgu.o. NEXT:
  main's run of the per-step print (plain clone's counters); piece 3 on w-shared's let_go_read; deflate reuse.
- 2026-11-22 ~15:10 PST: A-864 piece 2 stall fix LANDED 06aa0e2ce and the LFS `.git` endpoint fix 985e378b5 (merge
  8e2497cbf): stops are failure-driven and once a window (main's r29: git/git's clone stalled at 183 KB); git-lfs's
  `<remote>.git/info/lfs` reaches the repository named without `.git` (main's r28 lfs row). The many-ref push red
  (daemon_edge) is 2b(a)'s, settled by main (green at 7240125cf), w-shared fixing. NEXT: piece 3 on let_go_read
  (646de255c), peak gauge; deflate reuse.
- 2026-11-22 ~18:30 PST: A-864 readers + piece 3 + bring-back bound + guard lift LANDED 0fefc86e2 (merge 01ae9fc26):
  answers before the pack held for a let-go window and made again once it is back (HTTP, SSH; creation too); windows
  let go behind a fetch's cursor; vcs_store::commit lets the landed pack go again. w-shared's annotated-tags red green
  (1,115,127 B let go). Focused: server 361, git 202. OPEN: git/git mirror stall at 183,511 B (before the pack, with
  the guard in; main's sample pending); push decision reads; LFS object 404; mirror pack 554 vs 218 KB (made_whole).
- 2026-11-22 ~19:10 PST: guard put back LANDED 6692d6f24 (main's ruling): commit lets no landed pack go until the
  synchronous readers read through: (a) green_ready via views (w-shared), (b) push thin bases via IndexedObjects and
  (c) policy/signature reads (mine, ~3 h). Fetch-side readers stay. NEXT: (b)+(c); LFS on main's trace; stall on
  main's sample.
- 2026-11-22 ~22:30 PST: LANDED (merge 77b4cb33a): push reader (b) 17d27ced5 (thin bases in let-go windows stop and are
  brought back; (c) covered by (a)+(b), test kept); codex-review's upload admission fix 28f9ead8d (a retaken loose object
  is not charged again); LFS 482f51fb0 (PUT routed to git's site, not svn's WebDAV; a download admitted the shard's
  room); work-clone quadratic 9bf74aff4 (Kept::runs counted once; kept_fragments O(versions)). Guard stays until main
  says (codex-review's two w-shared readers). NEXT: main's Studio rerun of the lfs row and git/git whole.

- 2026-11-22: A-868 deflate reuse LANDED bdf54c675 (one deflate state a shard thread, reset + set_level, given back
  on drop, MOST_KEPT 8, let go idle; red 72 made for 42 objects -> 1 made 71 reused; server red 10 -> 1/9). Guard lift
  attempted on main's word: 30 server lib tests red (test harness readers read let-go windows directly since 2b(b)
  unpinned audit shards); patch parked ~/.cache/w-git-guard-lift.patch. NEXT (main's order): deepen-by-one bug
  (git_interop.rs:2400), lfs_row test fix (lfs filter config on the clone), then the lift with harness bring-back.

- 2026-11-22: r115 deepen LANDED 80586ec3e (merge 9ca9598c5): the session's repository kept default branch main
  against git/git's master, so the work clone began unborn and its commit was a root; repository now made with the
  source's default branch, red "the work clone checks out the source's tip" (left ""), green HTTP+SSH on a small
  dataset; server deepen guarded at the edge with stock git 2.50's exact request. Clippy checked_div d616218c8;
  lfs_row filter config 4db1a5e4d (merge 8e5ea5489; no git-lfs here, main's r144 runs it). GUARD LIFTED 76d8804c3
  (merge 814aa4bd3): red a_pushed_pack_is_let_go_once_its_push_ends_and_a_clone_after_is_whole (0 bytes with the
  guard); 31 harness reds (sync store calls with no bring-back) fixed by homed_drive bringing back and
  KEEP_LANDED (cfg(test)) for synchronous audit histories; pre-existing on main: daemon t_c1_windows_let_go_survive_
  warm_restarts and svn_commit a_revision_refused_at_publication. Lint split packed.rs:628 50dd194b6.
  NEXT: main's r144 (lfs_row, verbs deepen row, git/git whole) and the lift's round; GAPS owed: SSH edge's link-price
  writes, a price per client.

- 2026-11-23: LIFT REVERTED 5d9e81984 (main's r165: git/git mirror clone after the push refused
  Pack(Store(Unreadable)) at 16,270,345 bytes, deterministic). CAUSE FIXED 0c3f42cea (merge 9ea044028): the fetch
  stream's reads of an object where it lies (read_stored: made-whole entry, encoded afresh, delta check, search read)
  refused a let-go window mid-pack; Upload::read_or_stop stops the turn and retakes the unit. Reds over the ofs fixture,
  every window let go: made-whole (Unreadable at 0 stops before), encoded-afresh (after 5). Guard still in; re-lift is
  main's call after a round. Owed: encoding's Reading::read_window (no red yet); the push-end let-go is one unsliced
  step in ReceivePack's drop (r165 push 25-69% slower with the lift; main to time). SSH link price built, parked
  (~/.cache/w-git-ssh-paced.patch) pending w-svn's yes on the svn.rs EitherCommand forward.

- 2026-11-23: main's items 1-3 LANDED. (1) let-go sliced off the owing step 3e1588a54 (merge 96cf4a0a7). (2) every
  read of a let-go window stops, the stream's delta/search/rebuild reads and the encoding's object read and reuse
  check, under direct-state reds (REUSE_OFF/KEPT_OFF/NO_REUSE switches); slices ended by the quantum alone; R3
  MOST_KEPT gone, REUSE_COUNTED named: 7f7793c58 (merge b846c8ded). Correction told main: 0c3f42cea's message named
  the delta/search stops before they were saved. (3) RE-LIFT faafa5651 + test fixes c6dd9473d (merge 4c8b391ec):
  LANDED_LET_GO on; reds over HTTP (whole + shallow) and SSH (whole + shallow, window-adjusting client), 0 bytes let go
  with the switch off. Full server lib 1236/1238 (content_ranges timing, svn_commit NotHeld: pre-existing).
  NEXT: main's git/git whole + verbs A/B on the lift; SSH link price parked (~/.cache/w-git-ssh-paced.patch) pending
  w-svn's yes on svn.rs EitherCommand::paced.

- 2026-11-23: SSH link price LANDED 78477663c (merge df307829b; svn.rs EitherCommand::paced forward agreed with w-svn);
  reds: late-reading client's writes heard (0 before), the svn wrapper forwards (lost before). Sliced let-go test
  made load-proof 4fc1cea9d (merge 0ffbdb5ef): judged by the shard's own SliceTrace, not the test's wall time; red 1/6
  under 16 spinners before, 6/6 after. Reran on w-shared's A-870 piece 2 (a9c4e7190): 426 green. NEXT: main's r222
  (git/git whole A/B on the lift, then verbs) and the restart read-back row (w-shared votes read-through).

- 2026-11-23: r227 SSH REGRESSION FIXED 56fbf0381 (merge 78ba9b117): with the re-lift, an upload answer held for a
  let-go window its pack choice read (Answer::held_away, sections written) was dropped by the SSH edge's write_answer
  (HTTP makes it again); now kept and made again via read_through_held. Session red: git_interop whole over SSH on a
  made history 3/3 failed before, every step held after. Post-merge rerun 368 green. OWED: unit red at the SSH edge
  (single-shard tests reach no held answer; two-push shallow test kept as guard); SSH wait for room (A-855). Disk:
  freed 2.3 GiB of rcgu.o from my target. Pushed once after a merge before rerunning (rerun green after).

- 2026-11-23: GUARD BACK 45ff23ab3 + SSH wait for room d2dc3fb84 (merge 150d4e123), on main's order after r18 (lift
  on: verbs push-tag-of-a-tag declined by receive::works::tree_of reading a let-go tag; verbs shallow refused
  Pack(Store(Unreadable 2a3c9b98)); HTTP mirror clone stalled 0 bytes, in 4c8b391ec..09797375e, c20133168 cleared).
  Re-lift reds ignored. SSH room wait: red exit 128 -> 0, answered_again counted. NEXT behind the guard, red first:
  tree_of brings back; shallow fetch path (main sends the verbs request); sweep stored-object reads in receive/upload;
  the HTTP stall with the let-go on and a publication owed past its bound.

- 2026-11-23: TAG PEEL FIXED 6eae9f8bb (merge 1d7fd0ec4): Verdicts::prepare answers not ready where the new value's
  peel fails and the repository names a let-go window; the driver brings it back; the same window twice ends typed.
  Red at the session: verbs on the six-commit dataset with the lift on locally failed push-tag-of-a-tag, delete-tags
  and two refs rows; all pass after (118 s). Guard stays in. Unit red owed. Shallow fetch: request for main in
  inbox/main-from-w-git-shallow-lift.md (verbs over git/git with the lift on in a round worktree). HTTP stall: held
  until main's r108 bisect or w-shared's r8 names the commit.

- 2026-11-23: READ SWEEP sent (inbox/main-from-w-git-read-sweep.md); main accepted the order B1, B2, B3-B7, B8. B1 PARKED
  (~/.cache/w-git-b1-wip.patch: Repository::fresh_waiting, readers peek and only bring-back drivers take; red still
  flaky, 2 of 3) behind main's two later orders.
- 2026-11-23: PUSH HOT PATH (main's order, before B2-B8): ShardGreens::holding asks only the greens holding the
  commit, via Repository::holders (git store, head position -> green -> newest version, charged with the chains) and
  Repositories::member_keyed (db, key -> member and join rank; VolumeId::serial = green_key). Red
  t_goal93_a_pushs_search_..._is_flat_in_the_repositorys_greens: examined 60 (4 held) vs 156 (16 held) before, equal
  after. Focused: push 37/37, vcs_store 25/25, db repository 35/35, git store:: (reach skipped) 116/116. Owed (GAPS):
  the lend that only reads (is_owing, branch_version) and Lent::new's empty engine made and dropped each lend (w-svn's
  lend; w-svn's session not listed to tell); the commit's O(greens) head-map clone and footprint walk. Main runs
  git/git's push before and after on the Studio.
- 2026-11-23: UPLOAD STREAMING investigated (inbox/main-from-w-git-upload-streaming.md): the pack streams after
  derive_for_read (waited whole, unbounded), encode_for_fetch (one liveness window) and the wanted set (header count);
  all of it on the owner shard. Proposal P1-P3 to spread across shards (P1 needs a rule on the encoding's learned
  rules). NEXT: git_interop's per-step PATIENCE from the work (needs main's ratio over stock, asked), then B1.

- 2026-11-23 evening: LANDED hot path 68132daeb (r405: push answer 77.0 -> 59.9 s least-of-2, push to listable only -8%
  because the listing waited); PATIENCE by progress 0f40bfea3 (merge 719a296e1); shallow over let-go 050ef43c2 (merge
  bbe4a27e0; r524 26/26 verbs with the lift on). drop(put) lint is main's lint keeper's, not mine.
- ON BRANCH agents/w-git-push-barrier (NOT landed): 07b384f4c barrier waits for moved greens' merge placement;
  bb4bb182b ls-refs waits for no derive/encode (red 4 -> 0 turns). Main's v3 gate (GitHub 45312465577): before 0/5
  (loss), after 0/5 (push 1 disconnect). Cause from per-node counters: merge.replica.owes ~1,173/holder: push 0's create
  is a deferred push (P2), holders' replicas owe it and refuse to judge push 1's version 2 (GAPS A-812(b) OPEN,
  A-813's limit). Uncommitted in w-git-p2: diag (W-GIT-DIAG eprintlns in state.rs count, state_log.rs, receive.rs;
  20 s deadline) + log_uncommitted + PendingGreen::versions. REMOVE the W-GIT-DIAG lines before committing.
- NEXT (main 20:00 ruling): holder makes its owing replica lazily through the takeover's door from objects on the node
  (changeset store + region homes), sliced, counted (build begun, slices, versions waited); barrier progress-aware
  (refused only after a window of no progress, window from the record plane's live estimates; A-884's rule), expiry
  answers report-status `ng <ref> unable to commit`; reds: v3_takeover release+debug and a fleet double-push at f=1;
  record git/git build time at f=1. Read w-shared notes ~/.local/state/main-fanout/progress/w-shared.md 355-383 first.
  Then: (iii) plan/footprint O(G), B1/B2, pack shape vs stock, LANDED_LET_GO decision.
- 2026-11-23 ~20:45: holder replica (1)+(2) built in w-git-p2, uncommitted: vcs_store::object_record (partition or
  ShardState::vcs_replica_records), vcs_store/replica.rs (records_of, begin, adopt; oracle green at 6 cuts + adoption),
  takeover adopts; crate::replica_build (lazy on REPLICA_OWES, Building over replica, counters). LOCAL v3_takeover
  (debug) PASSES: pushes 1,2 acked 31/74 ms, begun 2 made 2 versions_waited 3. NEXT (3): ContentMessage::ReplicaBuilding
  {object, sequence, slices_done} (main approved; golden vector, hostile tests, unknown-kind refusal shown, §4.8
  amendment + amendment log + GAPS + refusal table), owner keeps latest per holder per record, barrier window from
  record plane live estimates with progress = placed/acks/slices_done rising; expiry -> report-status ng "unable to
  commit". Then strip W-GIT-DIAG, f=1 double-push test, release+debug v3 runs (main's gate), land as one piece.
- 2026-11-23 ~21:30: A-886 BUILT on agents/w-git-push-barrier = d72b7324b (main merged): barrier waits for moved
  greens (07b384f4c), ls-refs fix (bb4bb182b), holder replica store + lazy replica build + ReplicaBuilding reply +
  progress-aware barrier + ng on expiry (1b30ea732). Local v3_takeover passes (debug, macOS) and asserts the builds
  (made_ns 14.2/17.3 ms, 877/1,468 slices). Main runs the v3 gate (GitHub, before bbe4a27e0 vs after d72b7324b,
  3 release + 2 debug). On green: merge main, rerun focused, push HEAD:main. Then (iii) Repository::plan head-map clone +
  footprint walk (O(G) a push), then B1 (~/.cache/w-git-b1-wip.patch) / B2, pack shape vs stock, LANDED_LET_GO.
- 2026-11-23 ~22:30: A-886 LANDED b157eacd1 (gate 2: after 4/5, before 0/5; the fifth fails at :438 push 0 in both
  arms, older than A-886). Push-0 disconnect: 4 local debug runs back to back pass (macOS); not ports (pid*7 blocks far
  apart); load-shaped on CI's 2nd debug run. Edge failure logging (connection failed / reply failed / body refused,
  with cause) landing now so gate 3 names it. QUEUE (main's order): push-0 disconnect -> git commit-alone sites
  (commits.rs Before::Unmade, record_and_commit Err committed anyway, make_owed; made_commit::commit_held with w-svn's
  guard-then-apply, A-888) -> speed (interop round a51b627f5 big Studio, stock daemon/http-backend/slates: push
  11.5/13.1/87.8 s, clone 10.9/10.6/149.4 s, partial 6.2/6.6/73.3 s, incremental fetch 4 of 5 runs ~2.2 s; RSS 1.3 GiB
  after push, 2.9 GiB end): clone first, then partial, then push; with pack shape vs stock, (iii), B1/B2.
- 2026-11-24 ~02:10: LANDED: edge failure logging b98233544; made-commit NUL guard 32fc2c982 (dc7980f98); gate-4
  instrumentation d0c12a8e2 (d801b78d2; gate 4 = GitHub 45363070862 running); guarded made commits 22a88984b
  (7925ce564: commit_guarded, Before::Unmade refused typed). IN PROGRESS (uncommitted, ~/.cache/w-git-optionA-wip.patch):
  Option A (main 01:36): bare git submits refused RecordedSubmitRequired (take_turn), test driver made_landing_now
  (MADE_HELD_FOR_TEST), version_saying recorded; ~20 tests fail: recorded submits park on refine jobs (sync tests can't
  run them). Asked main: (a) async harness vs (b) sync refine driver (recommended). Owed records: replay refuses typed
  (main 01:51). Production check: no bare git submit in slates prod; kiln's vendored source not found locally (asked).
  W-GIT-DIAG eprintlns in paths/tests.rs version_saying + commits::held_count: REMOVE before commit.

## 2026-11-24 (slates-work-9a, worktree ~/slates-work/w-git-p2)
- LANDED d609d6e35: git_interop session encoding report (0417fd1f6), per-step daemon counters. Main runs the round.
- LANDED 770b7d200: refining RefineJob reshape alone (7d95662b5); behaviour unchanged; refining::HELD_FOR_TEST +
  refined_now; drift guard refining::tests::t_a787_a_recorded_submits_refinement_by_its_task_and_by_refined_now_agree.
  green_post_state 8/8; w-svn's two let-go reds green.
- LANDED eb60e1c3e: Option A (3265d0a42, A-896): bare git submit refused RecordedSubmitRequired{Git}; make / MadeAfter /
  owed staging+making+resume removed; after_submit = committed_alone bug counter; replay refuses owed record
  OwedCommit{key,version}; over_preview_step answers Whole for a copy (NoTrees gap, A-626 p4). Tests on recorded
  submits (paths::tests::submit_as_taken / recorded_git_submit). 390/390 at merge.
- Reported to main: watchdog exit 4 in t_a238 (git_push/tests.rs:1000) in 1 of 10 local runs (log
  ~/.cache/w-git-run-7.log); HeldShard dead_code warning on main.
- NEXT: gate 4 empty receive-pack-result body (run 45363070862, debug-1.log 251-343 on main's Mac): red on a freshly
  created repository's first push; body must carry a report or typed refusal; edge refuses to complete an empty body,
  counted+logged. Then hash to main for gate 5. Then the clone speed item (derive+encode behind the push).
- LANDED 0741deb0a (0037b3d6c): gate-4 guard: receive reply ending empty (non-probe) is failed, counted
  vcs_http.git.push_ended_empty, logged with bytes read; cut() logs cause. Red/green
  a_push_whose_request_never_arrived_is_failed_never_answered_with_an_empty_body; probe test kept. Main's clippy lines
  on 7d95662b5 fixed (PrepareStepped boxed; drift test split). OPEN: why git's 385 bytes never reached the exchange
  (gate 5 log names bytes read). Hash sent to main for gate 5.
- NEXT: clone speed item (derive+encode behind the push, off fetch's path); clone 149 s vs stock 11 s first.
- LANDED 337bd72c6 (drift test 12/10), 49d801ec6 (daemon:: 3 git tests recorded; sweep in commit), 77397a5f4 (gate 5:
  ReceivePack keeps side_band_asked so a refusal while answered goes on band 3; git_push::Unlanded typed x7, logged;
  red a_push_refused_while_answered_is_still_answered_on_band_three at lent 3).
- WITHDRAWN: the lend-6 'hang' was the 600 s command budget spent recompiling per probe step; built first, lend 6 passes in 4.75 s. Main told.
- WAITING: gate 6 at 77397a5f4 (names the Unlanded cause; HomedShort/repo-at-f+1 -> w-shared's A-894). Main to run
  interop git leg dumping `status --json` git_encode/git_derive counters per step (runs 2-5 fetch 9x, shallow 3x).
- NEXT: clone speed item.
- Clone item prep (04:55): main has w-interop widening git_scale.sh (my counter list before/after each clone, clone-again,
  partial, fetch, shallow; time to first byte; a separate `sample <pid> 30` pass). Counters already exported:
  vcs_store/upload.rs STREAM_COUNTED (git_upload.*), git_deflate.*, git_encode.*, git_resident.*. Reading key: loose_deltas
  >> reused_* = searching; parts_by_entry >> parts_copied = cover entry-by-entry; stops w/o stopped_brought = let-go stall;
  git_encode.turns during fetch = fetch pays encode. Stream: crates/git/src/store/upload.rs (cover 1165, holders 1193,
  reused 1494, search_unit 1849). Await main's rows + sample, then name the step and land red-first.
- 05:14: LANDED ba61d4890 (GAPS: A-891 git half built, held by t_a888_git x2; w-hg removes made_commit::commit, agreed in
  writing) and f7c70b076 (TSan: room_needed sliced a write a step, caller asserts >2 steps). Gate 6 7/7 with no Unlanded;
  gate 7 (10+10 at 3d65116a6) running. WAITING: t_a614a's TSan words (run 45398791868; passes natively 2.7 s), gate 7's
  cause, clone profile (git_scale.sh widened by w-interop).
- 05:30: gate 7 named Unlanded(HomedShort) for push 1's pack, 0.9 ms after the body (fast, not live_budget's ~3 s).
  LANDED ee9571813: homes.rs short-homing instrumentation (w-shared agreed in writing): home_in_region log line with
  branch/elapsed, publish_refused counter+log, stranded log; test a_stranded_tickets_wait_is_answered_not_durable_at_once_and_counted.
  Gate 8 at ee9571813 (main). Parked: retry-until-failover in git_push's Homing (second step if a home is truly late).
  If stranded fires -> F2 bug to w-shared.
- 09:30: gate 9 = (ii) NotAHome; w-shared landed A-902 (d48863ddb; OR clause agreed in writing). Clone item:
  LANDED 2dfda25d4 (Positions word-wise for bit tables: union/intersection/difference/settle/run_pairs/runs; oracle
  r870_bit_tables_combined...) and 0ebf10cf4 (push: reader-named objects admitted without a second SHA-1,
  Push::admit_placed_named, Named::ByReader; test r870_an_object_admitted_under_its_readers_name...). Push's remaining
  hashing is one SHA-1, one BLAKE3 (content identity, needed) and zlib inflate per object, one shard. WAITING: r935 git
  leg at 2dfda25d4+ (first-byte split vs r870's 102-111 s; new clone sample callers; push sample).
- 09:50 queue (main agreed): (a) r935 clone sample -> git_derive's remaining frames (Tree::parse, put_derived blake3);
  (b) clone-again's per-fetch gather (55 s first byte); (c) "intake off the owner" (§4.17.5 piece 5, owed; measure =
  many concurrent pushes to one repo using every core); (d) GAPS: one push over several shards not in the design,
  with r870 80.872 s / r940 / stock 10.797 s, for Ada, once r940 lands.
- 11:45: LANDED 197b0bc4f (kept fragment len read once), a30ffbdb8 (fragments weighed by ContentStore::tree_bytes,
  red a724_a_fragment_kept_as_a_tree...), 4664f8d26 A-906 (pool generation word, layout 20, own 128-B line; cached
  available() keyed (generation, hold_open, Windows readiness moves); tests a906 anchor + a906 pool). r940: push 81.6 ->
  69.0 s. Edge cross-node reach parked (Ada: name model). A-905 = w-shared (intake off owner; my review; Q1/Q2 owed when
  asked: Move can't carry a started service today). NEXT: next git leg's clone sample -> derive's thin spread,
  clone-again gather.
- 17:35: LANDED 675fd5fa2 (let-go test judged per slice; main split it 41e00948e for complexity), 77c0fe495 +
  142172e1f + ed71ac0be (GAPS: push0 named/fixed A-902; git creates measured: non-atomic = a ref a step, owed only for
  atomic; counters landing_step_creates_most / landing_step_ns_most), 427a27d1f (rt spin_kick: INJECT_AT stored on the
  shard + SPIN_NS 100 ms + INJECTED_IN_WINDOW; 20/20 at load 72). Reviewed w-hg's prepared creates (landed fda5613a1).
  Rule learned: delete only my own folders. WAITING: TSan diag 3 (f387b56d3); the full bench's git leg (clone sample,
  landing counters) -> derive's thin spread, clone-again's gather.
- 23:35 (11-24/25): LANDED a4aa0ebc4 A-911 (fetch walk's PositionCache: direct-mapped, sized from one sent commit's
  ids × 8, charged to admission, zero slots = same code; (b) one lookup after id_at; red 2,400 → 383 lookups). Bench
  round 2: clone 113 → 66 s, clone-again 121 → 64, partial 82 → 45, first pack 60-84 → 29-44 s; clone counted 48.7 M
  hits / 11.4 M lookups. 658cc5d04 t_a614a bound = the pace's rule (+ git_derive.turns_yielded). 427a27d1f spin_kick.
  ed71ac0be landing_step_{creates,ns}_most (non-atomic git push = a ref a step). 6d6a285a8 write-path bring-backs let go
  at their end (BroughtBack; derive, green build, replica build); reds: derivation (a64f385b1), green build
  (4fc2892c7). Replica build's red: unreachable while LANDED_LET_GO false; a holder's build reads a let-go window as
  Missing -> GAPS precondition of item 2b's re-lift (0a690a148), red kept at patches/w-git-replica-red.patch here.
  w-perf's re-bring test (b) landed f57e255d7 on my freed git_push/tests.rs. Incremental fetch CLOSED (BENCHMARKS):
  slates' server ~75 ms of 0.95 s; the rest is the client's 1,027-ref work + HTTP helper. NOW: A-911's miss counter
  (vcs_store.git_upload.position_cache_conflicts; red 0 -> green 292 of 383 lookups conflicts), landing; then main's
  A/B: set-associative vs wider window.
- 11-25 ~02:30: LANDED 15b4f1061 (position_cache_conflicts; later documented an upper bound, a8ca56a4f with GAPS'
  intakes_live line), f9090c7d8 = 86b057be4 (walk takes one tree entry a unit: unit_entries_most red 60 -> 1,
  tree_parse_bytes_most counted; frame held/released after t_a752 PackTooLarge; positions: one-sided union settled +
  span settled, seed in positions.proptest-regressions; git_derive.turns only counts real turns), 2dd25afe6
  (vcs_store.git_derive.ns). A/B arms built: B agents/w-git-a911-assoc 11128da63, C agents/w-git-a911-wide 0c8e7d496.
  Answered: git has no hg 57k-push let-go shape (LANDED_LET_GO false; landed_let_go_may waits intakes_live).
  PARKED agents/w-git-derive-behind 0552aca75 (derive a push's versions behind it; red/green done; t_a724 replay diff
  in a reach value open). WAITING: main's A/B verdict, the 57k read of tree_parse_bytes_most/long steps, git_derive.ns.
- 11-25: CLOSED the 11-23 line "git_interop's per-step PATIENCE from the work (needs main's ratio over stock)": superseded by PATIENCE by progress, 0f40bfea3 (merge 719a296e1).
- 11-25 ~06:00: LANDED d9af8b94f A-911 arm B (4-way; A/B: misses -65 %, first pack byte -7.4/-7.5/-5.7 s; C rejected),
  92b74ac50 A-916 (server's dead reach-value copy deleted; oracle derive_settled; design A-647 amended; GAPS line),
  608930048 derive_behind (12.15 s of a 40.3 s first clone's first pack byte was derivation; first fetch now derives
  none), 0cd27990e (look-ahead lets go behind first: TSan diag 8's peak 4 vs 3), 49e4c377a (fetch step phases timed:
  steps, long_steps.{refs,lend,work,settle,count}, phase_ns_most). Learned: arithmetic_side_effects denies bare ops.
  Closed: git_interop PATIENCE line (superseded by 0f40bfea3); incremental fetch (client-side). WAITING: main's next
  git leg's read of the phase counters, then cut the biggest phase.
- 11-25 ~04:30 PST: derive_behind REVERTED 9be9340af (clone mid-derivation regressed 50 %: behind + fetch yielding a
  unit each, 721k turns; TSan listing "waited" = old counter counting behind's turns), then REBUILT and LANDED
  e89adba55: reader-driven hand-off (behind_handed), behind turns a quantum at most, read_turns/behind_turns; reds:
  fetch mid-derivation (446 -> 2 turns vs alone 1), listing during behind (old count 100->117, read_turns 0->0).
  Main's "listing red" was w-hg's pre-revert HEAD (confirmed). WAITING: main's diag 10 + git leg with the phase
  counters (49e4c377a) -> cut the biggest long-step phase.
