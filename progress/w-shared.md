# w-shared -- UI goal 93's shared pieces of §4.17.1 (supervisor "slates-7a")

## 15:55 PST 11-24: LANDED A-908 steps 1-3, main 02bd0ec94: holders replicate a branched green by its lineage
  (membership from held names-log parts, no format change; founding/branching; OUT_OF_ORDER while a join or source is
  missing; whole-lineage adoption). Push red green (7.0 s, red at origin/main). NEXT (main): the successor branch
  refusal, red first with a908_a_branch_is_made_on_a_repositorys_successor_after_its_owner_dies (scratch
  successor-branch-test.rs): unable to commit, then "cannot lock ref". Then part 4 (rebuilds). Red worktree
  ~/slates-work/w-shared-red (origin/main at the time) kept; its target deleted.

## 14:50 PST 11-24: LANDED main 9272195a6: A-907's test fix (28cf8622a: waits for its sender to hold a region window,
  PUSHES_FOR_A_HOME 8; the flake was a home of neither window 1 run in 9) and A-908 part 1 (fdc59391e: holder replicas
  kept as crate::greens::Greens, each alone; lend/lend_mut at holder sites so the owner's P2 gate keeps its meaning).
  Gate 12 recorded in GAPS. NEXT: A-908 part 2 (version 0's record names membership, format 5; holder founds and
  branches lineages, OUT_OF_ORDER while a source is behind; whole-lineage adoption; branch rebuilds; format-4 records
  refused on restart leave replicas rebuilt, with a test). Red kept in scratch branch-red.diff.

## 13:10 PST 11-24: LANDED A-907, main a551b0a88 (113c937b6): every broadcast's stragglers end on their links (one
  settle for homes' exchange, the healer and credentials; Stragglers' Drop and ReplyTx::send hand sessions to an
  installed orphan hook), HOME_STREAM served on its own task. Gate 10's cause (A-902's slow home outlasting the healer's
  round, the dropped straggler's link lent for good). Red/green fleet a907_. OWED (main): gate 12. FOUND (GAPS OPEN): a
  second push creating a new branch refused `unable to commit` in GitFleet (new green unplaced, inputs_unplaced 4).
  PARKED: A-905 (red in scratch a905-red.diff, design a905-design.md; w-git's Q1 open; part 1 = Move carries a started
  exchange).

## 12:30 PST 11-24: A-904 recorded for Ada (GAPS 929baef80; red kept in scratch a904-red.diff). ACTIVE: A-905, intake
  off the owner (claimed). Seam agreed with w-git (A1 per-queue owner reads, A2 budgets, A3 Decoding where read).
  Red in worktree w-shared-a894 (uncommitted; scratch a905-red.diff): daemon_edge::a905_..., read_bytes [0, 149152].
  Design draft scratch a905-design.md, with w-git for review: two whole-connection moves (to intake_shard on head, to
  owner with its ReceivePack after the read). NEXT: w-git's review, then the split.

