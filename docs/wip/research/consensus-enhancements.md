# Consensus enhancements for the configuration groups (§4.8 mechanism 2)

Status: design, 2026-09-28. Nothing here is built yet; the build ledger at the end records each slice as
it lands. Ada's goal (2026-09-28): pre-vote, node-priority elections, parallel vote replication and
processing, learner nodes, multi-log synchronization (MLRaft), leader transfer, and Fast Raft — each
implemented to the project's standard and proven by full simulation (latency emulation, loss, partitions,
crashes) and on the KIND cluster, every decision backed by measured data.

## 1. Where consensus sits in slates, and what it is measured by

- **What the groups hold.** One Raft group per region (the council) holds membership, neighbourhoods,
  host epochs, takeover assignments and volume homes; one root group across regions holds region
  membership and cross-region promotions (§4.8 mechanism 2). The dialect is `crates/cluster/src/raft.rs`
  (sans-io: election, replication, PreVote §9.6, CheckQuorum §6.2, ReadIndex §6.4, joint consensus §6,
  log-integrated membership, snapshots §7).
- **The commit rate is near zero outside failures** and is a tripwire (D-14): the data plane never
  commits through these groups. So the user-visible metrics are **latencies on the failure path**, not
  throughput:
  - *takeover latency*: from a host's death to its objects served by the successor. It contains a
    council commit (the reconcile that retires the host and assigns its objects);
  - *election latency*: from a leader's loss to a new leader able to commit;
  - *unavailability*: the window in which no commit can complete;
  - *messages per commit*, since the root group crosses regions.
- **The three deployments every result must hold for** (R8; Ada 2026-09-28): a laptop (`f = 0`, one
  voter), one cluster (sub-millisecond RTT), and multi-region (tens to hundreds of milliseconds RTT,
  loss and jitter). Each mechanism below is measured on all three profiles.

Consequence for the design: a mechanism whose only gain is throughput (ParallelRaft, MLRaft) must still be
built to the standard, but its measured gain on the failure path is what decides whether it is on by
default, and the record says so in numbers.

## 2. Sources (evidence tiers per R4)

| Mechanism | Primary source | Tier | Read |
|---|---|---|---|
| Pre-vote | Ongaro, *Consensus: Bridging Theory and Practice*, thesis §9.6 | A | built; audited here |
| Leader transfer | thesis §3.10 (`TimeoutNow`) | A | |
| Learners | thesis §4.2.1 (catch-up rounds for non-voting members) | A | |
| Priority elections | MLRaft (abstract only, below); SOFAJRaft priority election (decaying target priority) | A (abstract) / C | |
| Parallel replication and processing | Cao et al., *PolarFS*, VLDB 2018 §5 (ParallelRaft); Gu, Wei, Qiao, Huang, *Raft with out-of-order executions*, IJSI 2021 (ParallelRaft-SE/-CE, TLA+) | A | read 2026-09-28 |
| Multi-log (MLRaft) | *MLRaft: Improvement of Raft based on multi-log synchronization model*, ICEITCE 2022, doi:10.1145/3573428.3573617 | A (abstract only — full text paywalled, HTTP 403 on 2026-09-28; flagged for verification) | abstract |
| Fast Raft | Castiglia, Goldberg, Patterson, *A hierarchical model for fast distributed consensus in dynamic networks*, arXiv:2004.06215 (§IV: pseudocode, safety, liveness; §VI evaluation); implementation report arXiv:2506.17793 | A | read 2026-09-28 |
| Fast quorum arithmetic | Lamport, *Fast Paxos*, Distributed Computing 2006; Zhao, *Fast Paxos made easy* (cited by Fast Raft for Lemma 2) | A | |

## 3. The mechanisms, mapped

### 3.1 Pre-vote (built; to audit)
Thesis §9.6: a would-be candidate asks for pre-votes at the term it would seek, without raising its own;
a peer refuses while it has heard from a leader within the minimum election timeout. Built in
`raft.rs` (`on_election_timeout`, `on_pre_vote`, `on_pre_vote_reply`). The audit adds the scenarios the
thesis names under simulation: a partitioned node rejoining with an inflated term cannot depose a healthy
leader; a symmetric partition heals without a disruptive election; and the pre-vote–CheckQuorum
interplay (a leader stepping down under CheckQuorum must not leave every peer refusing pre-votes forever:
the refusal window is the minimum election timeout, never longer).

**Corrected 2026-09-29 (slice 14).** The code held the refusal window longer than this: a follower forgot its
leader only at its own campaign, so for its own jittered timeout, and a voter yielding its timeout to a more
central one (§3.4) refused that voter's pre-vote. The timer now reports the lapse at the minimum election
timeout (`FollowerStep::LeaderLapsed`), and the drive calls `forget_leader`. In a three-region group the
outranked region had won 195 losses of 200, a timeout late
(`docs/bugs/2026-09-29-a-yielding-voter-refused-the-voter-it-yielded-to.md`).

### 3.2 Leader transfer (thesis §3.10)
The leader stops accepting new proposals, brings the target's log up to date, then sends `TimeoutNow`;
the target starts an election at once, skipping pre-vote (it was invited), and wins because its log is
current. If the transfer does not complete within an election timeout the leader resumes accepting
proposals (§3.10's abort). Used by planned maintenance (a draining node hands leadership off rather than
costing an election timeout) and by priority placement (3.4).

### 3.3 Learners (thesis §4.2.1)
A learner receives replication but neither votes nor counts toward any quorum. The dialect already
treats a node outside the voter set this way; the enhancement makes it explicit and adds the thesis's
catch-up rule: a new server is promoted only after it has caught up — replication proceeds in rounds, and
when a round completes within an election timeout the server is close enough to join without stalling
commits. The number of rounds is bounded (a server that cannot keep up is refused with a typed error, not
retried forever).

### 3.4 Priority elections
Refined 2026-09-28, before building. The leader should sit where commits are fastest, and a leader commits
once a majority including itself holds an entry. So a voter's priority is its **quorum round trip**: the
`⌊n/2⌋`-th smallest round trip from it to the other voters. It is not the mean.

Measured against Microsoft's published P50 round trips (Azure network latency statistics, page dated
2026-07-30, fetched 2026-09-28): for a five-region root group of East US, West Europe, Japan East, Southeast
Asia and Brazil South, the quorum round trip is 117 / 169 / 162 / 169 / 185 ms. An unprioritized election
averages 160 ms; the East US leader gives 117. With three regions (East US, West Europe, Japan East) it is
83 / 85 / 162 ms: 110 ms on average, 83 at best.

- **Measured, not predicted.** Each voter computes its own quorum round trip, with the spread of the path
  that sets it, from its measured paths (`peer_paths`, the Jacobson estimators the election timing uses).
  The Vivaldi coordinates were the first plan, but the fleet's integration is incomplete: each probe task
  owns an engine fed by one peer, and the announced coordinate comes from an engine that no sample feeds.
- **Exchanged through Raft.** A follower reports its figure in its `AppendReply`, and the leader returns
  every voter's figure in its `AppendEntries`, so all followers rank against the same table. The wire grows
  by one fixed-size record per voter.
