# Authority and durability for heads, chains and placement: the maximal design for a laptop-to-multi-datacenter VFS

Status: complete (written serially by the architect, 2026-09-04, for the A-6 proposal). Every
quotation below was fetched or read from the primary source on 2026-09-04 unless marked "verify".
Evidence tiers as in `README.md`.

## 0. The question

Ada (2026-09-04): "What is the *maximally* correct, robust, performant, efficient, fast solution
here? consider we're building effectively an on-demand VFS provisioning and merge system that
needs to run single-laptop and multi-datacenter." and then "Push harder. Do actual research."

The objects: volume heads, green merge chains, landing leases, the catalog. Each has exactly one
legal writer at a time (the owner), is small, and is written at seal and merge cadence. The
requirements: create in the laptop budget everywhere; merge path in microseconds on a laptop and
one intra-region round trip in a fleet; f-fault tolerance across failure domains with a bounded
copyset count; regional latency with cross-region durability as a policy; one code path for N=1.

## 1. The formal basis: Vertical Paxos

Lamport, Malkhi and Zhou, "Vertical Paxos and Primary-Backup Replication", PODC 2009 (MSR-TR-2009-63;
read from the authors' PDF, 2026-09-04) [A]:

- "We introduce a class of Paxos algorithms called Vertical Paxos, in which reconfiguration can
  occur in the middle of reaching agreement on an individual state-machine command. Vertical Paxos
  algorithms use an auxiliary configuration master that facilitates agreement on reconfiguration.
  A special case of these algorithms leads to traditional primary-backup protocols. We show how
  primary-backup systems in current use can be viewed, and shown to be correct, as instances of
  Vertical Paxos algorithms."
- Why consensus is mostly used for configuration: "While some consensus algorithms, such as Paxos,
  have started to find their way into those systems, their uses are limited mostly to the
  maintenance of the global configuration information in the system, not for the actual data
  replication." And: "a master allows a state-machine implementation to tolerate k failures using
  only k+1 processors instead of the 2k+1 processors required without it."
- The read/write quorum split: "Vertical Paxos achieves this structure by distinguishing between
  read and write quorums." The primary-backup case: "letting read quorums be as small as
  possible—namely, making any single acceptor a read quorum, so the only write quorum is the set of
  all acceptors. These are the quorums that allow k-fault tolerance with only k+1 acceptors.
  Suppose that in Vertical Paxos II we also always make the leader one of the acceptors and, upon
  reconfiguration, always choose the new leader from among the current acceptors. The new leader by
  itself is a read quorum for the previous ballot. Hence, it can perform the state transfer all by
  itself, with no messages."
- Local reads under a lease: "The primary obtains a lease that gives it permission to reply
  directly to reads from its local state. A new primary cannot be chosen until the lease expires."
- Vertical Paxos I versus II: in I "When a configuration changes ... the new configuration becomes
  active right away. The previous configuration remains active only for storing old information";
  in II "a leader has to query only acceptors from a single previous ballot ... Vertical Paxos II
  is especially useful for primary-backup replication."
- Instances: "Niobe, Chain Replication, and the Google File System are three examples of such
  protocols that have been deployed in systems with hundreds or thousands of machines ... the first
  two can be viewed as Vertical Paxos algorithms."
- The acceptor rule that is the fence: each acceptor keeps `maxBallot[a]`, "initially 0, that never
  decreases. The acceptor will not vote in a ballot whose number is less than maxBallot[a]."

The design below is Vertical Paxos II with the owner as the leader-acceptor, the regional
consensus group as the configuration master, one quorum rule for every object (2f+1 candidates,
commit at f+1, the acknowledging set recorded; §9 item 3 supersedes the two-rule draft of §4), and
a lease for local reads at the owner.

## 2. Production instances of the shape

- **FaRM** [A: Dragojević et al., "No compromises", SOSP 2015; abstract via OpenAlex 2026-09-04]:
  "a main memory distributed computing platform called FaRM can provide strict serializability, high
  performance, durability, and availability. FaRM achieves a peak throughput of 140 million TATP
  transactions per second on 90 machines with 4.9 TB database, and it recovers from a failure in
  less than 50 ms." FaRM replicates regions primary-backup under a configuration manager with
  leases and precise membership (the reconfiguration steps and the Zookeeper use are from the
  paper body: verify).
- **RAMCloud** [A: Ongaro et al., "Fast Crash Recovery in RAMCloud", SOSP 2011; read 2026-09-04]:
  "RAMCloud scatters backup data across hundreds or thousands of disks, and it harnesses hundreds of
  servers in parallel to reconstruct lost data ... In a 60-node cluster, RAMCloud recovers 35 GB of
  data from a failed server in 1.6 seconds." The coordinator "manages configuration information such
  as the network addresses of the storage servers and the locations of objects; it is not involved
  in most client requests." Placement is decentralised: "each RAMCloud master decides independently
  where to place each replica, using a combination of randomization and refinement ... A backup is
  rejected if it is in the same rack as the master or any other replica for the current segment."
  Coordinator failover: "The coordinator will use ZooKeeper to store its configuration information
  ... the active coordinator and additional standby coordinators will compete for a single
  coordinator lease in ZooKeeper." The primary is the sole writer: "a master that is suspected of
  failure (a sick master) must stop servicing requests before it can be recovered ... Once a backup
  with a replica of the active segment has been contacted, it will reject backup operations from
  the sick master". Recovery parallelism: "RAMCloud divides the objects of the crashed master into
  partitions of roughly equal size. Each partition is assigned to a different recovery master".
  The earlier position paper [A: Ousterhout et al., "The Case for RAMClouds", OSR 2009; read
  2026-09-04] states the multi-datacenter trade: "applications will have to accept higher latency
  for write operations (10's or 100's of milliseconds) in order to update remote datacenters
  synchronously. An alternative is to allow write operations to return before remote datacenters
  have been updated (meaning some 'committed' data could be lost if the originating datacenter
  crashes)."
- **Ceph** [B: Ceph developer documentation, "Peering", fetched 2026-09-04]: peering is "The process
  of bringing all of the OSDs that store a Placement Group (PG) into agreement about the state of
  all of the objects in that PG"; the acting set is "The ordered list of OSDs that are (or were as of
  some epoch) responsible for a particular PG"; epochs are OSD-map versions and "Last Epoch Started"
  is "The last epoch at which all nodes in the acting set for a given placement group agreed on an
  authoritative history"; the Golden Rule: "no write operation to any PG is acknowledged to a client
  until it has been persisted by all members of the acting set for that PG"; stale OSDs are excluded
  because the map "must reflect that the OSD was alive and well as of the first epoch in the current
  interval."
- **BookKeeper** [C: Apache BookKeeper protocol documentation, fetched 2026-09-04]: "A ledger has a
  single writer and multiple readers (SWMR)"; ensemble E, write quorum Qw, ack quorum Qa with
  "E >= Qw >= Qa" and the system can "tolerate Qa – 1 failures without data loss"; entries carry the
  Last Add Confirmed so "another client" can "read entries in the ledger up as far as the last add
  confirmed"; fencing sends "a fence message to all the bookies in the last fragment" so that "if
  the old writer is alive and tries to add a new entry there will be no write quorum in which Qa
  bookies will accept the write"; recovery "asks all bookies for the highest last add confirmed
  value" and reads forward.
- **Kafka** [C: Apache Kafka replication design wiki, fetched 2026-09-04]: the leader keeps "a set of
  in-sync replicas (ISR): the set of replicas that have fully caught up with the leader"; "Once the
  leader receives the acknowledgment from all replicas in ISR, the message is committed"; "After a
  configured timeout period, the leader will drop the failed follower from its ISR and writes will
  continue on the remaining replicas in ISR"; the rationale versus majority quorums: the
  primary-backup approach "tolerates more failures and works well with 2 replicas", f replicas
  tolerating f-1 failures. KIP-101 [C: Apache Kafka KIP-101, fetched 2026-09-04] records why an
  epoch had to be stamped on every batch: "The follower takes an extra round of RPC to update its
  high watermark. This gap leaves the possibility for a fast leader change to result in data loss"
  and "The replicas can diverge, with different message lineage in different replicas." The fix:
  followers query "the appropriate LeaderEpoch from the leader's vector of past LeaderEpochs".
  Lesson for slates: the epoch must be on every record, not inferred from a watermark.
- **Chubby** [A: Burrows, OSDI 2006; read 2026-09-04]: sequencers are fencing tokens: "a lock holder
  may request a sequencer, an opaque byte-string that describes the state of the lock immediately
  after acquisition. It contains the name of the lock, the mode in which it was acquired, and the
  lock generation number. The client passes the sequencer to servers ... The recipient server is
  expected to test whether the sequencer is still valid and has the appropriate mode; if not, it
  should reject the request." Coarse-grained use is intended: "Coarse-grained locks impose far less
  load on the lock server ... an application might use a lock to elect a primary, which would then
  handle all access to that data for a considerable time". Master lease and grace period: "the
  master must obtain votes from a majority of the replicas, plus promises that those replicas will
  not elect a different master for an interval of a few seconds known as the master lease."
- **PNUTS** [A: Cooper et al., VLDB 2008; read 2026-09-04]: "We have therefore chosen to make all
  high latency operations asynchronous, and to support record-level mastering. Synchronously writing
  to multiple copies around the world can take hundreds of milliseconds or more"; "Per-record
  timeline consistency is provided by designating one copy of a record as the master, and directing
  all updates to the master copy ... a one week trace of updates to 9.8 million user ids in Yahoo!'s
  user database showed that on average, 85 percent of the writes to a given record originated in the
  same datacenter"; mastership migrates: "If a user moves from Wisconsin to California, the system
  will notice that the write load for the record has shifted to a different datacenter (using
  another hidden metadata field in the record that maintains the origin of the last N updates) and
  will publish a message to YMB indicating the identity of the new master."
- **CockroachDB** [B: CockroachDB architecture documentation, replication layer, fetched
  2026-09-04]: "the Raft leader is always the range's leaseholder, except briefly during lease
  transfers"; leader leases rely "on a shared, store-wide failure detection"; they "remove the need
  for the single point of failure (SPOF) that was the node liveness range"; default since v25.2;
  the leaseholder serves strongly consistent reads and "bypass[es] Raft; for the leaseholder's
  writes to have been committed in the first place, they must have already achieved consensus". The
  property hecate demands (writer fused with proposer) is, in the design below, true per object:
  the owner is the only proposer of its own registers.
- **Hermes** [A: Katsarakis et al., ASPLOS 2020; abstract via arXiv 2026-09-04]: linearizability with
  "local reads and fully-concurrent fast writes at all replicas", writes "never abort",
  "logical timestamps with cache-coherence-inspired invalidations", "replayable writes" for fault
  tolerance under membership-based reconfiguration; "at 5% writes, the tail latency of Hermes is
  3.6X lower than that of CRAQ and ZAB". Not adopted for slates' records (see §4), recorded as the
  option if local linearizable reads at every holder are ever needed.
- **Paxos Quorum Leases** [A: Moraru, Andersen, Kaminsky, SoCC 2014; read 2026-09-04]: "Quorum
  leases allow a majority of replicas to perform strongly consistent local reads, which
  substantially reduces read latency at those replicas"; Megastore's alternative "grant a read lease
  to all replicas ... all writes now involve at least one round-trip to every replica"; on a
  five-datacenter deployment "over 80% of reads are handled locally ... and over 70% of writes have
  the smallest client-observed latency attainable". Same disposition as Hermes: an option, not
  needed for slates' read pattern.

## 3. Copysets

[A: Cidon et al., "Copysets: Reducing the Frequency of Data Loss in Cloud Storage", USENIX ATC 2013;
abstract via OpenAlex 2026-09-04, 115 citations]: "random replication almost is guaranteed to lose
data in the common scenario of simultaneous node failures due to cluster-wide power outages ...
Copyset Replication presents near optimal tradeoff between the number of nodes which data is
scattered and probability of data loss. For example, a 5000-node RAMCloud cluster under power
outage, Copyset Replication reduces data loss probability from 99.99% to 0.15%. For Facebook's HDFS
cluster, it reduces from 22.8% to 0.78%." The scatter width S is the number of nodes a node's data
is spread over; loss probability grows with the number of distinct copysets, recovery parallelism
grows with S (the body of the paper: verify the formula). RAMCloud's own placement is random with
rack-awareness and refinement (§2), which is what Copysets improves on.

Consequence for slates: per-volume rendezvous placement over the whole region maximises the number
of copysets and must not be used. Each host gets a neighbourhood of S hosts across failure domains,
chosen by the regional group; an object's holders are the owner plus f (content) or 2f (records)
hosts drawn from the owner's neighbourhood by rendezvous. S is derived from the measured
re-replication bandwidth needed to restore f+1 copies of a host's data within the recovery budget,
and from the loss probability the operator accepts.

## 4. The design (the A-6 proposal, revised)

1. **Configuration masters.** One consensus group per region (the Raft dialect of D-14) holds
   membership (fed by SWIM), each host's neighbourhood, each host's configuration epoch, takeover
   decisions, and cross-region homes with a root group across regions. It is touched on membership
   change, takeover, neighbourhood change and home moves, never per write. This is Vertical Paxos'
   "auxiliary configuration master".
2. **Owner as leader-acceptor.** A host owns the volumes it creates or is assigned; every write it
   makes carries its configuration epoch; every holder keeps a per-host highest-epoch-seen and
   refuses lower epochs (the acceptor's `maxBallot` rule; Chubby's sequencer check; BookKeeper's
   fence).
3. **One quorum rule (revised in §9).** Every object has 2f+1 candidate holders from the owner's
   neighbourhood, the owner among them; a write commits at f+1 acknowledgements from any of them;
   the acknowledging set is recorded with the object. Records are sent to all candidates at once;
   content is sent to f+1 and hedged to the rest after the measured p95, so content keeps f+1
   copies plus transient hedges and a straggler can never delay `placed`.
4. **Reads.** Immutable content and immutable chain versions from any holder that has them
   (BookKeeper's read-up-to-LAC rule, with the owner's last-acknowledged version piggybacked on each
   record). The head: at the owner under its lease, or by a majority read when the owner is
   unreachable. No local-read protocol at holders is needed because heads are read at attach and
   at `changed_since`, and the owner's lease answers both in one hop.
5. **Takeover.** The regional group bumps the dead host's epoch and assigns each of its objects to
   the neighbour that already holds it (RAMCloud's recovery masters, with no data movement on the
   critical path because every holder of write-all content has all of it); each new owner runs one
   batched phase-one round per register class across the neighbourhood (Ceph's peering; BookKeeper's
   fence-then-read-LAC), then serves; background re-replication restores the copy counts.
6. **Provisioning.** `create` is local everywhere: random 128-bit id, owner = creator, holders from
   the neighbourhood by rendezvous, the epoch-1 record written to holders asynchronously after the
   local commit, a `placed` flag in the reply, the catalog index eventually consistent through the
   neighbourhood. No metadata store is written per create (the BookKeeper-on-ZooKeeper creation
   bottleneck is avoided by construction).
7. **Multi-region (revised in §9).** Home region per volume (PNUTS record-level mastering);
   regional quorums; every committed record and its content is mirrored to the mirror region
   asynchronously, epoch-ordered, with the lag exposed; the durability scope is chosen per
   operation with `await placed(region | mirror)`, never per volume; cross-region promotion
   through the root group; ownership follows the writer by measured write origin (PNUTS).
8. **Hedging (revised in §9).** Reads from any holder and content puts both use hedged and tied
   requests after the measured p95 (Dean and Barroso); `placed` is the first f+1 acknowledgements
   among the candidates; a holder that is repeatedly late goes on probation and is replaced by the
   regional group; nothing waits for it.
9. **Laptop.** f = 0: every quorum is the owner itself, every write is a local append, the
   configuration master is one self-acknowledging voter. Same code.

## 5. Why this dominates the alternatives for slates

| Design | Hops per write | Per-write cost | Copysets | Takeover | Correctness argument |
|---|---|---|---|---|---|
| Sharded Raft groups (CockroachDB, TiKV) | 2 unless owner = leader | leader log append, follower append and apply, snapshots | bounded by group count | one election per group | Raft |
| Raft group per owner host | 1 | log append, follower apply | bounded | one election per host | Raft |
| Per-volume majority registers, random placement (A-6 v1) | 1 | one record write | unbounded | one round per volume | Paxos per register |
| hecate: proposer fused with a per-session Raft leader | 1 | log append and apply | bounded | one election per session | Raft |
| **Vertical Paxos II with neighbourhoods (this design)** | 1 | one record write; content write-all | bounded by S | one group commit plus one batched round per neighbour, no data movement | Vertical Paxos, with the primary-backup case proven in the paper |

Equal on correctness with every Paxos-family design, strictly fewer hops and no log machinery on
the write path, bounded copysets, zero-copy takeover for content, local creates, and one code path
for N=1. The fused-writer property hecate demands holds per object.

## 6. Downsides that remain, stated plainly

- Content writes complete at all f+1 holders: a straggler delays `placed` until the derived
  detection timeout removes it from the acting set; the agent's own write path is unaffected.
- The catalog is eventually consistent across hosts; the owner serves read-your-writes.
- Two quorum rules (majority for records, all for content) is one more rule than a uniform design.
- Cross-region durability is a per-volume policy, not a fleet-wide guarantee.
- Ownership follows creation; rebalancing is an explicit move through takeover.
- Three protocols to model and simulate: the regional Raft group, the fenced register and ledger,
  and the neighbourhood reconfiguration with joint writes during a holder change.
- The restart-identity invariant (a restarted holder rejoins as a new member and holds nothing)
  is load-bearing and must be a structural test.

## 7. What must be measured

Intra-region and cross-region round trips; per-record write cost; holder acknowledgement latency
distributions (set the majority and write-all behaviours); the detection timeout from measured
RTT variance; S from measured re-replication bandwidth and the accepted loss probability; takeover
time (group commit plus batched phase one) versus the recovery budget; catalog index convergence
time; the fraction of heads read by non-owners (would justify quorum leases or Hermes if it ever
grows); the regional group's commit rate (should be near zero outside failures).

## 8. Items to verify

FaRM's reconfiguration steps and its Zookeeper usage (paper body not fetched); the Copysets
formula for loss probability versus copyset count (abstract only); Hermes' exact reconfiguration
protocol (abstract only). None changes the design; each is a citation to complete in Phase 8.

## 9. The six remainders, solved (2026-09-04, after Ada's review)

1. **A straggler never delays `placed`.** Placement is dynamic within the neighbourhood: every
   object has 2f+1 candidate holders (the owner included); a content put goes to f+1 candidates
   first and is hedged to the remaining candidates after the measured p95 put latency, tied
   requests cancel the loser, and `placed` means f+1 acknowledgements from any candidates; the
   acknowledging set is recorded in the object's record so readers find the copies. Dean and
   Barroso [A: "The Tail at Scale", CACM 2013; read 2026-09-04]: "defer sending a secondary
   request until the first request has been outstanding for more than the 95th-percentile expected
   latency for this class of requests. This approach limits the additional load to approximately
   5% while substantially shortening the latency tail"; the Bigtable example: "sending a hedging
   request after a 10ms delay reduces the 99.9th-percentile latency for retrieving all 1,000 values
   from 1,800ms to 74ms while sending just 2% more requests"; tied requests: "When a request
   begins execution, it sends a cancellation message to its counterpart"; and the technique "is
   also applicable to more-complex coding schemes (such as Reed-Solomon)". Their
   latency-induced probation ("excluding a particularly slow machine, or putting it on probation
   ... the system continues to issue shadow requests") is the healer's straggler bookkeeping. A
   late copy that lands after the hedge won is redundant and reclaimed. The straggler stall
   becomes impossible by construction; only f+1 of 2f+1 candidates being slow at once, a failure,
   engages the detection timeout.
2. **No eventual catalog.** There is no global catalog to be inconsistent. A volume id carries its
   creator host; a lookup by id routes to that host, or, after a takeover, to the successor
   computed by rendezvous over the dead host's neighbourhood from the replicated configuration.
   The answer is always authoritative because it is served by the current owner under its lease.
   A request carrying a stale configuration version is refused with the current one, so one retry
   suffices. Names are scoped to a host and user, so a global name is (host, user, name) and
   resolves the same way. Fleet-wide enumeration is a scatter-gather over owners, each answer
   linearizable at its owner and the whole labelled with the configuration version. Precedents:
   CRUSH "maps objects to devices without relying on a central directory" [A: Weil et al., SC
   2006]; PNUTS routers hold "purely soft state" and a misdirected request "results in a storage
   unit error response, causing the router to retrieve a new copy of the mapping" [A: VLDB 2008];
   RAMCloud clients cache configuration and discover staleness "when it makes a request to a
   server that no longer contains the tablet" [A: SOSP 2011].
3. **One quorum rule.** Every replicated object has 2f+1 candidate holders from the owner's
   neighbourhood, the owner among them; a write commits at f+1 acknowledgements from any of them;
   the acknowledging set is recorded; a read of mutable state contacts f+1 (intersection) unless
   the owner serves it under its lease; a read of immutable content contacts any recorded holder
   and verifies the identity. Records are sent to all 2f+1 at once because they are tiny; content
   is sent to f+1 and hedged, because copies are the cost. That is one rule with a per-class send
   policy derived from measured size and hedge cost, and one proof: read and write quorums
   intersect (Vertical Paxos; Flexible Paxos with W = R = f+1 of 2f+1) [A: Lamport, Malkhi, Zhou
   2009; A: Howard, Malkhi, Spiegelman 2016].
4. **Cross-region durability is consistent, not a policy.** Every write commits at f+1 in its
   home region and is shipped, epoch-ordered, to the mirror region by the same multiplex; the
   mirror lag is measured and exposed per volume. The caller chooses the durability scope per
   operation, not per volume: `await placed(region)` returns at the home commit; `await
   placed(mirror)` returns when the mirror's f+1 have acknowledged and pays one WAN round trip
   for that call only. Synchronous mirroring on every write is rejected by the numbers: Spanner's
   writes cost 14.4 ms even with replicas "less than 1ms" apart, and F1 keeps "2 replicas on the
   west coast of the US, and 3 on the east coast" [A: Corbett et al., OSDI 2012; read 2026-09-04];
   PNUTS: "Synchronously writing to multiple copies around the world can take hundreds of
   milliseconds or more, while the typical latency budget for the database portion of a web
   request is only 50-100 milliseconds" [A: VLDB 2008]; RAMCloud states the same trade [A: OSR
   2009]. CockroachDB's regional tables ("fast in the table's home region and slower in other
   regions") are the same shape without global tables [B: CockroachDB docs, fetched 2026-09-04].
   Region loss promotes the mirror through the root group; the loss window is the mirror lag at
   that moment, zero for every operation that awaited the mirror.
5. **Ownership follows the writer, automatically.** Creation puts the owner where the writer is,
   because the writer's ring to its local owner shard is the latency floor. When write-intent
   attachments arrive from another host and stay, ownership migrates there through the planned
   handoff (auto-seal, ship the delta by identity, epoch bump by the regional group, promotion),
   triggered by measured write origin as PNUTS migrates mastership ("using another hidden metadata
   field in the record that maintains the origin of the last N updates") and as F1 asks Spanner
   "to tell Spanner where to preferentially place Paxos leaders, so as to keep them close to where
   their frontends moved". Load never moves ownership; holder duty is balanced by neighbourhood
   changes. Operators keep an explicit move for maintenance.
6. **Models.** `docs/wip/models/FencedRegister.tla` (fencing, promotion as phase one, majority
   writes, a resumed stale owner, majority reads; invariants TotalOrder, Fencing, ReadSafety,
   Continuity) and `docs/wip/models/Reconfig.tla` (a holder-set change with joint quorums and a
   fresh member replacing a restarted one; invariants ReadSafety, NoLoss), each with a TLC
   configuration. The regional group uses Ongaro's published Raft specification [C: raft.tla,
   CC BY 4.0] plus hecate's conformance suite. The models were written and the tools installed;
   the TLC run was declined in this session, so the models are unchecked until it runs:
   `java -cp tla2tools.jar tlc2.TLC -workers auto -deadlock -config FencedRegister.cfg FencedRegister.tla`.