## 11:55 PST 11-24: LANDED the lint-only commit d9cabf2cc (my five sites at d48863ddb: RegionHold, caught_up while-let,
  serve_remote / exchange / heal_one split by meaning) and the must_use sweep cd4e2bf04 (poll_until,
  poll_for_periods, poll_for_live_periods #[must_use]; 14 sites: nine asserted, five helpers commented as settles;
  8 of 8 tests green in 116 s, no finding). Edge-to-owner routing: owed per D-14 (w-git's reading), w-git's after
  the clone work, awaiting main's ruling on (a) forward vs (b) redirect. Nothing active; waiting for main's next piece.

## 11:20 PST 11-24: LANDED A-903, main 29f45e606: a successor writes its adopted state logs' parts at the adopted
  epoch (was its host epoch; 19 StaleEpoch refusals a run, no push to a taken-over repository committed). Red/green in
  A-894's takeover test (push to the successor). OPEN for main and w-git: a repository is reachable only at its
  owner's edge (Repositories::locate is node-local): forward at the edge, or send the client to the owner. NEXT:
  main's ruling on that; then the poll #[must_use] sweep.

## 10:45 PST 11-24: LANDED A-894, main 87a0b1b8c: a repository's creation answers once its record is placed at f + 1
  (63.8 ms at f = 1, debug), held on the destroy's hold made general (live_ship::defer_until_placed). Red: git_fleet's
  immediate check 0 of 2 holders. FOUND (GAPS OPEN): after a repository's takeover, a push is refused at both survivors
  ("not found" at the non-successor's edge, "unable to commit" at the successor). NEXT (scheduled by main): the poll
  #[must_use] sweep. Gate 10 (A-902) result pending from main.

## 09:55 PST 11-24: LANDED A-902, main d48863ddb: a home behind its sender's configuration waits for its own install
  within the put's budget (gate 9: NotAHome 0.83-1.89 ms into 2 s, 3 of 40). Home wire 0x84; agreed in writing with
  w-git. Red in-process (a) refused at once, (b)/(d) no wait; green 2.25 s. OWED (main): gate 10 at d48863ddb, 0 of 40.
  NEXT: A-894 (claimed; a repository's creation answers once its record is at f+1), then the poll #[must_use] sweep.

## 06:25 PST 11-24: LANDED the Windows fleet hang fix, main 866374d85: a_captured_clone_moved_to_another_node_is_found_there_by_its_name
  now snapshots its clone, so the carry is made and asserted (red with the assertion alone after 4,027 periods; green
  4.29 s, was 407 s on the Mac and killed at 240 s on Windows). OWED, for main to schedule, as a separate piece after A-894:
  make poll_until, poll_for_periods and poll_for_live_periods #[must_use]. Every site the compiler then names (34 calls
  in fleet.rs begin a statement) is either asserted or shown to be a deliberate settle. A site that goes red is a
  finding to report, not to quiet. ACTIVE: gate 8's homing short. w-git logged each non-Kept answer and each host left
  unanswered (7d6874639); I'll write the one-wait proposal from gate 9's lines. Reading the code makes (i) unlikely:
  session_returned waits for a session to be installed, up to the budget. A-894 (claimed) follows.

## 14:23 PST: goal status reviewed (inbox close note). OPEN: the lane's proof (320/512 MiB 10/10). Branch
  agents/w-shared-slice 0deb19aea, worktree ~/slates-work/w-shared-slice: (1) no re-encode/re-hash at the owner
  (ec245f32e), (2) the fetched increment moved to the owner shard a chunk a call (0deb19aea). kiln r2: holder tests +
  A/B 256/320/512 x5 vs 1a35ab091. Next cuts: the verb's gather+decode, append's publish, place (each measured), then
  datagram batching on the content plane, then the 10 x 5 proof. BLOCKED: A-658 3c (A-767 holders' changesets).

## 14:00 PST: LANDED A-826 (3b) main 156007dff, family three c5051eb51. ACTIVE: owner-side slicing of the holder route
  (performance; near misses 454-883 ms on the control shard at 512 MiB). First cut agents/w-shared-slice ec245f32e: a
  holder's submit not re-encoded/re-hashed at the owner (SealedWork.held). kiln r1 (new numbering): holder tests + A/B
  256/320/512 x5. The 512 MiB exit 4 is the LOCAL submit (w-work-cow's slicing; told). takeover_round: main's (GAPS).

## ACTIVE 2026-11-21 10:07 PST: lane 3b on agents/w-shared-3b (269f12e3f off 8f73b6928), worktree ~/slates-work/w-shared-3b.
  kiln r24 red+green; r23 A/B of A-825 (895b80994 vs 545cf46c5) queued. Map in ~/slates-work/w-shared-scratch/3b-map.md.

## LANDED 2026-11-21 09:40 PST: lane (A) piece 1 = A-825 chain entry format 6, main 545cf46c5 (tested 000d0e442, kiln r19).
  3a bench (r13): 320 MiB 9/10, 512 MiB 0/10, liveness open. NEXT: 3b content ahead by identity, then slicing, then
  the bench. Family three (agents/w-shared-sweep-3) still waits for a Linux runner (adacachy refuses runs).

## ACTIVE 2026-11-21 00:57 PST: lane (A) by reference, piece 1 (chain entry format 6), approved by main 00:58. Worktree
  ~/slates-work/w-shared-byref, branch agents/w-shared-byref off origin/main 544020bd2. Design: the increment is held in
  the shard's one content hold under the green's object by its inputs identity, before the verdict (typed refusal at the
  shard's room), the hold published before the entry appends (the db entry is durable at append, the hold only at a
  publication), released when its entry folds into a checkpoint and no entry above names it, and at recovery any owned
  green's held content no entry names goes. Push writers (w-git, w-hg, w-svn) move in this piece; told the hour before
  landing. Red first: a real-process kill -9 of the owner with an unfolded version (crates/cli/tests/cli.rs). Kiln r15
  (3a release bench) and r14 (family 3 Linux) still queued.

## LANDED 2026-11-20: sweep family 2 (stragglers woken by their reports) = 54b9d8b2b (main ab30bfb75). Reports to main now
  go as inbox/main-from-w-shared-<topic>.md on this branch (main moved to the Studio; SendMessage no longer reaches it).
  Family 3 (fuse helper exit via ExitWatch) on agents/w-shared-sweep-3 215556146, worktree ~/slates-work/w-shared-sweep-3;
  its Linux test is kiln run r5 on adacachy. NEXT after it: exit 4 at 64 MiB; (1); (4) incl. reply polling; (2); f; A-658 3c.

## LANDED 2026-11-20: sweep family 1 (lent record sessions woken by their return) = c03a8953b (main 86e23c4ef; focused
  tests local on 8ebf8a1c6 per Ada: fleet 25/25, lib 84/84, fmt). Local target deleted. Rules now: Ada says focused
  tests run locally (no blocking on CI); main: build nothing here under 30 GiB free, delete target after each landing;
  MEMORY SIZING (briefs/common.md) binds the lane/by-reference work: lend, don't copy, nothing allocated per shipment.
  NEXT: family 2 (owner_location broadcast stragglers), family 3 (fuse mount-helper exit); exit 4 at 64 MiB; (1) B holds
  its inputs; (4) lane throughput incl. exchanges' poll_interval reply polling; (2) content-plane-vs-lane A/B; f; A-658 3c.

## STOPPED 2026-11-20 after landing 1fc1da1fb (main 2eca76a9b), per CLAUDE.md §8 (supervisor starts the next piece).
  Reported to slates-72 (slates-7a is gone; slates-72 presumed the relaunched main). Worktrees: ~/slates-work/w-shared-lent-wake
  (fresh off origin/main 359da2388, no edits; for the lent-session wake piece), ~/slates-work/w-shared-state (my own
  fanout-state worktree: the shared ~/.local/state/slates-fanout index holds a staged reversal of my ledger lines that I did
  not make, so I push the ledger from here). Old branch archived as agents/g261-a636p2b2-archive; its worktree removed.
  Lent-session wake design (not started): per-host waiters on the control shard (xshard::awaited), answered from
  fleet::return_sessions and every record_sessions insert/remove site (fleet.rs 7642/7877/7907/7938/8268, member_task.rs
  321, live_ship.rs 4469); the four waits (forward_lane SessionOut, fleet campaign 8920, owner_location 476, live_ship 4090)
  wait on it within their own deadlines.

## LANDED 2026-11-20: forwarded completion + phase timers = 1fc1da1fb (main 2eca76a9b; tests on 306214c57, key tests
  re-run on 2eca76a9b). Release baseline (fresh mode) in BENCHMARKS. Bench branch agents/w-shared-a697-bench on origin
  (Studio clone ~/.cache/w-shared-a697-bench-src over https; target ~/.cache/w-shared-a697-bench-target).
  NEXT (main's order): sibling waits (lent sessions x4, broadcast stragglers, FUSE helper exit) each with its own wake;
  explain exit 4 at 64 MiB; (1) B holds its composed inputs, A's record to B record-only; lane throughput (~200 MB/s);
  (2) content plane vs lane A/B before by-reference; then f (flip + hook deleted together), then A-658 3c.

## LANDED 2026-11-20: A-697 part e2 = c0710afca (main e261100c1; tests on base f2421b5f3, fleet a_holder 23/23 re-run on
  e261100c1). Target now ~/.cache/w-shared-local-target.noindex with CARGO_INCREMENTAL=0. Removed landed worktrees
  g261-a497/a515/a534/a575/a603-land/a635-land/a636/git-mercurial-subversion-a275; kept g261-a603, g261-a635,
  g261-a636-red (commits not on main). NEXT: by-reference increment route (main's ruling (b)): release A/B on the big
  Studio via kiln run BEFORE building (4 MiB + 64 MiB, holder + local, best of N all shown, phase breakdown), then build,
  then measure again; fix record-plane input shipping if slow for its own reason; BENCHMARKS tables. Then part f.

## LANDED 2026-11-20: A-697 node-verb authority = 43160e726 (main b0ececd09). NEXT (main's order): (2) increment too large
  for the forward lane in bounded parts; (3) dropped creation's floor released at recovery; (4) f.

## LANDED 2026-11-20: A-697 rebase+copies = 4f61920f0 (main b279ba52c). Main's new order (2026-11-20): (1) MoveWorkFloor
  authority = the fleet link's authenticated member id equals the work id's creator member; red: another member refused;
  (2) increment too large for the forward lane carried in bounded parts; (3) a creation dropped at restart releases its
  floor at the owner at recovery; then (4) f: routing flip + hook deleted in the same change, no dual path.

## LANDED 2026-11-20: A-697 part e = 76b5dd46a (main 285ab0923): holder work recovery over the replica. NEXT: part f (takeover
  + routing flip: delete the test hook Daemon::serve_works_on_holders/WorkHome::on_holders, replace green_elsewhere's hook
  with the real "replicated here" rule, two-node f=1 e2e with taken-by-position counter; takeover of A -> successor
  answers holder works' asks). Owed: holder rebase + copies (Unsupported), dropped pending creation's floor at owner,
  increment too large for the forward lane.

## LANDED 2026-11-20: A-697 part d = f253a8017 (main 04a69de57): hold_until_replica/replica_reached (control shard waits,
  woken by merge_service::recompute; bound = parked verb); fixed part b test's counter race. NEXT: part e (recovery:
  holder works after a restart -- unbuilt works whose green is elsewhere replayed via the replica / owner admission;
  pending submits/creations resent over the fleet), then f (takeover + routing flip: delete the hook, real "replicated
  here" rule, two-node e2e with taken-by-position counter). Holder rebase + copies still Unsupported (owed).

## LANDED 2026-11-20: A-697 part c = b8db5ba37 (main c729cdea5): SubmitComputed/ComputedSubmit; work_home::verdict_over_fleet,
  computed_request; verbs::submit_computed; Green::submit_computed. Holder rebase + copies refused Unsupported (owed).
  NEXT: part d (settle once the replica holds the version) -- ask main what (d) means precisely if unclear; then e, f.

## LANDED 2026-11-20: A-697 part b = 26454e7de (main b93699f93): MoveWorkFloor/WorkFloorMoved, verbs::move_work_floor (monotone,
  forwards_as_read), work_home::tell_floor_over_fleet, route_gated refuses it from clients. NEXT: part c (commit by
  position: holder computes increment + verdict on its replica, owner takes commit by position carrying increment +
  computed form; judge_computed + commit_judged, A-658 3b). Gate refuses Verdict/Rebase for elsewhere works today.

## LANDED 2026-11-20: A-697 part a2 = fd679ecd4 (main 57ea648f2): hook Daemon::serve_works_on_holders / WorkHome::on_holders;
  work_home::green_elsewhere, admit_over_fleet; verbs::base_engine (replica reads on control shard); verdict/rebase
  refused Unsupported (part c), floor moves counted owed FLOOR_OWED_HOLDER (part b). NEXT: part b (floors at the owner:
  tell_floor for green_elsewhere -> forwarded floor move/drop to A, e.g. an appended verb or AdmitWork-like WorkFloor
  verb; owner keeps history at floors). Worktree ~/slates-work/g261-a636p2b2.

## LANDED 2026-11-20: A-697 part a1 = b856352cb (main 0831c5a0d; ledger c8e444a18): AdmitWork/WorkAdmitted, verbs::admit_work
  (forwarded-only, owner_admit), Daemon::work_floor, fleet test a_holders_work_is_admitted_... NEXT: part a2 (holder B
  serves CreateWork locally behind a TEST-ONLY daemon hook; mints id on B's partition; asks AdmitWork over the forward;
  base reads from state.merge.replicas; test: id names B, files identical, edit reads back, no-rights refused by A, same
  name AlreadyExists; amend A-247's rule). Worktree ~/slates-work/g261-a636p2b2.

## A-697 (a) SPLIT: (a1) owner admission across nodes: append RequestBody::AdmitWork { green, work } (forwards_as_write;
  served only from serve_forward, refused in a local dispatch) -> verbs::owner_admit on G's owner shard (rights from its
  record, WorkFloorSet, head) -> ReplyBody::WorkAdmitted { id, base }; exactly-once by run_forwarded's completion key.
  Fleet test (tests/fleet.rs two_node_fleet + green_and_work_on shares): client on B asks AdmitWork -> forwarded to A ->
  admitted at head, floor recorded on A; resend same id same answer; a principal with no share refused.
  (a2) B serves CreateWork locally behind a test hook: holder replica is state.merge.replicas (control shard), not
  state.greens; work_home Need::Admit asked over the forward; base reads from the replica.