- **Rank.** A voter's rank is how many live voters (the caller's liveness view) have an interval — round
  trip ± spread — lying wholly below its own. An unknown figure never outranks, and overlapping intervals
  tie. On one host every interval overlaps, so the gate is a no-op there, as it must be.
- **Election gate** (SOFAJRaft's decaying target, in ranks). A follower whose timeout fires campaigns only if
  its rank is within the timeouts it has yielded since it last heard a leader. Otherwise it yields this one,
  and each yield admits the next rank. So an election waits at most one timeout per more-central live voter,
  and none when the best live voter is up.
  - A yielding voter must grant the pre-vote of the voter it yields to. Until 2026-09-29 it did not (§3.1's
    correction). Measured with the fix over 200 seeds, the leader cut off: among three regions the most
    central survivor succeeds on every seed at its first campaign, at a 3,322 ms median, where the outranked
    one had won 195 of 200 at 6,766 ms.
  - Ties are left to Raft's randomized retry. Among five regions, three survivors tie within their spread,
    and 11 losses of 200 split their vote and take about 6.4 s (p99 6,472 ms). Three mitigations were measured
    and rejected (the bug record): a span of the base, a strict order among tied voters (Ongaro & Ousterhout
    2014 §5.2 abandoned ranking for the availability cost measured here), and deferring a campaign after
    granting a pre-vote.
- **Transfer** (3.2). A leader that has led for a full CheckQuorum window hands off to a caught-up, live
  voter whose interval lies wholly below its own. It does so at most once per leadership that aborts, since
  proposals are refused while a transfer is in flight and a failing target must not cost the group its
  availability.

### 3.5 Parallel vote replication and processing (ParallelRaft-CE)
Raft acknowledges and commits strictly in index order, so one lost `AppendEntries` holds back every later
entry. ParallelRaft lets a follower acknowledge, and the leader commit, entries of the current term out of
order. The published description omits the recovery details, and Gu et al. show it can then violate
consistency ("ghost log entries") when execution is out of order. Their corrected protocol,
ParallelRaft-CE, keeps a per-node **sync number** (the term whose entries it currently accepts), recovers
the previous term's entries Paxos-style over a majority before a new leader takes office, and advances a
follower's sync number only once it has acknowledged every entry of the older term.

The mapping, as verified (slice 9): out-of-order **acknowledgement** within a term — a synced follower
buffers the leader's entries that arrive ahead of a hole in its window, so they are not sent again and
commit the moment the hole fills — with **in-order commitment and application**.

**Built with pipelining (slice 11).** Out-of-order acknowledgement needs entries in flight ahead of a hole,
so the leader pipelines (thesis §10.2.2; etcd's probe and replicate states): a follower whose place is
confirmed is sent the next batch before the last is acknowledged, while its backlog is more than one resend
carries and what is in flight beyond the first unacknowledged batch fits its window; a refusal returns it to
probing, one batch at a time. When a resend would carry the whole backlog it is sent instead, since it also
recovers a lost batch within one send. Measured on five Azure regions (one send a period, as the council's
drive): a window of one batch changes nothing at 20 or 500 proposals a second, sends 14 % fewer bytes at
1,000, and at 2,000 keeps up — a 172 ms median commit — where no window is overloaded at 7.3 s; a larger
window cuts the tail under 1 % loss (458 → 321 ms p99).

**Out-of-order commitment was measured and rejected.** The prefix model found a 12-step history. A leader
commits an index out of order from a window's copy. Recovery by the next leader fills the uncommitted index
below it with a no-op. That no-op conflicts with the old leader's log, and Raft's truncation deletes the old
leader's replica of the committed entry. A later leader, elected without the one node still holding it,
loses it. Nothing in slates would use out-of-order commitment: the configuration applies in order, and the
leader proposes only once its log is fully committed.

### 3.6 Multi-log (MLRaft)
The abstract: the single log is divided into n logs, a leader is elected per log, leaders are spread by
priority election and dynamic leader transfer. In slates the council's state partitions naturally by
key (host epochs, takeover assignments, volume homes), so n logs over the same voters, each with its own
leader, spread leadership and let independent takeovers commit concurrently. Operations spanning logs
(a membership change, which every log must observe) go through a designated log with a barrier every
other log orders itself after. n is derived (not hand-set); at `n = 1` the design is today's single log
(R8: the laptop and the one-log case are the same code with `n = 1`).

**Built, measured and decided (slice 14; `crates/cluster/src/multilog.rs`).**
- **The merge.** A command is keyed (it writes one key's state) or global. Keyed commands go to the log their
  key hashes to; global ones to log 0. Each other log's leader appends a barrier once its replica of log 0
  commits a global command.
  - Every replica applies a keyed command in its log's order, in the epoch its log's last barrier opened.
  - A global command applies only when every other log has reached a barrier naming it, so every entry
    those logs ordered before it has applied first.
  - Keyed commands of different logs touch different keys and commute, so every replica reaches one state,
    whatever order the commits arrive in.
- **Leaders spread**: log k's preferred voter (the k-th best by quorum round trip) advertises the least
  measurable priority there, and priority elections and transfers move the log to it.
- **The premise, corrected.** The council's commands do not partition: Admit, Retire and TakeOver all change
  the member set, which every placement reads, so every one is global. The root group's `MoveHome` (per
  volume) and `PromoteRegion` (per region) are keyed; region admission and retirement are global.
- **Measured** on the five Azure regions (20 seeds, each log's preferred voter crashed for 20 s in turn).
  With five logs led apart:
  - A crash of log 0's leader pauses the keyed stream 463 ms, where one log pauses it for its election,
    4,484 ms.
  - A crash of any other log's leader stalls every log's keyed commands, 3,713–6,312 ms. Log 0 goes on
    committing global commands, each waits for the lost log's barrier, and the keyed commands behind every
    other log's barriers wait for them. So four region losses in five pause everything, where with one log
    only the leader's region's loss does.
  - A log that lost its leader pauses its own keyed commands 4,004–6,525 ms, no less than one log's
    election.
  - Steady keyed commands are slower, 174 → 302 ms median, since logs are led from less central regions.
  - Global commands wait for every log's barrier: 199 → 839 ms.
  - Messages grow fivefold.
- **Decided: both groups keep one log.**
  - The council's commands are all global, so more logs only cost.
  - The root group's failure-path command, a region's promotion, is keyed, but gains nothing in
    expectation. It is proposed as one of the five regions is lost, each alike. Its log's leader was in the
    lost region in one case of five, and it waits out that crash's pause of its log's keyed commands;
    otherwise it commits at the steady median. Averaged over the logs, as measured:
    - one log: (4,484 + 4 × 174) / 5 ≈ 1,036 ms;
    - two, three and five logs: 1,333, 1,399 and 1,301 ms.
  - CI's gate holds the inequality on its own run (2 seeds; 6,813 against 5,809 ms as five times the
    expectation), with the stall of every other log's crash.
  - A log per region pays across the WAN on every period. More than one log also needs compaction across
    logs (owed before any group could run it; GAPS).
  - The measurement found the lease defect of §3.1: before its fix, one log's pause was 4,615 ms, and a
    non-designated log's election took 8.7 s on the traced seed.

### 3.7 Fast Raft
The fast track: a proposer sends its entry straight to every member; a follower inserts it at the
proposed index if the slot is empty (marked *self-approved*) and votes to the leader; the leader commits
index `k = commitIndex + 1` when a **fast quorum** voted for the same entry in its current term. Otherwise,
with votes from a classic quorum, the leader inserts the entry with the most votes as *leader-approved*
and runs the classic track (one extra round). Elections compare only leader-approved entries; voters send
their self-approved entries with their vote, and the new leader re-runs the vote selection over them
(recovery).

- **Quorums, derived.** The paper states the fast quorum as ⌈3M/4⌉. The requirement behind it: a
  fast-chosen entry must have the most votes in every classic quorum the leader may hear from, i.e.
  `2|F| + |Q| > 2n` with `|Q| = ⌊n/2⌋ + 1`. slates derives `|F|` as the smallest size satisfying that
  inequality and checks it exhaustively against ⌈3n/4⌉ (they agree for every n checked by hand, 3–7).
- **The published recovery is unsafe, as suspected; the ballot rule replaces it** (slice 8,
  `crates/cluster/tests/slot_model.rs`). The election compares leader-approved entries only, and a
  self-approved vote carries no term. An exhaustive search of the log model found where that breaks.
  - **Under the two readings in which the leader decides by votes** (the pseudocode as written, and a
    leader deciding each index once per term), two values commit at one index; the shortest histories take
    15 and 18 steps with four nodes. In the second, a leader holding a leader-approved entry from term 1
    wins term 4 with two voters that hold only self-approved copies of the value committed in term 3. Two
    votes are short of a classic quorum, so the leader's own stale entry stands, overwrites theirs and
    commits. With five nodes, where a fast quorum (four) is larger than a classic one (three), the history
    derived by hand from this hypothesis replays step for step.
  - **Under the reading that keeps a leader's leader-approved entries,** as the paper's prose says ("treated
    the same as they are treated in classic Raft"), the searched scope has no fault. But one leader crash
    between a decision and its commit then stalls the log for good. The next leader may not decide over the
    inherited entry, and may not commit it, since it is not of its own term.
  - **The ballot rule** (Fast Paxos's, in terms of Raft's):
    - Every slot records its ballot: its term, and within a term a leader's decision outranks a fast vote.
    - Recovery takes, per index, the highest ballot among a majority's reports, the new leader's own
      included. A decision is re-proposed as it is. At a fast ballot, the value with at least
      `|Q| + |F| − n` of the reports is re-proposed, and otherwise the index is free.
    - Recovered values are re-proposed at the new term, so the new leader can commit them, which is what
      the third reading lacks.
    - The fast track opens only at free indices, and a leader decides from fast votes of its own term,
      once per index.

    It holds at every scope searched, up to 23,552,907 classes with five nodes and four terms, and the
    election needs no log comparison for safety.
- **Measured trade-off.** The paper: about half of classic Raft's latency below 4 % loss on five AWS
  sites, worse above (the classic track costs an extra round after a failed fast attempt). slates
  measures the same crossover on its own profiles and decides from the data whether the fast track is
  always on or chosen per group from the measured loss.

  **Measured (slice 13), on five Azure regions with derived windows** (a proposer in each region, 20 a
  second):
  - A proposer far from the leader commits up to 37 % sooner on the fast track below 4 % loss (Southeast
    Asia 435 → 275 ms at the median with no loss, 473 → 322 ms at 4 %).
  - A proposer beside the leader commits later (East US, which leads by priority, 156 → 201 ms): a fast
    quorum of four is larger than a classic three.
  - At 10 % loss the fast track is slower for every proposer (medians 199–479 → 448–598 ms).

  **Decided:** a group opens the fast track only when its proposals come from away from its leader, at a loss
  below the crossover. The council's and the root group's proposals all come from their leader, where the
  track only costs, so both keep it closed; it is built, verified and measured, and waits for a group whose
  proposers are elsewhere.

## 4. Composition

- **One log model, as built** (verified by the prefix model, slices 9 and 10; built in `RaftNode`, slice 10).
  Raft's log keeps every rule it has: in-order appends with the consistency check and truncation, Raft's
  election rule, and commitment by a majority's logs at the current term. Above the log, each node keeps a
  **window** of slots:
  - the leader's entries that arrived out of order;
  - fast votes, each with the term it was accepted in.

  A node accepts either only once it is **synced** to its term's leader, meaning its log holds the no-op
  that leader appends after its recovery.

  **The commit index is classic.** A node's commit index is the highest index a majority's logs hold at
  their leader's term; it is what a follower learns and what is retained. A leader also counts the indices
  fast quorums chose, in order past its commit index, and applies, acknowledges and reads at that frontier
  (`RaftNode::committed_through`); the frontier is its alone and ends with its leadership.

  **A slot goes only under a classic commit.** A slot is an acceptor's record of an accepted value, and a
  node drops it only once its commit index covers the slot's index: Raft's election rule then puts that
  entry, and every value below it, in every later leader's log. Nothing else drops a slot — not a sync, not
  the node's own election, not a fast commit.

  **Recovery.** Voters report their window slots with their votes. For each index above the candidate's
  last log entry, the highest ballot decides as in §3.7, and the leader appends the recovered values at
  its term, a no-op at each free index below the last of them, and then its own no-op. It keeps its own
  window. Each of these choices is backed by a search (`crates/cluster/tests/prefix_model.rs`), and each
  rejected rule is kept as a variant that fails by its shortest history:
  - **Reports are windows only.** The candidate's own log is kept (Raft's election rule makes it safe),
    and voters' log entries are not reported. That keeps a classic leader from resurrecting a deposed
    leader's uncommitted entries, which Raft discards.
  - **A window slot outlives the log entry that covers it.** Dropping it once covered loses a committed
    entry in 16 steps.
  - **Not at a sync.** Dropping a synced node's older slots at the sync (and clearing a new leader's window)
    loses a chosen value in 18 steps: the synced log carried it, but a later leader's truncation erased it.
    The explorer met it first (slice 10), at a scope the model's earlier searches had not combined.
  - **Not under a fast commit.** Pruning under a commit index that counts fast commits keeps a committed
    entry under the fast leader's term while every later leader holds it under its own, 12 steps; with a
    fourth term it loses the value.
  - **Commitment stays in order** (§3.5).

  Because a follower's committed prefix is classic, committed entries never differ in term between nodes,
  and Raft's log matching holds as it stands: the model never takes the path that would keep a committed
  entry under an older term than a leader's.
- **One configuration per fast term.** The fast track opens only on a committed configuration that is not
  joint, with the leader's no-op appended and no transfer in flight, and stays open for the term; a membership
  change waits for a classic term. A successor's recovery counts the votes against that configuration, which
  every later leader's log holds. An opening counts in the term it was announced in only.
- **Application stays in order** for every mechanism (the configuration state machine is sequential).
- **Pre-vote and priority** gate *starting* an election; **transfer** starts one deliberately; none
  changes the vote rule.
- **Learners** never count toward any quorum — classic, fast, or joint.
- **MLRaft** multiplies whole groups; everything above holds per log.

## 5. The simulation harness (built first; everything is tested against it)

A deterministic, seeded, sans-io driver over N nodes of the real `RaftNode` (not a model of it):

- **Network**: per-link latency drawn from a profile (laptop: 0; cluster: sub-millisecond; multi-region:
  a measured WAN matrix with jitter), loss, duplication, reordering, and partitions (symmetric,
  asymmetric, and the pre-vote-relevant "one node isolated then healed").
- **Nodes**: crash and restart from `SavedRaft` (what is durable is exactly what the dialect saves), clock
  drift on election timers, concurrent proposers.
- **Checks after every step**: Election Safety, Log Matching, Leader Completeness, State Machine Safety,
  and linearizability of the committed command history; each fast path exports a counter the test
  asserts moved (non-vacuity).
- **Bounds**: every queue in the harness is bounded and every run has a virtual-time deadline; a run that
  exceeds it is recorded as a liveness failure with the seed.
- **Model-level exploration** for the log-shape changes (Fast Raft, ParallelRaft): an exhaustive search
  over small configurations (3–5 nodes, 2–3 indices, bounded message counts) in Rust tests — TLA+ is not
  run in CI (banned item 13), so the search is the executable equivalent at small scope, and the
  randomized harness covers large scope.

## 6. Measurement plan

For each mechanism, before and after, on the three profiles, over the seed budget with the noise band
measured first: election latency (leader loss to first new commit), takeover latency (in-process fleet and
KIND), commit latency p50/p99 and messages per commit at 0–10 % loss, unavailability under partitions.
Rejected variants stay on record with their numbers (`docs/wip/BENCHMARKS.md`).

## 7. Build order

1. The simulation harness, driven against today's dialect (baseline numbers; conformance checks).
2. Pre-vote audit (3.1).
3. Leader transfer (3.2).
4. Learners with catch-up rounds (3.3).
5. Priority elections (3.4).
6. The generalized log and one recovery; ParallelRaft-CE (3.5).
7. Fast Raft (3.7), counterexample first.
8. MLRaft (3.6).
9. Wiring into the council and root groups, the in-process fleet suite, and the KIND lane.

## Build ledger

- **Slice 1 (2026-09-28): the safety explorer** — `crates/cluster/tests/explore.rs`. The real `RaftNode` under
  a seeded adversarial network (loss, duplication, reordering, partitions, crash-restarts from what the node
  retained), with Election Safety, Log Matching, Leader Completeness and State Machine Safety checked after
  every step against the whole history. Adversarial stretches alternate with calm ones (a history that never
  settles commits nothing, which left the first five-voter run vacuous: 108 entries over 400 seeds); a
  network tick delivers one message per node (one per step saturated the bag and silently dropped 14,324
  messages). The drive mirrors the council's: pre-vote first, the new leader's no-op, replication to the
  **other** voters only — the first cut sent the leader its own append, which demoted it at the same term,
  so leaders deposed themselves on most heartbeats until the filter (commits at three voters: 2,183 → 39,668
  with the filter). Full scale (CI, release, `--ignored`): 400 seeds × 4,000 steps × 3 and 5 voters, no
  violation. The workspace's debug run explores 24 seeds (about 10 s).
- **Slice 2 (2026-09-28): leadership transfer** (thesis §3.10) — `RaftNode::transfer_leadership`,
  `take_timeout_now`, `on_timeout_now` with the typed `TransferRefusal`; the leader refuses proposals and
  membership changes while a transfer is in flight, invites the target once its `match_index` reaches the
  leader's last index, and aborts at its second CheckQuorum tick (at least one whole election timeout, at most
  two). `TimeoutNow` is wire tag 7 (golden vectors now pin all seven message kinds — the module doc claimed
  them, none existed). Both groups record an invitation on the serve path and start the invited election in
  the drive loop (the only place terms change and broadcasts leave); the vote round is shared by a won
  pre-election and an invited campaign. Proven:
  - explorer: at full scale, 10,249 transfers started at three voters, 7,714 invitations, 7,341 invited
    elections; no safety violation;
  - by use, in-process fleet over real loopback: the council handoff took 0.101–0.203 s (median 0.103 s,
    five runs) against a leader-loss election of 1.019–1.817 s (median 1.316 s) under a 1 s election timeout;
    the root group's handoff across three regions took 0.092–0.096 s (three runs).
  - Owed from this slice: the users — a graceful drain (a stopping leader hands off first), priority
    placement (3.4) and MLRaft balancing (3.6) — and the WAN/KIND measurement.
