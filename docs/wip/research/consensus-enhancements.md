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
Each member has an election priority (derived, never hand-set: from its measured centrality — the
Vivaldi coordinate's mean RTT to the other voters — so the leader sits where commits are fastest). A node
starts an election only while its priority is at least the group's current *target priority*; the target
decays after each election timeout that elected nobody, so liveness never depends on the highest-priority
node being up (SOFAJRaft's rule). A leader that sees a caught-up voter of higher priority transfers to it
(3.2). MLRaft uses the same two tools to spread leaders across nodes.

### 3.5 Parallel vote replication and processing (ParallelRaft-CE)
Raft acknowledges and commits strictly in index order, so one lost `AppendEntries` holds back every later
entry. ParallelRaft lets a follower acknowledge, and the leader commit, entries of the current term out of
order. The published description omits the recovery details, and Gu et al. show it can then violate
consistency ("ghost log entries") when execution is out of order. Their corrected protocol,
ParallelRaft-CE, keeps a per-node **sync number** (the term whose entries it currently accepts), recovers
the previous term's entries Paxos-style over a majority before a new leader takes office, and advances a
follower's sync number only once it has acknowledged every entry of the older term.

The mapping: out-of-order **acknowledgement and commitment** within a term; **in-order application**
(the configuration state machine is order-dependent, so execution stays sequential — ParallelRaft-SE's
semantics, which Gu et al. refine to Multi-Paxos; ghost entries cannot break sequential execution). The
commit index exposed to the state machine is the contiguous committed prefix.

### 3.6 Multi-log (MLRaft)
The abstract: the single log is divided into n logs, a leader is elected per log, leaders are spread by
priority election and dynamic leader transfer. In slates the council's state partitions naturally by
key (host epochs, takeover assignments, volume homes), so n logs over the same voters, each with its own
leader, spread leadership and let independent takeovers commit concurrently. Operations spanning logs
(a membership change, which every log must observe) go through a designated log with a barrier every
other log orders itself after. n is derived (not hand-set); at `n = 1` the design is today's single log
(R8: the laptop and the one-log case are the same code with `n = 1`).

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
- **Suspected hole in the recovery rule, to be tested before building on it.** The election compares
  leader-approved entries only, and a self-approved vote carries no term. A candidate whose last
  leader-approved entry is w at index i (term t−1) can win term t+1 even though v was fast-committed at i
  in term t (the leader of t held v leader-approved; the fast quorum held v self-approved, and their last
  leader-approved entries may be older than w). If the new leader keeps w by the classic rule, a committed
  entry is lost. The fix Fast Paxos implies: every self-approved vote records the term it was cast in;
  recovery takes, per index, the highest term among the reports; a leader-approved entry at that term
  wins; otherwise (fast votes only) the value that could have been fast-chosen wins. The simulation must
  find the paper-rule counterexample (a failing test) before the corrected rule is built.
- **Measured trade-off.** The paper: about half of classic Raft's latency below 4 % loss on five AWS
  sites, worse above (the classic track costs an extra round after a failed fast attempt). slates
  measures the same crossover on its own profiles and decides from the data whether the fast track is
  always on or chosen per group from the measured loss.

## 4. Composition

- **One log model.** A slot is empty, self-approved (entry, the term it was voted in), or leader-approved
  (entry, term). ParallelRaft's holes and Fast Raft's overwritable slots are the same generalization:
  both need a recovery phase over a majority before a new leader takes office, so there is **one**
  recovery: per index above the leader's contiguous committed prefix, the highest-term report decides
  (Paxos phase one), with Fast Paxos's rule when that term's reports are fast votes.
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