## A-697 (a) PLAN (reading done 2026-11-20; nothing written yet):
  - Today: work_home::gate parks a CreateWork whose green's owner shard is elsewhere ON THIS NODE (create_need -> park ->
    xshard ask). Across nodes the whole CreateWork is forwarded (forward_lane.rs over fleet::FORWARD_STREAM) and served
    on the owner (A-247 "A creation forwarded from another node is served on the green's owner").
  - New (behind a TEST-ONLY daemon hook like Daemon::inject_merge_fault, daemon.rs:3043): on B holding G's replica
    (state.greens has G), CreateWork is served on B: mint the work id on B's client partition; forward an owner ask to A
    (new appended RequestBody::AdmitWork { green, work, name } -> ReplyBody::WorkAdmitted { head }, served only as a
    forward); A checks the principal's rights on G from its record (reuse create_work's admission), records
    Op::WorkFloorSet { green, work, base: head } (idempotent on resend), answers head; B then records the work on its
    partition with base = head, files read from B's replica at head (wait if the replica is behind).
  - Test in crates/server/tests/fleet.rs with two_node_fleet() (L20623): client on B, hook on: work id names B; G's files
    byte-identical; edit reads back; a no-rights principal refused by A; same name AlreadyExists. Amend A-247 (design
    ~L22640 + §4.16 status ~L11701) + amendment log.