- **Slice 3 (2026-09-28): the graceful drain** — the first user of transfer. The anchor's stop was a SIGKILL,
  so a planned stop of a council leader (a pod deletion, a rolling upgrade) cost a leader-loss election. Now
  the anchor writes a stop request into the supervision block (`SUP_STOP`); the daemon hands off every
  leadership it holds to the most caught-up voter, declares its deadline (`SUP_STOP_BY`: two CheckQuorum
  intervals of its slower group — the core's abort bound — and one period) and exits; the anchor kills only on no acknowledgement within
  the liveness budget, a lapsed heartbeat, or an overrun deadline. A drain completes when the successor is in
  office (this daemon learned it from the successor's first append) — a first cut completed on stepping down,
  which happens on the target's vote request before the target wins: two runs in three found no leader at
  the stop. Proven:
  - anchor, cross-process: a graceful child exits on its own after its drain (0.11 s, not killed); a deaf
    child is killed at the 300 ms test budget; a wedged child is killed at its heartbeat lapse, long before
    the 3 s deadline it declared;
  - in-process fleet: the drain reports `HandedOff` in 0.305–0.310 s (five runs: catch-up, invitation and
    the successor's first append, one 100 ms period each), and the survivors lead the moment it stops;
  - real processes (`slates daemon --fleet` × 3, `SIGTERM` to the leader): a survivor led 0.111 / 0.127 /
    0.106 / 0.125 s after the signal, the daemon exiting 0 each time, against a 1 s election timeout.
  - Measured-and-open: the drain is three drive-loop periods; waking the loop on an invitation (instead of
    the next tick) would cut it toward round trips — to be measured before it is built.
- **Slice 4 (2026-09-28): the timed simulation and the pre-vote audit** — `crates/cluster/tests/support/timed.rs`
  models real `RaftNode`s and the real `ElectionTimer` on a virtual clock: per-pair latency, jitter and loss,
  partitions and crashes as windows, the council's drive (heartbeat periods, pre-vote, CheckQuorum on the leader's
  cadence, the new leader's no-op, invitations), the SWIM probe's round trips feeding each node's path estimate
  (without them a follower derived its timing from one or two startup samples — a tail three times the true
  round trip — and waited 5.9 s instead of 2.2), and a steady proposal stream whose longest gap between commits
  is the unavailability. The audit (`tests/prevote.rs`, twenty seeds, a 0.25 ms cluster and an 80 ms ± 20 ms
  multi-region profile):
  - an isolated follower rejoins without deposing the leader and without moving the term, on every seed and
    both profiles; the direct-election control deposes the leader on every seed after inflating the term by
    22 (cluster) or 12–14 (multi-region);
  - an isolated leader is succeeded (cluster 1.4–1.6 s; multi-region 3.0–3.6 s) and its return deposes nobody;
  - it found a liveness bug in the production timer: correlated jitter split the vote for up to 19 s (bug record
    `docs/bugs/2026-09-28-correlated-election-jitter-livelocked-a-split-vote.md`); fixed by an independent draw.
- **Slice 5 (2026-09-28): compaction, bounded appends and fast backup** — the base learners need. Both
  groups compact by the thesis's size rule (§5.1.2 "Servers take a snapshot once the size of the log
  exceeds the size of the previous snapshot times a configurable expansion factor"; factor one, since the
  cost traded is the retained publication, not disk bandwidth). The shared rules live in `crates/cluster/src/fold.rs`:
  replay without copying the log, the compaction rule, the snapshot install that decodes the state first,
  and the snapshot-aware join and restore. Appends carry at most `raft_wire::append_batch_bytes`, and a
  refusal carries the §5.3 conflict hint. `InstallSnapshot` and its reply ride the wire (tags 8 and 9), and
  the groups' identity is checked against a retained origin.
  - Found on the way, latent until compaction ran, each with a test failing on `HEAD` (five-by-five):
    - a late append below a snapshot pushed compacted entries onto the log (5 → 7 entries);
    - one refusal per entry (20 to find an empty follower);
    - late replies moved progress back (10 → 3);
    - the snapshot reply credited the leader's own later snapshot.

    See `docs/bugs/2026-09-28-a-late-append-could-land-compacted-entries-on-a-log.md`.
  - Measured and rejected: compacting the moment a majority commits. The follower one round behind was sent
    the whole snapshot at every compaction, and the third voter of three never compacted itself. The leader
    now waits for its followers while the log is within twice the snapshot.
  - Proven:
    - explorer at full scale — 32,954 / 38,192 compactions, 2,917 / 2,405 snapshots installed, 1,419 / 978
      corrupted snapshots declined, 20,616 / 34,839 bounded batches, 15,309 / 17,072 conflict hints — with no
      violation;
    - the council folds exactly an oracle's configuration over 120 changes, and a voter left behind the
      leader's snapshot installs it and converges (the root group likewise);
    - a compacted council restores under its group id (the old identity check refused it: "retained council
      genesis differs");
    - fleet suite 53/53, CLI process suite 13/13.
  - Measured: 4,000 changes cost 25.5 µs per change before and 0.6 µs after; retained bytes 90,083 before and
    45,607 after (`docs/wip/BENCHMARKS.md`).
- **Slice 6 (2026-09-28): learners with catch-up rounds** (thesis §4.2.1; verified against the text). The
  core stages the members a target voter set adds (`RaftNode::catch_up`). A staged member is a replication
  target counted toward nothing. Each round replicates what the leader held when it began, and the member
  is caught up when a round completes within one CheckQuorum window. It is aborted after a whole window in
  which its lag did not shrink (`STALLED_WINDOWS`; the first tick only sets the baseline, so the abort comes
  at least one election timeout in, as a transfer's does). A new member's replication starts at the leader's
  end, so the conflict hint finds its end in one refusal and a nearly current member is not sent the whole
  log. Both groups' `reconcile_voters` stage before the joint change. The fleet keeps a session to a staged
  member (`DirectContact::Staged`), and the election timing counts voters only.
  - **Measured.** Figure 4.4(a) replayed (a fourth voter with an empty log, 40 entries behind at a
    two-entry budget, then the loss of an original voter): 21 rounds without a commit when added directly,
    1 when staged.
  - **Explorer.** It now explores membership changes: a spare node, adds through staging, and random
    removals including the leader. At full scale, in release, 27.7 s:

    | Counter | Three voters | Five voters |
    |---|---|---|
    | changes begun | 2,571 | 2,200 |
    | members caught up first | 1,359 | 1,151 |
    | stagings aborted | 147 | 128 |

    There was no violation.
  - **Found on the way:**
    - The explorer's own snapshot encoding dropped configuration entries, and its first membership run
      flagged the compacting node at seed 0, step 3,018. Its new trace tool (`replay_to_the_first_violation`)
      named the model's fault in one replay.
    - The groups' `voters()` returned the replication targets, which the recovery plan and elections read
      as the voter set. The CLI drain test caught it before commit: `DrainReport { council: NoTarget }`,
      and no survivor led within 40 s.
  - **Drain timing.** `SIGTERM` to a survivor in office took 171–208 ms over five runs, against 106–127 ms
    at `4e38d3e`. A timestamped trace splits one run: the drain's start to the successor winning took 109 ms
    (one period, unchanged); `SIGTERM` to the drain's start took about 95 ms. The daemon checks for a stop
    once per 100 ms heartbeat, so that part is the phase between the test's signal and that check. What
    shifted the phase is not established.
  - Fleet suite 53/53, CLI suite 13/13.
- **Slice 7 (2026-09-28): priority elections** (§3.4 as refined above).
  - **Mechanism.** The priority is the measured quorum round trip with its spread
    (`timing::quorum_priority`). Followers report it in `AppendReply` and the leader returns the table in
    `AppendEntries`. `RaftNode::election_rank` counts live voters whose interval lies wholly below one's own;
    `ElectionTimer::follower_period` yields one timeout per rank; `RaftNode::priority_transfer` hands off to
    an outranking live voter after `PRIORITY_WINDOWS` ticks, latched after one abort per leadership.
  - **Measured** (`tests/priority.rs`, Microsoft's P50 matrix of 2026-07-30; twenty seeds, the regions placed
    on hosts by a seed-dependent permutation):
    - **Three regions** (East US, West Europe, Japan East): the orders agree. East US leads every seed
      (quorum 83 ms, tied with West Europe at 85), and median commit latency is 137 ms either way. The
      derived election timing already favours the node whose slowest path is shortest.
    - **Five regions**: by the first timeout, East US 14 and West Europe 6; by priority, East US 20, after
      6 transfers. Median commit latency 189 ms by timeout, 171 ms by priority.
    - **East US down for 20 s, then back**: the control ends with Brazil South 3, Japan East 6, Southeast
      Asia 5 and West Europe 6 (median 234 ms); priority ends with East US on every seed (27 transfers,
      median 201 ms).
  - **Measured and rejected: requiring the target to hold every entry before a priority transfer.** A
    remote voter is always a proposal cadence behind, so no handoff happened once proposals flowed (six, all
    before the stream). The transfer's own catch-up (thesis §3.10) replaces it.
  - **Explorer.** Random priorities, with leaders handing off at CheckQuorum: 1,843 / 2,149 priority
    transfers at full scale, no violation. Fleet suite 53/53, CLI suite 13/13.
  - **Observations.**
    - Before the permutation, the simulation's fixed host numbering made one region win every first
      election, since the timer's jitter is a draw from the node's id.
    - The fleet's Vivaldi coordinates are not fed coherently: engines are per probe task, and the announced
      coordinate comes from an unfed engine. Priority therefore uses measured paths.
- **Slice 8 (2026-09-28): the slot model.** Fast Raft's published recovery loses committed entries, and the
  ballot rule does not (`crates/cluster/tests/slot_model.rs`; §3.7 above). The model is the log that the
  fast track and parallel replication share: every index above the committed prefix is a single-decree
  instance. Its actions are atomic, as a TLA+ specification's would be: a term bump, an election won with a
  chosen majority and its recovery, a proposal landing at a node, a leader's decision from a chosen
  majority's votes, one replication, and a commit. After every step it checks:
  - agreement (Fast Raft's Definition 2.1);
  - P2c: no leader of a term at or after a commit sends another value there, which is the claim of Fast
    Raft's Lemma 2;
  - one leader per term;
  - one decision per classic ballot.
  - **The published rule, four nodes, one index, two values, four terms.** Its decision loop is searched
    under three readings:

    | Reading | Classes searched | Result |
    |---|---|---|
    | as written | 1,049,232 | a committed value overwritten at step 13; two values committed at step 15 |
    | once per term | 1,199,113 | overwritten at step 16; two values committed at step 18 |
    | keeping leader-approved entries | 999,583 | no fault, but the log stalls: after a 12-step history with one crash, all 225 futures within four terms commit nothing |

    With five nodes, a scripted 27-step history commits two values. That fault needs no fast commit:
    breadth-first search found a 16-step five-node history in which a leader decides by votes over its own
    classically committed entry.
  - **The ballot rule.** No fault at any scope searched, and every path was taken at every scope:

    | Nodes, indices, values, terms | Classes | Fast commits | Recoveries a possible fast choice constrained | Commits above an uncommitted index |
    |---|---|---|---|---|
    | 4, 1, 2, 3 (the default suite) | 56,971 | 626 | 8,684 | — |
    | 4, 1, 2, 4 | 463,715 | 2,003 | 72,856 | — |
    | 4, 1, 3, 4 | 642,654 | 2,003 | 86,443 | — |
    | 5, 1, 2, 4 | 23,552,907 | 6,269 | 6,082,739 | — |
    | 3, 2, 2, 2 | 639,871 | 6,290 | 12,742 | 84,071 |

    One index proves every index count. Each action touches one index or the shared terms and votes, so a
    history over several indices projects onto a valid one-index history.
  - **The search.** Each state is stored as the representative of its class under renaming nodes and
    values. The ballot rule is searched breadth first over 128-bit fingerprints, level by level across
    every core (a skipped state needs a fingerprint collision, below 10⁻²¹ at a billion states). The
    published rule is searched breadth first on one core with whole keys, which yields the shortest
    counterexample.
  - **Measured and rejected** (`docs/wip/BENCHMARKS.md`):
    - A model that kept each leader's vote tally, searched without symmetry reduction. It held 18 GB after
      300 s without finishing, and it had no memory bound.
    - A depth-first search. On the same scope it visited the identical 463,715 classes in 1.16 s against
      the serial breadth-first search's 1.13 s, and held 38 MB against 94 MB. It saves memory but no time,
      loses the shortest counterexample, and does not spread across cores. The parallel breadth-first
      search takes 0.16 s.
  - **Next.** Build the ballot rule into the dialect, and explore the real code with the randomized
    explorer. The rule becomes sync terms (a node accepts out-of-order and fast slots only of the term it
    was synced to), windows above the Raft prefix, the recovery, and a sync append that truncates stale
    tails. Membership changes stay classic and in order, with no windows or fast votes open while one is
    pending.
- **Slice 9 (2026-09-28): the prefix model — the dialect's design, verified before it is built**
  (`crates/cluster/tests/prefix_model.rs`, on the search machinery the models now share,
  `tests/support/exhaustive.rs`). The slot model has no order. The dialect keeps Raft's log, so two
  steps of the design fall outside the slot model's proof:
  - the candidate's own log is kept;
  - a synced follower forgets older slots.

  This model has Raft's logs, appends with truncation, Raft's election rule, windows, syncing, and the
  recovery (§4). After every step it checks agreement, P2c, log matching, election safety and leader
  completeness.
  - **Found and fixed on the way.** The first design counted a window's copy toward a commit. The model
    found a 12-step history losing a committed entry (§3.5), so commitment is in order. It also showed that
    a window slot must survive the log entry that covers it.
  - **The design** holds at every scope searched, and every path it has was taken:

    | Nodes, indices, values, terms | Classes | Fast commits | Recoveries of a possible fast choice | Slots dropped at a sync |
    |---|---|---|---|---|
    | 3, 3, 1, 2 (the default suite) | 331,522 | 886 | 18,178 | 9,467 |
    | 3, 3, 1, 3 | 2,228,602 | 1,347 | 125,506 | 49,934 |
    | 3, 3, 2, 3 | 14,625,406 | 6,666 | 784,136 | 546,068 |
    | 4, 2, 2, 3 | 1,220,407 | 484 | 32,178 | 4,632 |
    | 3, 2, 2, 4 | 515,747 | 24 | 8,836 | 3,360 |

  - **Rejected alternatives,** kept as executable variants:
    - **Commits counted from windows:** loses a committed entry in 12 steps.
    - **Window slots dropped once the log covers them:** loses one in 17. A fast quorum commits at
      index 2. A new leader's append covers one voter's log there, so the voter drops its vote; the next
      append conflicts below and truncates the entry; a later leader, elected with the one voter still
      holding a vote, finds one vote short of the threshold and fills the index with a no-op.
    - **Voters reporting their logs too:** safe (14,648,981 classes), but its recoveries resurrected a
      deposed leader's entry 104,206 times. Classic Raft never does that, and the design never needs to.
  - **Next.** The dialect (`RaftNode`): the window and sync term in `SavedRaft`, window reports in
    `VoteReply`, the sync point and fast-track opening in `AppendEntries`, the fast track's messages, and
    the randomized explorer extended to all of it.
  - **Correction (slice 10).** The design above was unsafe outside the scopes searched: it dropped a synced
    node's older slots at the sync. The corrected design, and the rejected rules, are in §4 and slice 10.
- **Slice 10 (2026-09-29): the fast track and the window in the real core, and two design faults the
  explorer found.**
  - **Built** (`crates/cluster/src/raft.rs`, `raft_wire.rs`):
    - the window and the synced term, retained in `SavedRaft` and validated before a node votes
      (`InvalidWindow`);
    - window reports in `VoteReply`, the sync point and the fast track's opening in `AppendEntries`;
    - the fast track's two messages, `FastPropose` and `FastVote` (tags 10 and 11, golden vectors,
      hostile-input tests);
    - the recovery (§3.7's ballot rule over windows);
    - the fast track: a leader opens it, any synced node proposes, voters vote to their leader, the leader
      decides each index at a classic quorum of votes and commits at a fast quorum;
    - buffering of the leader's entries ahead of a hole, and their absorption;
    - the window's bounds: a byte budget and the span it buys above the log.

    The council and the root group dispatch the two messages; the window budget defaults to zero, so nothing
    opens a fast track until the groups are wired (owed).
  - **The explorer covers it all on the real code.**
    - Every node holds a window as large as one append.
    - A leader may open the fast track at any step.
    - Any node proposes on it, and votes go to the voter's leader.
    - A ghost of every vote cast (the acceptors' state) marks the indices a fast quorum chose.
    - New checks: no two commands chosen at one index; a new leader holds every chosen command; log terms
      never decrease; windows stay within budget and above the commit index.
    - State Machine Safety and Leader Completeness compare commands and configurations, and terms too except
      at a chosen index, which a leader commits under its term and a successor re-proposes under its own.
    - Log Matching stays Raft's own.

    At full scale (400 seeds each of three and five voters, 24 s): 1,227 and 1,691 fast choices, 743 and 831
    fast commits, 740 and 1,474 recoveries of a fast choice, 10 and 27 commands committed under two terms,
    7,704 and 12,431 slots pruned under a classic commit. Each path is floored per seed, or per exploration
    where it is rarer.
  - **Found and fixed, each with a failing test first:**
    - A leader's own fast votes never left its window. Three voters need all three votes, so fast commits
      stopped after the window filled (3 of 10).
    - A leader handing off kept deciding from votes, which can keep its target from ever catching up
      (thesis §3.10). It now decides nothing until the transfer ends, and decides the waiting votes if it
      aborts.
    - An opening of the fast track outlived its term. A node that won a term without opening it, then timed
      out, still believed the previous term's opening and voted where no leader had opened (the explorer's
      first run, seed 0). An opening now counts in the term it was announced in only.
    - The fast track could open with `C_new` appended and not committed. A successor lacking it would take
      the joint configuration as its own and treat a chosen index as free. It now opens only on a committed
      configuration.
    - **The design's sync rule lost a chosen value** — the explorer at full scale, seed 266 (three voters,
      step 630), then the model in 18 steps at three nodes, three indices and four terms. The model's
      earlier full scopes never combined three indices with four terms. A new leader cleared its window;
      a later leader's truncation then erased the log entries that carried the value. The first correction
      (older slots kept until the node's commit covers its synced leader's no-op; slots pruned under the
      commit index) failed in the model too: a fast commit is in no majority's logs. The rule that holds:
      a slot goes only under a classic commit, and the commit index is classic (§4).
      [Bug record](../../bugs/2026-09-29-window-slots-dropped-before-a-classic-commit-lost-chosen-values.md).
  - **Measured and rejected:** answering term differences at committed indices, by skipping the
    consistency check at the commit index and refusing a snapshot at or below it. Both answered a symptom of
    a commit index that counted fast commits; with a classic commit index no committed entry differs in term
    between nodes, so both were reverted rather than kept as a second path.
  - **The model, re-verified.** Each node knows its classic commit index, and the append keeps a follower's
    committed prefix as the code does. The design holds with Raft's strict log matching:
    - three nodes, three indices, one value, four terms: 152,906,020 classes;
    - three nodes, three indices, two values, three terms: 188,172,261 classes.

    Both were searched by hand: they hold 11.7 GB and 14.0 GB, past CI's 4 GiB ceiling. CI searches the
    scopes that fit, and every rejected rule at the smallest scope where it fails (`docs/wip/BENCHMARKS.md`).
    The serial search keeps fingerprints in its visited set now, not whole keys: 131 bytes of accounted
    memory per state for a 64-byte key, where a key held twice cost about 400.
  - **Next.** The leader pipelines: it sends the next batch before the last is acknowledged. Without that a
    follower almost never has a hole to buffer across (4 buffered entries in 3,200,000 explored steps), and
    §3.5's out-of-order acknowledgement pays nothing. Then the groups' wiring (window budget, vote routing to
    the leader, the fast track's policy) and the timed measurements that decide it.
- **Slice 11 (2026-09-29): pipelined replication, and a leader whose cost grew with its backlog.**
  - **Built** (`RaftNode::replicate_to`, with its first failing tests):
    - A follower is **probing** after an election, a refusal or its staging: sent one batch from its next
      index until one is acknowledged.
    - Otherwise it is **pipelined**: sent the next batch before the last is acknowledged, while what is in
      flight beyond its first unacknowledged batch fits its window, and the next index moves past what went.
    - Otherwise it is sent its first unacknowledged batch again — a heartbeat when it holds everything.
    - A batch goes ahead only when a resend would not carry the whole backlog. The first cut went ahead only
      when a resend would not reach the next index, and at 2,000 proposals a second — batches just short of
      full — it never went ahead, however far the backlog grew.

    With no window nothing goes ahead, so the groups (window zero until wired) send as before, byte for byte.
  - **Measured** (`crates/cluster/tests/pipelining.rs`; five Azure regions, 20 seeds, 30 s streams,
    `docs/wip/BENCHMARKS.md`):

    | Offered rate, loss | No window: median / p99, commits a second | One batch | Four batches |
    |---|---|---|---|
    | 20 or 500 a second | 156–174 / 206–241 ms | the same, byte for byte | the same |
    | 1,000, none | 172 / 222 ms, 96.5 MB sent | 172 / 222 ms, 83.1 MB | as one batch |
    | 1,000, 1 % | 183 / 267 ms | 174 / 241 ms | 174 / 241 ms |
    | 2,000, none | 7,319 / 14,378 ms, 1,029 | 172 / 222 ms, 1,988 | as one batch |
    | 2,000, 1 % | 7,357 / 14,483 ms, 1,022 | 260 / 458 ms, 1,980 | 201 / 321 ms, 1,985 |
    | 4,000, none | 1,030 a second | 2,058 a second | 2,058 a second |

    A window of one batch holds the capacity, since the drive sends one append a period and the quorum round
    trip (83–185 ms) is under two periods; a larger one cuts the tail under loss, since followers buffer
    more while a hole is repaired. CI gates the 2,000-a-second case and the identity at a low rate
    (`a_window_of_one_batch_keeps_up_where_none_does`, 0.64 s in debug).
  - **Found on the way: a leader's work per message grew with its backlog.** The commit rule tried every index
    from the log's end down to the commit index, and every configuration lookup scanned the log. One
    proposal cost 20 ms at a 5,000-entry backlog, and it now costs 61 ns to 102 ns at any backlog up to
    50,000. The rule now takes the index where a majority begins among the match indices, and the log's
    configuration entries are indexed. [Bug record](../../bugs/2026-09-29-a-leaders-commit-rule-scanned-its-backlog.md).
  - **Found on the way: the recovery read only its own window's reach** above its log, while the prefix
    model's reads every slot above the log. A leader with a smaller window than a voter's left an index free
    where the voter had helped choose a value beyond it — harmless only while every window is the same, which
    nothing enforced. It now reads every slot above its log (each reporter's window bounds its reports), so a
    group's windows may differ, as windows derived from each node's measured paths will. The explorer now
    gives each node one of three windows per history (one, one and a half, two appends); at full scale 2 and
    25 recoveries took a value past the new leader's own reach, with no violation.
    [Bug record](../../bugs/2026-09-29-a-recovery-read-only-its-own-windows-reach.md).
  - **Next.** The groups' wiring: a window derived from each node's measured paths — one batch holds the
    capacity, and more cuts the tail under loss — then votes routed to the leader, the fast track's policy from
    its crossover measurement, MLRaft, and the KIND lane.
- **Slice 12 (2026-09-29): the groups' window, derived.**
  - **The rule** (`ElectionTiming::window_budget`, `slates_cluster::timing::REPAIR_ROUND_TRIPS`): one append
    budget for each period a lost batch takes to repair on the node's slowest measured voter path.
    - The leader keeps sending one batch a period ahead while the follower's refusal travels back and the
      resend travels out: two round trips, ⌈2 × tail / heartbeat⌉ periods.
    - That is one batch on a loopback, where nothing need go ahead, and four or more across regions.
    - The council and the root group set it each period with their election timing, and the recovery's
      reading of every report (slice 11) makes windows that differ across a group safe.
  - **Measured** (the same five regions, 20 seeds, 30 s streams; each node deriving its own window each period
    in the simulation as the daemon does):
    - The derived window equals the best fixed window in every case.
    - At 2,000 proposals a second with 1 % loss, 201 / 321 ms median / p99, where one batch gives 260 / 458 ms.
    - At every other rate and loss it matches one batch or four, whichever is better.
  - **Next.** The fast track's crossover — proposers away from the leader, loss, placement — which decides
    whether a group opens it, and its votes' routing to the leader in the fleet.
- **Slice 13 (2026-09-29): the fast track's crossover, and a leader that fills the holes lost votes leave.**
  - **The measurement** (`crates/cluster/tests/fast_track.rs`, on the timed simulation):
    - A proposer can sit in any region. Classic, it forwards each command to its leader; on the fast track, it
      sends to every voter, and each voter's vote goes to its leader.
    - It sends again when a command is not committed within two round trips of its slowest path.
    - Latency runs to when the proposer learns the commit: the leader's commit and the leader's one-way path
      back.
    - The table is in §3.7 and `docs/wip/BENCHMARKS.md`. CI gates the three facts the decision rests on
      (0.48 s in debug).
  - **Found and fixed: a fast track under loss stalled for good.** A leader decides fast-track indices in
    order, and an index whose votes fell short of a classic quorum — a proposal or votes lost — held up every
    index behind it: the proposer's resend goes to a new index and never fills the old one. At 4 % loss a
    proposer lost up to half its commands (348 of about 780 committed).
    - The fix is Fast Paxos's coordinator-run round: the leader proposes a no-op at the stalled index on its own
      fast track (`RaftNode::stalled_index`, `fill_hole`). A voter that voted there re-sends its vote, one that
      did not votes the no-op, and the classic quorum it then hears decides by the ballot rule, which does not
      look at what is proposed.
    - The drive fills an index once it has stalled for a repair's two round trips. At 4 % loss every proposer
      commits as many commands as on the classic track (18–35 fills per region over 20 seeds).
    - The explorer drives it as an adversarial move: 208 and 206 fills at full scale, with no violation.
  - **Decided:** the groups keep the fast track closed (§3.7): their proposals all come from their leader, where
    it only costs.
  - **Next.** MLRaft (§3.6), and the KIND lane's measurement of the groups under a burst.
- **Slice 14 (2026-09-29): MLRaft — built, verified, measured, and one log kept; its measurement found a voter
  refusing the voter it yielded to.**
  - **Built** (`crates/cluster/src/multilog.rs`; §3.6): `n` Raft logs over one voter set, the routing (a keyed
    command to its key's log, a global one to log 0), the barrier, the merge, and leaders spread by priority.
    Log `k` prefers the `k`-th best voter by quorum round trip, which advertises the least measurable priority
    there. At `n = 1` it is the single log.
  - **Found and fixed on the way:** the preferred voter first advertised a zero round trip. That is the unknown
    priority, which outranks nothing, so no log ever handed off. The test checks the hand-off itself
    (`each_log_hands_off_to_its_preferred_voter`), and failed before the fix.
  - **Verified** (`crates/cluster/tests/multilog.rs`). The explorer drives every node's real logs over one
    adversarial network: loss, duplication, reordering, partitions, crash-restarts, keyed and global
    proposals, barriers. It checks each log's Raft safety, and the merge: every node's application is a
    prefix of one history, across nodes and restarts. That covers each key's commands in order with the
    epoch each saw, and the global commands in order.
    - At full scale (200 seeds × 3,000 steps; three voters × three logs, and five × two; 1.40 s in release),
      with no violation:
      - keyed commands applied: 99,499 and 62,858;
      - global commands applied: 18,483 and 13,180;
      - barriers: 3,398 and 1,142;
      - crash-restarts: 9,714 and 9,597, with 46,551 and 20,077 restarted replays matched.
    - A mutation, a global command applied without waiting for the other logs' barriers, is caught at seed
      0, step 1,072.
  - **Measured and decided** (§3.6; `crates/cluster/tests/multilog_timed.rs`, whose `n = 1` case measures what
    the single-log simulation does: 174 against 171 ms). Both groups keep one log. A keyed command proposed as
    a region is lost expects 1,036 ms with one log, and 1,301–1,399 ms with two, three or five. A crash of any
    non-designated log's leader stalls every log's keyed commands.
  - **Found and fixed: a yielding voter refused the voter it yielded to** (§3.1, §3.4). In a five-log group, a
    crash of log 1's leader took 8.7 s to elect a successor. The trace showed the most central survivor
    refused by every voter: each was yielding its own timeout to it and still held its lost leader's lease.
    - The lease now lapses at the minimum election timeout, as thesis §4.2.3 sets.
    - Three regions, 200 seeds: the most central survivor succeeds on every seed at its first campaign
      (3,322 ms median), where the outranked region had won 195 at 6,766 ms.
    - Five regions: the median falls from 4,430 to 4,158 ms. Ties then split 11 losses in 200 (p99 4,828 →
      6,472 ms), which the bug had serialized. Three mitigations were measured and rejected (the bug record).
    - The daemon's fleet suite passes 53 of 53.
  - **Next.** The KIND lane's measurement of the groups under a burst.
- **Slice 15 (2026-09-29): the priority election on real pods, and a round budget that cut off the far
  voter.**
  - **The measurement** (`cargo xtask kind succession`; `docs/wip/kind-lane.md`, Piece 6). Three pods on
    KIND, pod 0's egress 80 ms and pod 1's 20 ms. A central leader's egress is cut, and the survivors are timed
    to a successor. It is slice 14's three-region case on real daemons, real sockets and `tc netem`.
  - **Every node's status now reports its election state** — term, priority, rank, lease, the campaigns it
    began, and the pre-vote replies it drew and refused by reason. With those, the first runs were read
    rather than guessed at.
  - **Found and fixed: a round with nothing gathered stopped at its lookahead.** The collection loop took the
    extender's "no extension" at three quarters of the deadline for "stop now". A candidate whose one live
    voter answered in the last quarter never collected it: on KIND, 7 and 14 empty pre-elections, and 13.4
    and 25.4 s to a successor. The loop now gives such a round its whole deadline, unextended, as the
    budget's contract says; a round that gathered and stalled stops as before
    (`docs/bugs/2026-09-29-a-round-with-no-reply-yet-gave-up-at-its-lookahead.md`).
  - **Measured** (six fresh fleets each):
    - with the lease and round fixes, the central survivor succeeds 6 of 6 (1.57–4.96 s, median 3.08 s);
    - without the lease fix, the outranked pod succeeds 6 of 6 (median 6.47 s), after refusing the central
      candidate's pre-votes as leased.
    The simulation of the same profile had predicted both successors, 200 of 200 each way, and CI now gates
    its prediction.
  - **Open.** A symmetric partition never heals: neither side probes a peer it believes dead, so a healed
    node stays out until it restarts. And a late pre-vote reply is dropped: 4 in 10 of the successors'
    pre-elections cost a timeout for a reply just past the deadline. Both are in GAPS.
  - **Next.** The partition heal, then the late pre-vote reply.

