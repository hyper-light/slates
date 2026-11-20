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