## NOW: A-697 piece (a) (briefs/g305.md; progress/g305-merge-anywhere.md L377+): owner admits a holder's work by a forwarded
  verb (rights from owner's record, base held); holder's replica answers base reads; work made on client's partition;
  TEST-ONLY hook gated like inject_merge_fault; two-node f=1 test (client on B): work id names B, G's files identical,
  edit reads back, no-rights principal refused by A, same name AlreadyExists; amend A-247 rule (design + amendment log).

## LANDED 2026-11-20: svn copy-from rows 8ac39d0c0 (main 3e1fba44c). Queue item 3 next: A-697 (a)-(f) then A-658 3c
  (briefs/g305.md; progress/g305-merge-anywhere.md).

## LANDED 2026-11-20: typed copy refusals 9ddd1fbe9 (main c2de447a8). NOW: svn_made copy-from rows (plan above: pass the
  increment's Copy ops; dest rows Add/Replace with copy_from; beneath dest compared to source at version; add_parents with
  copies). Then A-697 (a)-(f), A-658 3c.

## NOW step 4 (main's call): merge verdict gets its own refused outcome (not a conflict); submit answers
  Refusal::CopyRefused { case } with case a closed enum (Root, PastHead, SourceEmpty, OutsideNodeChanged, VersionFolded,
  LinkAcrossCopy ...); append both to ipc wire + publish; SDKs/MCP exhaustive match over cases; A-586/GAPS classify cases
  (FSFS also refuses: root, past head, empty source; retention: folded; owed: outside node changed, links across copy).
  THEN svn copy rows (not started; plan in notes above).

## LANDED 2026-11-20: hard-link copy 7fe4dc0e2 (main 95044a373). NOW: step 4 - every remaining "does not yet copy" case its
  own typed refusal, never TypeChanged (needs a conflict class or a typed submit refusal: ask main the shape). Then svn copy
  rows, A-697, A-658 3c.

## NOW: hard-link copy (inbox note to w-engine pushed 8afb8e625). DESIGN: crates/merge/src/engine/copy.rs: copy_of_source
  refuses `self.hardlinks.holds_tree(&spec.source)` (TypeChanged) ~L913 -> stop refusing; at commit of Effect::CopyTree
  (find where CopyTree applies; dims graft of files etc.), for each (L, P) in hardlinks under source at version: L' = dst +
  rel(L); P inside source -> hardlinks[L'] = P' and links_to[P'] += L'; P outside -> files[L'] = files[P] value (by
  reference) + P's metadata (metadata_path) to L'. Identity: explicit keys move the sum as ordinary changes; check
  head_identity_from_scratch, replica replay (replica_reads_alike), checkpoint restore (restored_reads). copy_fix's
  "hard link stood in the tree at it" stays refused (owed, own typed refusal). Step 4 (typed refusals, never TypeChanged)
  likely needs a new MergeConflictClass (wire u8 class; ipc/SDK docs) - decide with main after the build.
  Failing tests: merge copy_tests (inside link remapped, outside link own file) + server paths::tests repro.

## LANDED 2026-11-20: copy surfaces b6f6426ac (main 807a6f3c0). NOW: engine copy of hard links (main's 4 steps; inbox note to
  w-engine on files first). Python venv at ~/slates-work/w-shared-scratch/venv (maturin develop); Node addon = cdylib copied
  to scratch slates.node; daemon = ~/.cache/w-shared-local-target/debug/slates (cargo build -p slates-cli).

## QUEUE (main 2026-11-20): copy surfaces (MCP done uncommitted: decode+exhaustive work_op_kind+by-use test; CLI copy verb
  added uncommitted, needs by-use test; then Python SDK, Node SDK) -> engine copy of hard links (amend A-586/§4.16: names
  sharing a node inside the copy share one new node; a node with a name outside gets its own node, bytes by identity;
  failing test in paths::tests + slates-merge copy tests; build in engine/copy.rs incl. checkpoint/replay v4; every
  remaining "does not yet copy" a typed refusal, never TypeChanged; inbox note to w-engine on files touched first)
  -> svn copy rows -> A-697 -> A-658 3c.

## NOW (main reorder): WorkOp::Copy on every surface first (CLI, Python SDK, Node SDK, MCP), each proven by use vs a real
  daemon (work copies, submits, reads copied tree); make WorkOp matches exhaustive where a surface lists/decodes them.
  Then svn_made copy-from rows (design noted above), A-697, A-658 3c.

## LANDED 2026-11-20: A-626 part 4 work verb 7fed9d428 (main 9e8e489d6). NOW: svn_made copy-from rows (pass the increment's
  Copy ops (path=dest, src=source, at=version, REPLACE flag) from judge_submission; rows Add/Replace with copy_from; paths
  beneath a dest compared to the source at its revision). Then A-697, A-658 3c.

## LANDED 2026-11-20: successor's sliced rebuild dae3f979e (main af3551739). NOW: A-626 part 4: WorkOp::Copy (client verb) over
  VolumeOp::Copy (merge increment.rs:184) / LoggedOp::Copy; then svn_made copy-from rows; A-697; A-658 3c.

## NOW (main): slice make_whole (successor's rebuild, takeover::rebuild_repository) via vcs_store::rebuild; then A-626 part 4
  (WorkOp::Copy over VolumeOp::Copy/LoggedOp::Copy), svn copy rows, A-697, A-658 3c. Pack-record turn -> w-git; flaky fetch -> w-perf.

## LANDED 2026-11-20: git/svn sliced rebuild adb33875d (main 7efceb67b). NOW: A-626 part 4 (the WorkOp that makes a copy;
  read GAPS' A-626 entry), then svn_made copy-from rows, then A-697, A-658 3c.

## QUEUE (main 2026-11-20): git/svn sliced rebuild (NOW) -> A-626 part 4 myself (the WorkOp that makes a copy; engine
  CopyTree parts 1-3 on main; read GAPS' A-626 entry) -> svn_made copy-from rows -> A-697 (a)-(f) -> A-658 3c.

## NOW (main 2026-11-20): git/svn sliced rebuild, sibling of w-hg's A-732 part 3 (main 0f756ec77, VcsStores::rebuilding):
  vcs_store::rebuild/make_whole for git and svn as a task replaying a record a turn; failing test first (a rebuild of a
  many-object store on a running daemon advances a step a turn). Then A-697, A-658 3c. Reported homes test flaky (w-balance).

## LANDED 2026-11-20: svn symlinks 015f15b5f (main f771ede60). svn copies BLOCKED: no WorkOp copy (A-626 part 4) - told main.
  NOW: queue item 5, A-697 (a)-(f) then A-658 3c (briefs/g305.md, progress/g305-merge-anywhere.md).

