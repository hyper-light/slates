# w-balance progress (Mac Studio Adas-Mac-Studio-2.local)

## 2026-11-20 status
- State folder ~/fanout-state-w-balance (push URL set to SSH). slates clone ~/slates-work/w-balance (done).
- Read: CLAUDE.md; design D-11, D-28, §4.8 (role, data model, lookup, placement), §4.10 (content replication,
  ownership follows the writer), §4.11 cost model, §4.17/§4.17.1 (objects, push intake A-719, residency, A-714,
  A-662, learned tunables), A-636 2b-2 (family placement), A-704, A-713; w-perf's progress (A-723, A-718).
- (resolved) commits: kiln-git-sign refuses this session; Ada OK'd -c commit.gpgsign=false.
- OLD: commits fail. commit.gpgsign=true is injected on the command line; kiln-git-sign refuses this session.
  A-724 row is appended to amendments.tsv locally, NOT pushed (so not yet mine). Not overriding signing.
- Disk: 18 GB free on this Studio (< 20 GB guard). No builds until main says so.
- Outline below = first deliverable. No code before main's go.

## Design outline (A-724, pending claim): one server, many repositories, balanced by learned usage
1. Unit of balance: the repository (its family, A-636 2b-2) and, inside it, the principal (agent or client).
   Each owner shard keeps a per-repository ledger, shard-thread-local (no atomics or locks on the data path):
   bytes hot / warm / cold, CPU-ns by phase (intake, verdict, pack build, index merge), push/fetch counts and bytes,
   concurrency. Per principal: a bounded heavy-hitters sketch (Space-Saving, Metwally ICDT'05), k derived from the
   ledger's memory share. Never unbounded maps (§2 ban 8).
2. Learned demand: decayed rates with the decay derived from each repository's own measured inter-arrival times;
   working-set size as a miss-ratio curve by sampled reuse distance (SHARDS, Waldspurger FAST'15), sample rate derived
   from the ledger's budget. First values come from the first request's own size and the measured rate (§4.17.1 rule).
3. Shares, by stated formulas over measured inputs, logged with inputs (D-11):
   - memory: split the shard's admittable room by marginal utility on each repository's miss-ratio curve (utility
     cache partitioning, Qureshi MICRO'06; convex hull, Talus HPCA'15), floor = the A-188 share (room / repositories
     served now), so small repos always fit;
   - CPU: weighted fair queuing of step quanta (§4.3) among repositories with runnable work; multi-resource by
     dominant-resource fairness (Ghodsi NSDI'11), weights = learned demand capped at measured use;
   - work-conserving: unused share is lent; a borrower's room is reclaimed first from its own cold tier.
4. Tiers and the cold trigger: hot = heads' trees, active works, live intakes; warm = recent history inside the
   repository's memory share (its MRC decides how much); cold = past the share. Levers in cost order, each the
   §4.11 cost model with value_of_byte(pressure) taken from the repository's share overrun: drop derived indexes and
   caches (pack fragments, reach sets, A-718 deltas; rebuild cost x predicted access vs bytes), compress cold content,
   then hold cold content only on peers (f+1, fetched by identity). RAM only (R1): no disk tier.
5. Admission and backpressure: every intake/fetch is admitted against its repository's share, else unclaimed room.
   Fast path (R9): a request within the floor share passes on one counter check, no model. Past the share: pause the
   reader (TCP window, ring credit) while the projected drain time is within the client's deadline; past that, a typed
   refusal `ShareExceeded{repository, asked, share, inputs}`, retryable, mapped into each tool's words (git's
   maxInputSize/ERR, hg pusherr, svn E-code). Never OOM; a hot repo never takes another's floor.
6. Giant repositories (Google/Meta size, several at once): ownership never moves for load (§4.10). The owner keeps
   only the register (refs, ordered landing), the index tail and fences, and its hot set. Spread instead:
   - intake work (decode, hash, compress, check) runs on the connection's ingress shard; only the landing runs on the
     owner shard (A-719 rule 4), so one hot repo uses every core;
   - content homed by its identity: rendezvous over content id across shards (N=1) and across region members (fleet),
     f+1 holders, one rule (R8). Index tables are content (A-581), so cold runs live on peers too.
   - Conflict to rule on: §4.10 today places content at the owner's neighbourhood candidates; homing by identity is
     §4.17.1's owed "content placed by identity in a fleet". Needs main's ruling before piece 6.
7. No global catalog (ban 12): content routes by id, repo state at its owner, shares published to sibling shards
   as an epoch-published immutable snapshot (D-8) at the learning cadence, never per request.
8. Laptop = fleet: same code, N=1 node, f=0; one repository gets the whole room.
9. Proof: extend push_load.py (agents/w-perf-pushload): M repositories, Zipf sizes and Zipf activity, toy to giant;
   per-class p50/p99, RSS, refusals per repo; no starvation = small-repo p99 within a derived bound of its solo run.

## Pieces (after go), each one commit to main
P1 per-repository/principal accounting + `slates status` rows; P2 share model, pure module with oracle test
(serial reference allocator); P3 admission by share + backpressure + typed refusals; P4 cold trigger (drop derived,
compress); P5 intake off the owner shard / content by identity across shards; P6 fleet placement by identity
(after ruling); P7 harness and measurements (ask main before heavy load).
Depends on: w-perf A-723 (one copy per byte), w-git A-719/A-722 (sliced intake). I own balancing, not copies.

## Piece log
- 2026-11-20 Ada said go (main's go not seen; Ada's overrides). P1 LANDED main 8c1feae15 (merge 6090a4502), checked
  with merge-base. Files: crates/vcs/src/usage.rs (new), server state.rs/daemon.rs (ShardState::vcs_usage),
  git_push.rs (charge_step/charge_answer, answer_turn), vcs_store/upload.rs (lend_on counts), vcs_store.rs (free
  forgets); docs §4.17.5 + A-724 log entry + GAPS line. Target ~/.cache/w-balance-target. Clone ~/slates-work/w-balance.
- NEXT: P2 share model (pure module in slates-vcs: decayed rates, SHARDS MRC, utility split + floor, DRF weights) with
  a serial reference-allocator oracle.
- P2 division LANDED main 4bbf7f4ba (merge abc2722e0), merge-base checked: crates/vcs/src/share.rs (divide, Claim,
  Granted). share tests 4/4 (oracle 500). Found+fixed: zero-room unused resource stopped claims at level 0.
- NEXT (P2b+P3): server side. Per shard: claims from vcs_usage rows + held bytes (objects_charged, store) -> divide
  over [admittable room, step-ns per period]; admission by share in git_push::push_room / upload room; typed
  ShareExceeded in git's words; floor fast path counted.
- P3a LANDED main 08b440739 (merge-base checked; pushed as f2a3a04fa after re-running focused tests on the merged
  tree, since the merge touched git_push.rs). crate::vcs_share (room, Live, Bound, Room; counters vcs_share.fast /
  divided / narrowed). Seam proposal to w-git: inbox/w-git-from-w-balance.md (A-722 checks vcs_share::room, carries
  Bound into its typed refusal; share words). Found (pre-existing on main b0655e8a3, reported, not chased):
  vcs_ssh::tests::hg::stock_7_2s_push_over_ssh_lands_through_the_daemons_lenders_as_7_2_answered_it panics "a step
  that waits".
- Gotcha: a second worktree built into my target dir left stale slates-vcs artifacts (E0432 on new exports); touching
  the crate's files fixed it. Prefer not to share the target across worktrees.
- NEXT: P3b (once w-git answers or A-722 lands): share words per tool; hg/svn intakes Live; backpressure. Meanwhile P4
  cold trigger design.
- Main's input (w-perf): snapshot slot 746,962,928 B fixed at boot; ~41 B/object after A-727. Designed P8 (snapshot
  names sealed index tables by identity; slot derived per snapshot, grown by re-laid segment at handoff, typed
  SnapshotRoom refusal) and P9 (index cut into 2^b ranges by object id, rendezvous over (family, range), b in the
  record, split by doubling, push rows committed by owner's landing record). LANDED docs 3471c18a2. Note: config.rs
  snapshot_bytes = 2 x table_bytes is the current boot derivation.
- Queue: P3b (after w-git), P4 cold trigger, P5, P6 (ruling), P7, P8, P9.
- Main's rulings: P6 approved (owner keeps register/landing/tails/fences/hot set; cold content homed by identity,
  f+1, fetched by id, durable before naming record; amend §4.10 same change; agree replication with w-fed-move,
  w-fed-faults, w-shared via main FIRST). Seam approved and sent to w-git; bound+words in admission.rs/report.rs
  after A-722 lands.
- P4a LANDED fbb5d5f8b; P4b LANDED 87edaacb6 (merge d6673b719). Questions for P6 in inbox/goal91-w-shared-from-w-balance.md.
- NEXT: P5 (intake work off the owner shard; content homed by identity across a node's shards, N=1).
- Cleared ~/.cache/w-balance-target/debug/incremental (7.6 GB) per main.
- P5a LANDED 129cd0d1b: vcs_store::homes (home_of, keep_here/read_here/let_go_here, async home/read/let_go),
  db register rendezvous_weight_of + rendezvous_ranked_by_key (outputs unchanged, register:: 93/93).
- Main's answers: Q2 yes (move moves register + hot set; cold stays at identity homes); Q4 agreed (no coordination).
  Q1 (w-shared), Q3 (w-fed-faults): no reply in inbox yet.
- NEXT: P5b = cold lever 3 (move a repository's cold windows to homes; stores read through) needs a read-through seam
  in ContentStore/git store (crates w-perf/w-git own) -> propose seam; intake decode on ingress shard waits on A-722.
  Meanwhile P7 harness (push_load.py many repos) can proceed.
- dcb6a0406 LANDED: §4.10 amended (content homed by identity), from worktree ~/slates-work/w-balance-land (clean
  origin/main) since local w-balance main holds wip bench commit 1e84630b1 (branch agents/w-balance-load; must be
  re-landed via fresh worktree with real message).
- P7: harness crates/cli/bench/multi_repo_load.py on agents/w-balance-load; release built on .local-2 at
  ~/.cache/w-balance-target/release/slates (src ~/.cache/w-balance/src). run1: alone 0.228/0.705/3.174 s for
  v0.99/v1.0.0/v1.5.0 holding 70/183/777 MiB; v2.0.0 push KILLS the daemon (run-requests). run2 (3 classes) r7 running.
- w-perf seam proposal sent via agents_send (id 01a1f722b428761c9ae0225a84cc3b41) + inbox/w-perf-from-w-balance.md;
  w-perf replies in inbox/w-perf-w-balance-homes.md.
- P7 LANDED 489398c9a (harness + BENCHMARKS row + design log line), from worktree ~/slates-work/w-balance-land.
  run3 (.local-2, release 1e84630b1): R=1/10/100 all ok; R=100 small p99 14.1x alone, medium 6.8x, hot 11.3x;
  R=1000 estimate 202 GB > 30 GB free. Logs on .local-2 ~/.cache/w-balance/run{1,2,3}.log.
- RULE FOR ME NOW: ~/slates-work/w-balance's local main holds wip commits 1e84630b1 + 7614bfe (bench, landed by content
  in 489398c9a). NEVER push from that clone's HEAD. Work only in ~/slates-work/w-balance-land (worktree, detached,
  fast-forward it from origin/main before each piece).
- NEXT: P3c CPU share: each shard's step quanta scheduled fairly by repository (WFQ / DRF CPU dimension), proven by
  rerunning multi_repo_load R=100 (target: small class p99 near alone). Then P5b/A-727 seam with w-perf, P6 region
  half (Q3), P8, P9.
- 9e9cfbd58 LANDED (from worktree ~/slates-work/w-balance-land2): homes ticket/published; share tolerance from
  Higham gamma_k (w-lint item done); P3c fair turns rejected (BENCHMARKS "Fair turns by repository on a shard"):
  branch agents/w-balance-cpu fdfcf2421 holds the rejected code. run4 log on .local-2 ~/.cache/w-balance/run4.log.
- w-perf agreed seam (kiln message): ticket+published first (DONE, told it), batch forms after its a3.
- Worktrees now: ~/slates-work/w-balance (local main has wip commits: never push from it), ~/slates-work/w-balance-land2
  (detached at 9e9cfbd58; fast-forward from origin/main before each piece).
- NEXT: profile the R=100 step to find why toys wait 14x (sample the daemon on .local-2 during the burst: `sample`
  per shard, or a slates status counter dump), then fix by evidence. Then batch forms (after a3), P6 region half
  (Q3), P8, P9.
- run5 (main b8e4591, NO gate) at R=100: daemon GONE too (hot 19 pushes reset/empty), toys 16.5x -> regression on
  main between 129cd0d1b and b8e4591 (run request filed). Corrected BENCHMARKS (landed). Profile saved on .local-2
  ~/.cache/w-balance/r100.sample (8 s sample during burst); kiln server stopped answering before I could read it.
- NEXT: read r100.sample when kiln answers (heaviest shard threads); find toys' wait cause.
- r100.sample read: shards' samples ~101k of 133k at ShardContext::run+2576 (the runtime loop itself, not git work);
  git unpack/sha1/fsck only a sliver. So the toys' 14-16x wait is not shard CPU. Candidates to measure next (not guess):
  (1) the harness client (one Python asyncio thread sending ~350 MB; split the client into processes, or time-stamp
  first-byte-sent vs answer), (2) the HTTP edge accept/body path, (3) arena growth (RSS 87 MiB -> 3.5 GB in seconds).
  Also: main b8e4591 ends the daemon at R=100 (run request; bisect owed).
- 04376a543 LANDED: moved A-724's piece bullets (3c,4a,4b,5a,5,6,7,8-9) out of A-727/A-728/A-726/A-719p3 into A-724's entry (main's docs fix). RULE: append future A-724 log bullets at the END OF A-724'S ENTRY (before the next ### heading), never at the file's end.
- STARVATION INVESTIGATION (2026-11-20, .local-2, release from main + harness 463fc501c):
  * split timing: toys send in ~0.1 s, wait 4.4 s for the answer (server side).
  * lanes per repository per shard (branch agents/w-balance-lanes 1b8010400): DEAD EVEN (toy wall p99 4.42/4.57 vs
    4.55) -> rejected, not landed.
  * 2 classes (no v1.5.0): daemon survives (crash is v1.5.0-specific); toys 11.6x; hot repo's 19 pushes serialized
    (19 x 0.55 s = 10.4 s).
  * ALL-SMALL control R=100 (v0.99 only): toys 12.6x / 10.7-11.8x -> not caused by a hot repo.
  * name placement owner_of_name (FNV-1a mod partitions): R=10 one repo per partition, R=100 max 6 of 23 -> fine.
  * sample top-of-stack: kevent 25,807 (zero-timeout spin polls, NOT idle), sha1 ~3,100, blake3 ~840; ps -M during
    burst: 22 shard threads at 35-99% CPU -> daemon SATURATED: 86 x 0.28 s / 22 shards = 1.1 s ~ p50 0.99 s.
  * CONCLUSION: "p99 / alone" is the wrong starvation metric under saturation; the concentrating load is a hot repo's
    whole intake on its one owner shard (P5b, intake off owner, overlaps w-git's A-722/A-719 p4). Floor fast path /
    admission order would not change these runs (no refusals, memory never bound).
  * MY EARLIER CLAIMS CORRECTED: 'shards mostly idle' (r100.sample) was wrong (kevent = spin polls).
- Worktree now ~/slates-work/w-balance-land3 at 463fc501c (harness split + counters, not on main yet; real messages).
- 9be789460 LANDED: harness fair-share ideal (one shard / spread), split timing, counters; BENCHMARKS 'held to their
  fair share': under today's placement every class 0.72-0.87x of fair share (no starvation); hot repo 7.61x spread.
- 3ae8ca99d LANDED: vcs_share::place (intake_shard two choices, Decoding guard per shard, decoding_on); ReceivePack holds
  Decoding. Told w-git in inbox/w-git-from-w-balance.md. allow(dead_code) on intake_shard/decoding_on until A-722 calls.
- MY RULE BREAK (reported to main): `git push -f` to MY OWN scratch branch agents/w-balance-load3 after a rebase
  (463fc501c -> bc2b042e0); no one else's work lost. Use NEW branch names from now on, never -f.
- NEXT: P6 N=1 parts, P8 (snapshot slot derived), P9 (index across partitions).
- 1729a9c5a LANDED P6 node half: homes home_many/read_many/let_go_many (one call a home shard a turn, quantum-bounded).
- 096413d1c LANDED P8a: Db projects snapshot (floor + since + record) vs slot before append; crossing -> exact snapshot;
  can't fit -> DbError::SnapshotRoom {needed, room} (mutate: nothing applied; commit: rollback with cause); snapshot()
  refuses before publish; Db::snapshot_room(); wire DatabaseRefusal::SnapshotRoom (wire_order 12); server maps.
- NEXT: P8b slot derived at each snapshot + re-laid segment at handoff (anchor geometry: check owners first),
  vcs_share::room reading snapshot_room as a 2nd resource, P8c (snapshot names sealed index tables, with A-727),
  P9 (index cut across partitions by object id).
- 8c seam proposed to w-perf (inbox/w-perf-w-balance-homes.md + agents_send): record v3 names object rows as one content
  by identity (put_object), w-perf owns record.rs; offered names.rs helper. Waiting reply.
- 61a74fe35 LANDED P9a: slates_vcs::cut (range_of, bits_for, split_due, child_of) + vcs_share::place::range_partition.
- NEXT: 9b needs 8c; meanwhile snapshot_room as 2nd resource in vcs_share::room (DRF over memory + snapshot room).
- b759bddb3 LANDED: vcs_share::room DRF over [memory, snapshot slot room]; Bound::Snapshot; tests 23/23, edges 88/88 (the hg ssh test now passes on main).
- 0011a4606 LANDED 8c codec slates_vcs::rows. BLOCKER for record.rs v3 switch: A-727 a3 (content durable in image,
  recovered before records); today HeapWindows rebuilt FROM records -> v3 would lose pushes on restart. Told w-perf.
- Q3 answered (w-fed-faults, inbox line 37): identity-homed content must NOT use ContentHold (object-scoped releases
  follow owner epochs: takeover release_certified, retain_object late copies, tombstone forget_object, ledger
  compaction). Keep it keyed by content id; healer reads membership + rank only.
- NEXT: P6 fleet half per Q3; then 9b.
- 58f756e0a LANDED P6 region rule: slates_db::identity_homes (homes, heal) by membership+rank only.
- P6 transport PROPOSED to goal-91 + w-shared (inbox goal91-...md, bottom): HomeContent/HomeRead/HomeLetGo request kinds
  keyed by content id, authority = configuration's homes(), ack after home image; healer per config change. Waiting.
- NEXT: 9b (repository record's b; range rows routing) while waiting; 8c v3 switch after A-727 a3.
- Main ruled: build P6 transport myself. 599294892 slates_cluster::homes (HomeMessage Put/Read/LetGo/Answer,
  HOME_STREAM=30, admits). c77324051 serve_remote + fleet.rs dispatch arm; acks only after published() (Unanswered
  until A-727 a3 calls image_begun/image_published).
- NEXT: P6 sender + healer loop (6c-3); 9b; record v3 after a3.