## LANDED 2026-11-20: parked-wait bound 968c90093 (main 92bc438bf). (a) complete. NOW: svn_made copies (copy-from rows), then
  symlinks (svn:special). Then A-697 (a)-(f), A-658 3c (briefs/g305.md, progress/g305-merge-anywhere.md). Target dir was wiped
  clean for disk (90 GB free then).

## LANDED 2026-11-20: flaky fix 1d4aeb63d (da4dc2ff0); push storm 6ad42c7a4 (c0071e402). NOW: parked submit's wait bounded by
  the derived failover delay (state.failover_ns, as base_wait), typed refusal past it. Then svn copies, symlinks; A-697; A-658 3c.

## LANDED 2026-11-20: (a) part 2 owed commit = 311e4a235 (main 602983873). NOW: fix flaky t_a664_live_a_pushs_whole_map_is_built_
  in_slices (slice count depends on step budget timing; make settle's budget 0 -> deterministic), 20 loaded runs. Then push
  storm, parked-wait bound, svn copies, symlinks.

## MAIN ORDER 2026-11-20: 1 (a) part 2 owed commit; 2 push storm: one build per green, a mid-build push hands its change to
  the running build (or moves to newest head once at a slice boundary), never restarts; RESTARTED counter asserted 0 under a
  50-push storm test; 3 parked submit's wait bounded by the derived failover delay, typed refusal past it. Then svn copies,
  symlinks; A-697; A-658 3c.

## LANDED 2026-11-20: (a) push half = cd302e110 (main 777022f2a). NOW: (a) part 2, the owed commit (main's 3 requirements:
  replay reads no clock + determinism test crash-after-staging recover-twice same ids; completion settles owed in the
  same transaction; one owed per green, parked, typed refusal at bound). Then svn copies, symlinks. Then A-697, A-658 3c.

## (a) PUSH HALF DESIGN (in progress, worktree ~/slates-work/g261-a636p2b2, nothing committed yet for it)
- New child module crates/server/src/vcs_store/paths/whole_build.rs (child of paths: uses Plan::Put, made_holding,
  give_back, git_file, map_path, put_file, count, BUILT). Builds(BTreeMap<green, Build>) in VcsStores.whole_builds.
  Build{repository, version, paths: Vec<String> engine paths (whole_head's names), at, entries, held, parked, task}.
- push.rs:749 calls paths::advance_after_push: whole plan -> whole_build::start (spawn task: loop with_state_counted(slice)
  + yield_now, like git_encode::settle_encodings; TaskId kept, detached; cancel on abandon); else step inline.
- slice: take the build out of the map; head moved -> restart at new head; put files (lend per file, settle_in at end)
  until slates_rt::futures::step_budget_ns spent (>=1); done -> made_holding(Plan::Put) keep head, count BUILT, resume parked.
- Park: owner_judge (remote work path) before db.begin; local submit() before take_sealed (park the verb's args, set
  acceptance_deferred). Parked enum {Owner{sealed,evidence,policy}, Local{principal,work,evidence,signature,authorship,
  approval}} + origin,id, route Option<ReplyRoute>. Bound: clients_per_shard x region.slots (base_wait's), refuse Overloaded.
  Resume: owner_judge / a local transaction (begin, current_request, reply_route, submit, record_completion unless deferred,
  commit), deliver via xshard::hand_reply_home like merge_service resolve (~3437).
- vcs_store::free -> whole_build::abandon(repository): give back held, cancel task, resume parked.

## LANDED 2026-11-20: svn:author = cb1cc1782 (main 707d513ab). hg half -> w-hg (main's ruling; seam note sent). Main APPROVED (a)
  design: owed commit staged durably in the submit's transaction (green, version, message, identity, second); completion
  entry settles it in the same transaction as the made commit; replay never reads the clock (determinism test: crash after
  staging, recover twice, same commit ids); one owed per green, later submits parked, typed refusal at the bound. PUSH HALF
  FIRST. NOW: (a) push half. Then svn copies, symlinks. Disk: cleared incremental, removed w-shared-mode-land (49 GB free).
  Landing helper: python3 ~/slates-work/w-shared-scratch/land.py <commit> (merge+resolve docs+push loop).

## QUEUE (main 2026-11-20): item 4 hg half (asked main: now or after w-hg's rework) -> svn_made's three: svn:author (as the
  svn:// commit path maps the principal), copies (copy-from rows), symlinks (svn:special) -> (a) live whole builds -> A-697, A-658 3c.
  NOW: svn:author.

## LANDED 2026-11-20: A-664 Subversion objects (bug) = 55cf568ae, main ..196ba2e15. Answered w-balance's Q1/Q4 (inbox).
  NOW: item 4's Mercurial half (a submitted version -> server-made hg changeset). Then main's (a) live whole builds; then
  A-697 (a)-(f), A-658 3c.

## MAIN RULING 2026-11-20: A-664 live whole builds = (a): defer the made commit until the sliced build ends; later submits to
  that green PARKED (woken by the build's completion, never polled); each slice yields under the step budget. Do after item 4.
## NOW: item 4 svn first: failing test t_a664_svn_a_submitted_version_is_the_revision_the_server_makes (svn_commit/tests.rs)
  RED (submit -> version 1, records youngest 0). Fix: a server-made svn revision from the submit's change, landed in the
  submit's transaction without moving the green. Uncommitted in ~/slates-work/g261-a636p2b2.

## LANDED 2026-11-20: A-664 sliced whole builds part 1 (recovery's remake a file a unit) = ba7c04d7b, main 469436cce. Asked
  main for a ruling on the live whole builds (the made commit needs trees in the version's transaction). NOW: queue 4, A-664
  hg and svn server-made objects, unless main rules otherwise.

## LANDED 2026-11-20: A-664 holders' maps = 107297a15, main f0b23d651 (+ docs 22347ac53: 2c-2 objects owed -> content held
  once, main's ruling). Ledger/run request pushed, reported. NOW: queue 3, A-664 sliced whole builds (whole map builds and
  whole map release sliced a unit at a time). Then (4) hg and svn server-made objects, (5) A-697 (a)-(f), A-658 3c.

## LANDED 2026-11-20: A-636 part 2c-2 = c8452ee97, main 7379a4d8e..c7c18db3e (ledger + run request on agents/fanout-state
  f168847; reported). NOW: queue item 2, A-664 holders' maps (briefs/g308.md; progress/g308-server-commits.md). Worktree
  ~/slates-work/g261-a636p2b2 reusable (merge origin/main into it).

## LANDED 2026-11-20 (kiln w-shared, slates-work-8c): A-636 part 2c-1 = 878fc386c, main 183dd0e23..82d3538b0. Ledger + run
  request done, reported. NOW: A-636 part 2c-2 in ~/slates-work/g261-a636p2b2 (branch agents/g261-a636p2b2 = main 82d3538b0).

## KILN w-shared (session slates-work-8c) 2026-11-20: took over (slates-work-91 already ENDED). Merged origin/main ea05b2ad6
  into ~/slates-work/g261-a636p2b2 (merge 80dba3209; design-log conflict kept both, main first, 2c-1 last). Running the
  2c-1 filter + fmt (out ~/slates-work/w-shared-scratch/a636p2c1-run2.out). NEXT: green -> push HEAD:main, ledger, report.

## RESUME HERE (handoff 2026-11-20, local w-shared ENDED for the kiln instance)
- LANDED: A-636 part 2b-2 = commit 4c38e425e, on main via eea7e6497 (amendments.tsv row and run-requests updated).
- A-636 part 2c-1 is BUILT, COMMITTED, PUSHED but NOT ON MAIN: branch agents/w-shared-a636p2c1 at 1c91eaac2 = change
  878fc386c plus a merge of origin/main 003936b3e; the only conflict was the amendment log's end, resolved main first,
  2c-1 last, no markers. Local worktree ~/slates-work/g261-a636p2b2 (its local branch agents/g261-a636p2b2 is the same
  commit).
  Green BEFORE the merge (macOS arm64, this Mac, local, own target ~/.cache/w-shared-local-target): `cargo test --locked
  -p slates-server --lib -- fleet::state_log_tests takeover:: state_log mirror:: repository::tests --test-threads=4`
  33/33, the new tests named; fmt clean. The merge brought server code 2c-1 compiles against (state.rs, daemon.rs,
  vcs_http, git_push), so NEXT: re-run that same filter plus `cargo fmt -p slates-server -- --check` on the branch;
  if green, `git fetch && git merge origin/main`, check `git diff --stat origin/main HEAD` and conflict markers, then
  `git push origin HEAD:main`; then add 2c-1 to amendments.tsv A-636 col 4 and run-requests.md; report to slates-7a.
- What 2c-1 is: the repository record as its own register, class byte REPOSITORY_CLASS = 6 (server catalog.rs
  repository_value, repository_of, repository_sequence = policy.version + names position + 1);
  fleet::owed_repositories; takeover::adopt_own puts it in ShardState::pending_repositories (counter
  fleet.takeover.repository_adopted). Docs are in: design status lines, the 2c-1 amendment entry, GAPS.
- THEN 2c-2 (told main): the successor's rebuild. Once a repository's record and all four state logs are adopted and
  its greens materialized, keep each state log's records only where every log they span holds them whole (part 1's
  keep_whole rule in crates/db/src/ledger/logs.rs; a green's log judged by its adopted head). Then record the
  repository whole in the successor's db (a new Op) and make its store (vcs_store::make_from_log). The holder's
  parts are in holder_records acceptors per state-log register. Then queue items 2-5 (briefs/w-shared.md).
- Read-only findings already sent to main: before 2b-2, the repository id and the root id were minted at the same
  counter (same bytes); and nothing shipped the repository record (2c-1 fixes that).

## NOW: A-636 part 2c split (told main): 2c-1 = repository record as own register (REPOSITORY_CLASS 6, seq =
  policy.version + names pos + 1, fleet::owed_repositories, takeover adopt_own -> ShardState::pending_repositories);
  2c-2 = successor rebuild by part 1's rule. 2c-1 BUILT uncommitted in ~/slates-work/g261-a636p2b2 (on main eea7e6497),
  docs written; run: server fleet::state_log_tests takeover:: state_log mirror:: repository::tests (pending).

## LANDED 2026-11-20: A-636 part 2b-2, commit 4c38e425e, main 3fee2f575..eea7e6497 (merge eea7e6497). Worktree
  ~/slates-work/g261-a636p2b2 (keep for 2c's base or remove). NOW: A-636 part 2c (successor adopts a repository's logs and
  records whole). Note for 2c: nothing ships a repository's RECORD (name/tool/format/owner/policy) to holders yet.

## NOW 2026-11-20: queue item 1, A-636 part 2b-2 (route by id, family ids). Worktree ~/slates-work/g261-a636p2b2 (branch
  agents/g261-a636p2b2, ff'd to 9f51d7525, change uncommitted). Main AGREED placement follows the family (A-333). Built: db
  register family ids + line_key, Partition::family_counter (snapshot), Db::family_number_at, in_family join rule; server
  verbs::fresh_family_id_at (repo, root, branch, push create, lone green); design status + amendment entry, GAPS. Target
  ~/.cache/w-shared-local-target (own). Runs: db lib register:: repository:: snapshot 124/124. Next: db --test wire_order,
  model; server lib repository:: tests; fmt; commit (msg ~/slates-work/w-shared-scratch/a636p2b2-msg.txt); merge; push;
  amendments.tsv A-636 row; then 2c. 9f51d7525 re-run (own target) 10/10 green, reported.

Queue (briefs/w-shared.md): 1. A-664 part 3's commit (g308) + follow-ons; 2. A-636 part 2b-2 then 2c (g261);
3. A-697 (a)-(f) then A-658 3c (g305). Studio d1323898, sync false, CARGO_TARGET_DIR ~/.cache/g308-target, JOBS=2.

## LANDED 2026-11-20 ~05:00Z: branch's first map from its source's, main 9f51d7525. Follow-ons left: holders' maps (BLOCKED:
  holders hold no vcs store; needs A-636's fleet half), copy through a work (BLOCKED: no client copy verb), sliced whole
  builds/releases, hg and svn objects. Then queue 2 (A-636 2b-2/2c), 3 (A-697, A-658 3c). Worktree ~/slates-work/w-shared-mode-land.

## LANDED 2026-11-20 ~04:45Z: mode fix main 73cbdfc42. NOW: piece 1 follow-ons (holders' maps first; g308 brief).

## LANDED 2026-11-20 ~04:30Z: piece 1 A-664 part 3's commit, main 6dfb8fc0c (c6f7976c3 + merge). NOW: piece 1b, the mode fix
  (chmod+x then mv; test ~/slates-work/g308-scratch/mode-test-for-next-piece.rs). Studio clone ~/.cache/w-shared-clone, target ~/.cache/g308-target.

## RELAUNCH (2026-11-20 ~04:20Z, fresh w-shared): p3k had died (signal 15 at 04:06Z, the session exit). Rerun as kiln r2
  (same command, SKIP_RED=1 studio-p3j.sh, logs ~/.cache/g308-logs-p3j on the Studio). origin/main still b63f8a05d.

## EARLIER (2026-11-19 ~20:05 PST): piece 1, A-664 part 3's commit
- Branch agents/g308-a664p3c at 09a38d73d (merge of main b63f8a05d = origin/main at start). p3j run had died at clone.
- Studio run p3k = SKIP_RED=1 ~/.cache/g308-scripts/studio-p3j.sh agents/g308-a664p3c (logs ~/.cache/g308-logs-p3j),
  local output ~/slates-work/g308-scratch/run-p3k.out.
- NEXT on green: fresh worktree off origin/main, `git merge --squash agents/g308-a664p3c`, runs bullet, one commit, push.
  Then piece: mode fix (test ~/slates-work/g308-scratch/mode-test-for-next-piece.rs).

## 2026-11-20 ~16:55 PST: CLOSE LIST order (Ada). Closed: inputs at B (f26544b65), sleep-and-look 1 (c03a8953b), 2 (54b9d8b2b);
  inbox/main-from-w-shared-close.md. Working now: A-697 f (completion), in ~/slates-work/w-shared-a697f.
  Lane note for item 4 (local, load 74, NOT a reported number; base 5d29b9572, agents/w-shared-lane 3421049f1): path MTU
  reached 9209 (the 1.1 KB-datagram hypothesis is refuted); 64 MiB stops: window ~6800, pacer ~600, budget ~7340, credit 0,
  goodput 34-60 MB/s; 4 MiB 18-476 MB/s. The congestion window bounds the lane on loopback: look at loss and cwnd
  collapse next, on a quiet machine.

## LANDED 2026-11-20 ~18:05 PST: A-697 part f = a9e789e54 (main 56f33c6f6): the hook deleted, the rule "replicated here",
  the holding list fanned, and the takeover test. A-697 complete. Close report appended. NEXT (completion): A-658 3c, with
  its scope question to main (inbox/main-from-w-shared-a658-3c-scope.md; building reading (A) unless told otherwise).
  Then robustness: family three (kiln r6 queued), exit 4 (kiln r9 queued). Then performance: lane (congestion window),
  A/B.

## 2026-11-20 ~18:45 PST: leads recorded (no code yet; order is completion -> robustness -> performance)
- A-658 3c: reading (A) moves no work out of git's decision step (judge and append share one step), so it would land
  code with no effect. Asked main to choose: record it as blocked on the repository store's replication to holders,
  or build (B). inbox/main-from-w-shared-a658-3c-scope.md. Worktree ~/slates-work/w-shared-a658-3c is clean
  (no changes).
- Exit 4 / record plane: cut_shipments rebuilds the version's inputs bytes (inputs_of) and the whole inputs archive
  (chunk + BLAKE3) on every shipment pass, and each pass carries one archive slice, so the cost is size squared, all on
  the owner shard. Suspect for exit 4 at 64 MiB; kiln r9 (macOS, 64/128/256/320 MiB) will name the spinning shard.
- Lane (perf): UDP sockets keep the OS default SO_RCVBUF. macOS net.inet.udp.recvspace = 786,896 B (~85 datagrams at
  the 9,209 B path MTU; net.inet.udp.maxdgram 9216); this Mac shows 11,411,409 datagrams "dropped due to full socket
  buffers" since boot (all processes). Hypothesis: a congestion window up to 560 KB overruns the receive buffer, the
  drops read as loss, and the window collapses (64 MiB stops: window ~6800). Test it with netstat -s -p udp deltas
  around the bulk row, then size the buffer from the work (the path's bandwidth-delay product, up to
  kern.ipc.maxsockbuf), on a quiet machine.

## 2026-11-20 ~18:55 PST: main's ruling on A-658 3c: (A) not built; (B) stays on my list, sequenced after w-store's
  A-767 server wiring lands. GAPS line landed (main 548e99440). NOW: robustness, family three (kiln r6) and exit 4
  (kiln r9). Then 3c (B) once the store wiring is on main, then the lane and the A/B.

## 2026-11-20 ~19:20 PST: family three widened and hardened before landing (agents/w-shared-sweep-3 731331eef, merged
  with main): the unmount helper's exit (`reap`) is watched too; a refused watch is a typed refusal
  (MountError::ExitUnwatched; unmount counted refused), with no tick fallback (banned second path). bridge-fuse suites
  green locally except a pre-existing flaky handshake count (1 of 4 on a loaded Mac; GAPS line added). Linux green+red =
  kiln r11. Exit 4 = kiln r9 (queued).

## 2026-11-20 ~21:35 PST: kiln runs held, both still queued (not started)
- r11: family three, Linux green + red (agents/w-shared-sweep-3 731331eef); needs linux/amd64, tool:cargo, cpus=8,
  mem=16g, disk=40g. kiln: "queued; not linux 8 runners, machine under strain 1 (adacachy 451397fc...), no cargo 1".
- r9: exit 4 reproduction (64/128/256/320 MiB holder submits, release); needs macos/arm64, cpus=8, mem=32g,
  disk=60g. kiln: "queued; link down 3, haven't answered 3, not macos 3, under strain 1 (the big Studio d1323898...)".
NOW (not idle): exit 4 in code. cut_shipments rebuilds the version's inputs and their whole archive on every shipment
  pass (size squared, one owner step each); building the fix with an RSS-bound test.

## LANDED 2026-11-20 ~22:50 PST: record plane, inputs archived once = 8f04c9130 (main c990989f7). Exit 4 open until kiln r9
  (queued) reports. Family three: kiln r11 started on adacachy (green running). Next: land sweep-3 on r11 green; read r9.

## LANDED 2026-11-20 ~23:55 PST: lane step one = b72aff1fc (frames encoded in place, packets sealed in one reused
  buffer; Studio-1 profile before/after in BENCHMARKS: memmove 604->491, median 1,066.5->1,103 MB/s). Lane is ~1.1 GB/s
  quiet. NEXT: step two (frames by reference into the send stream until acked; receiver copies). Family three: kiln r15
  (Linux, queued, r11 lost to a runner restart). Exit 4: kiln r9 (queued).

## LANDED 2026-11-21 ~00:40 PST: lane step two = 35c740150 (main 585b89299): spans of the sender's bytes, no copy
  into frames. memmove 604 -> 491 -> 433 over the two steps; goodput ~1.1 GB/s quiet, level. NEXT in lane: receiver
  copies (decode, reassembly), then datagram batching per syscall. Family three: kiln r15 (Linux, queued). Exit 4: r9.

## LANDED 2026-11-20 ~23:20 PST (machine clock): lane step three = c3db76bcd (receiver copies). Lane ~1.31 GB/s median
  quiet (from ~1.07). Asked main: batch datagrams per syscall next, or close the lane item. Kiln: r4 family three on
  adacachy (running), r3 exit 4 (queued for this MacBook).

## 2026-11-20 ~23:50 PST: EXIT 4 LOCALIZED (kiln r3, main 90e9b72): 128/256/320 MiB exit 4, shard 1 spinning ~1.0-1.1 s
  CPU in the task at identities.rs:243 (read_then_route: a session's first submit runs whole inline in one poll). 64 MiB:
  exit 101 with 1,652 ms near miss. Record-plane fix did not cover it. Next: sample a 128 MiB submit, slice the step.
  inbox/main-from-w-shared-exit4.md. Family three: kiln r4 still running on adacachy.

## LANDED 2026-11-21 ~00:35 PST: exit 4 = c592a81f0 (main 5ab1c23b5): deriver's piece list blocked (√ops), runs merged.
  Bench 64-320 MiB all pass on Studio-1. CLOSED (note in close file). Lane: in-place forward ~105-150 MB/s; asked main.
  Family three: kiln r4 still on adacachy.

## 2026-11-21 ~01:05 PST: lane in place: 320 MiB holder submit refused Overloaded 1 of 2 runs (forward ~3.1 s; transfer
  ~2.2 s ~150 MB/s vs 1.3 GB/s raw). Reported; next: time the forward's phases on both ends. Family three: r4 running.

## 2026-11-21 ~01:20 PST: family three's Linux run is kiln r14 (queued: adacachy, the one Linux runner, at load 60-93;
  r4 stopped: its 30 min test timeout covered a starved compile). r14 builds first, then times the tests; green then red.
  Waiting on: r14; main's answers on the lane (in-place forward refusals at 320 MiB, next step proposed).

## 2026-11-21 ~03:00 PST: LANE (main's ruling 23:50): step 1 landed 47b87f28d (main 948711996). 3a on branch
  agents/w-shared-lane-live (worktree ~/slates-work/w-shared-lane-live): task_to_its_end + acked request bytes as
  progress; red test a_holders_submit_still_crossing_is_not_refused_by_a_size_independent_bound flaky because the
  OWNER's step is 4-7 s in one control-shard step (debug, 32 MiB) -> lane gives up silent. NEXT: measure the owner's
  step by part (decode / judge / append / record checksum), slice or by-reference the biggest; land 3a with it.
  Family three: kiln r14 (queued, adacachy). Plan + update: inbox/main-from-w-shared-lane-plan.md.

## LANDED 2026-11-21 ~03:40 PST: lane 3a = 71e55b6c2 (main ed3fbc616). NEXT: by-reference increment (owner's durable
  step small): design note inbox/main-from-w-shared-lane-byref.md first, then build; red = 32 MiB holder submit 10/10.

## 2026-11-21 ~04:05 PST: WAITING on main: owner's durable step, shape (A) by-reference chain entry vs (B) sliced db
  append (inbox/main-from-w-shared-lane-byref.md; both reach crates/db or the chain format, overlapping A-767/w-store).
  Family three: kiln r14 queued (adacachy not answering). A-658 3c: waits on w-store wiring.

## 2026-11-24: A-909 landed (f387b56d3, commit c9262f763)

- An owner's own hold fences each object as its holders do (`Acceptor::accept_own`, called by
  `commit_record_sent_as`). The successor's branch push was refused because its one owner acceptor, fenced at the
  takeover's epoch 2 by registers it adopted, refused its new green's records at host epoch 1 (`StaleEpoch`).
- Tests: the fleet test is red without the fix and green with it; 4 slates-db a909 tests; focused register 109,
  cluster commit 26, server lib takeover and state_log 39, fleet a894, a908 and the new branch's push 3.
- Models: they fence per register, and the specs are unchanged.
- Owed (main): gate 15 (v3_takeover) and a lint diag.
- Left: A-908 part 4 (a holder's rebuild makes a member by its lineage); A-905 (parked on w-git's question 1);
  the second new branch's `cannot lock ref` is unchecked.

## 2026-11-24: TSan diag 3's failure and A-908 part 4 landed (b7e65cc63)

- af061de87: daemon::tests::refused_merge_authority_leaves_the_replica_unchanged used ObjectId([20; 16]), a family id.
  Since A-908 steps 1-3 its version 0 waits for a join (member_unknown 1, out_of_order 1). It is red natively at main
  and at A-909's parent 142172e1f, so it comes from neither A-909 nor TSan. The test now uses a non-family id. Sibling
  family-bit ids pass natively (10 of 10).
- 7106f83ea: A-908 part 4. A restarted holder rebuilds a root (founding its lineage) and a branch (branched from its
  source, a missing ancestor rebuilt a level per ask) by membership; red first with 2 merge_origin tests.
- Left: A-905 (parked on w-git's question 1). A fleet test that restarts a holder warm between a branch's versions was
  not run (main's to ask for).

## 2026-11-24: A-908 part 4's fleet test landed (2b6b43071, commit 7e04a16c3)

- a908_a_holder_restarted_warm_between_a_branchs_versions_serves_the_branch_after runs in the anchored git fleet
  (anchored_git_fleet, GitFleet::restart_warm, push_on_top_of). Green 4 of 4. Red in a scratch copy with part 4's
  membership step off: unable to commit, b's recompute_mismatch 1.
- Found: a c stopped instead of refusing a's records left a push refused `retry` 3 of 3. c's takeover moved the
  configuration from 7 to 13 within the push, and A-902's install wait on the restarted b elapsed. This is the window of
  a push during a takeover, reported to main.
- Left: A-905 (parked on w-git's question 1).
