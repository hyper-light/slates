# Designing the slates database from scratch: in-memory, async, Arc-free, single-node fast and globally distributed

> **Current contract, A-9 (2026-09-05).** §4.8 requires recovery of acknowledged bytes and references as well as metadata, correct acceptance epochs and safe read authority. The current simulations are not a correctness proof.
> See [the contract review](hecate-contract-review.md) and [the unified design](../SLATES_DESIGN.md).
> The rest of this file is dated research evidence; conflicting recommendations are superseded.

Status: research report (work in progress; sections appended as completed). Date: 2026-09-03.
Author: research agent for the slates project. Evidence tiers: (A) peer-reviewed / thesis; (B) textbook / standard; (C) deployed implementation + design doc / source; (D) blog (gap-filler, flagged).

Terms of art are defined in one line the first time they appear.

## 1. Questions answered

1. Which index structures (ordered vs hash) should back each slates catalog table, and does a thread-per-core single-writer partition design remove synchronization entirely?
2. Which concurrency-control model (partitioned single-writer vs shared MVCC/OCC) fits slates' mostly-per-volume, rarely-cross-volume metadata transactions?
3. How do we reclaim memory safely without `Arc` (epochs, hazard pointers, IBR, Hyaline, DEBRA), what do crossbeam-epoch / seize / haphazard do internally, and what does a per-core epoch scheme cost?
4. What does "durable" mean with no disk (f+1 RAM replicas across failure domains), which replication protocol should carry the data path (primary-backup, chain, consensus), and is an anchor-process / shared-memory design for laptop process-restart survival sound?
5. Which consensus, membership, failure-detection and placement protocols should slates use, how are reads served locally in microseconds, and how does the single-node case degenerate with zero overhead?
6. Which wire format (rkyv / Cap'n Proto / FlatBuffers / SBE), exactly-once mechanism (RIFL), flow control, multiplexing/cancellation, transport security (TLS 1.3 vs Noise) and versioning rules should the protocol adopt?
7. How should the database be tested (deterministic simulation, PCT, Jepsen/Elle, TLA+, fault injection)?

## 2. Findings by sub-question

Citation format: `[tier: source]`; sources are listed in full in §6. "Per-lookup cycles" etc. are the papers' own measurements under the conditions stated next to them.

### 2.1 Index structures

Terms. *Trie / radix tree*: a tree keyed by successive bytes (or bits) of the key, so depth is bounded by key length, not by the number of keys. *ART*: Adaptive Radix Tree, a byte-span trie whose inner nodes change layout with their fan-out. *OLC*: Optimistic Lock Coupling, readers validate a per-node version counter instead of taking read locks; writers take short per-node locks. *ROWEX*: Read-Optimized Write EXclusion, writers lock, readers never lock and never restart. *Bw-tree*: a lock-free B-tree that never updates a node in place; updates are prepended as "delta records" to a per-node chain and reached through an indirection "mapping table". *HOT*: Height Optimized Trie, a trie whose per-node bit span adapts so that every node has close to maximal fan-out. *Cuckoo hashing*: each key has two candidate buckets; inserts displace occupants along a path. *Swiss table*: an open-addressing hash table with a separate byte array of per-slot metadata that is probed with SIMD.

**ART (ordered index, integer- and string-keyed).**
- Four inner-node layouts by fan-out with sizes 52 B (Node4, 2–4 children), 160 B (Node16), 656 B (Node48), 2064 B (Node256), each with a 16 B header; span is 8 bits; path compression and lazy expansion keep depth close to the number of distinguishing bytes [A: Leis, Kemper, Neumann, ICDE 2013, §III].
- Worst-case space is proved bounded at 52 bytes per key for arbitrarily long keys; measured space is "often as low as 8.1 bytes per key" [A: Leis et al. ICDE 2013, §I contributions and §III-G].
- Single-threaded random lookup cost on an Intel Core i7 3930K (3.2 GHz, 12 MB LLC, DDR3-1600): with 65K keys ART needs 40 cycles (dense keys) / 105 cycles (sparse keys) versus 94 for FAST (a read-only SIMD search tree) and 44 for a chained hash table; with 16M keys ART needs 188/352 cycles versus 461 (FAST) and 191 (hash table). The paper's summary: "ART and the hash table have the best performance"; the 10x difference between 65K and 256M keys "is mostly caused by caching effects" [A: Leis et al. ICDE 2013, §V-B, Fig. 10 and Table of performance counters].
- Consequence for slates: for *dense* 64-bit keys (volume ids, inode numbers, sequence numbers) ART is as fast as a hash table and additionally ordered; for *sparse/random* keys (content hashes) it is roughly 2x slower than a hash table at 16M keys, so content hashes should not go in a trie [A: derived from the same table].

**Synchronizing ART: OLC vs ROWEX vs lock-free vs HTM.**
- Hardware: Intel Xeon E5-2687W v3 (Haswell EP, 10 cores / 20 hyper-threads), workload of 50M random 8-byte keys. Per-lookup cycles at 1 thread / 20 threads: no synchronization 211/381; lock coupling (read-write locks) 418/2787; OLC 348/418; ROWEX 375/427; HTM 347/428; Masstree 982/1231. I.e. OLC costs ~65% extra single-threaded (extra instructions) but only ~10% at 20 threads, while classic lock coupling is 7x slower at 20 threads because every read-lock acquisition invalidates the lock's cache line on other cores [A: Leis, Scheibner, Kemper, Neumann, DaMoN 2016, §5.1 and CPU-counter table].
- OLC-ART needs 4.8x fewer instructions and 3.6x fewer L3 misses than Masstree for 8-byte integer keys; Masstree closes the gap for long string keys (URLs of average length 63 bytes) [A: Leis et al. DaMoN 2016, §5.1, §5.3].
- Core-algorithm lines of code (lookup / insert / remove): HTM 30/96/88; lock coupling 41/136/139; OLC 44/148/143; ROWEX 34/200/156 [A: Leis et al. DaMoN 2016, §5.4].
- Under contention (10M dense keys, one lookup thread and one insert+remove thread, Zipf skew up to 83% of operations on one key) ROWEX lookups "stay very high even under extreme contention" because reads never restart; OLC readers can restart repeatedly in pathological cases, so the paper bounds restarts and falls back to write locks to guarantee progress [A: Leis et al. DaMoN 2016, §5.2, §3.2].
- OLC's two correctness obligations: (1) a reader may see a torn node, so a version check must precede dereferencing any optimistically read pointer and loops must be provably finite; (2) a removed node "must not be immediately reclaimed ... because readers might still be active" — the authors use epoch-based reclamation and mark deleted nodes obsolete so writers restart [A: Leis et al. DaMoN 2016, §3.2]. This is the direct link between sub-question 1 and sub-question 3: any shared optimistic index forces a deferred-reclamation scheme.
- OLC generalizes to B+-trees and is the synchronization used for the B+-tree baseline in the Bw-tree study below [A: Leis, Haubenschild, Neumann, IEEE DEB 42(1) 2019 — PDF not retrievable in this session (TLS failure at sites.computer.org); the protocol is the same as DaMoN 2016 §3; A: Wang et al. SIGMOD 2018 §6 uses OLC for its B+Tree and ART].

**Masstree (trie of B+-trees).**
- Structure: layers of B+-trees, each layer indexed by an 8-byte key slice; 15 keys per node with a 64-bit permutation word so inserts do not shift keys; lookups use no locks or interlocked instructions and validate node version numbers; writers take per-node locks; reclamation is RCU-style (epochs) [A: Mao, Kohler, Morris, EuroSys 2012, §4].
- 16-core machine, logging enabled, queries arriving over the network: "more than six million simple queries per second"; 6–10M ops/s on YCSB A–C, more than 30x VoltDB or MongoDB, comparable to memcached [A: Mao et al. EuroSys 2012, abstract, §1].
- In-memory absolute numbers: 8.03M gets/s and 5.78M puts/s (140M keys, 1–10-byte decimal keys, 16 cores); scaling from 1 to 16 cores is 12.7x (get) and 12.5x (put); the missing scaling is DRAM stall per operation rising from 2050 to 2800 cycles [A: Mao et al. EuroSys 2012, §6.2 footnote 5, §6.5].
- A single-core, synchronization-free Masstree beats the concurrent tree "by just 13%" on a single-core put workload — the cost of well-designed optimistic concurrency is small [A: Mao et al. EuroSys 2012, §6.5].
- An open-coded hash table (30% occupancy, superpages) has 2.5x the throughput of Masstree on 8-byte keys; the authors conclude only range queries are "inherently expensive" [A: Mao et al. EuroSys 2012, §6.5 and footnote 6].
- Hard partitioning: 16 single-core Masstree instances versus one shared 16-thread tree, 140M keys, skew parameter δ where one of 16 partitions receives δx the load of the others; "while partitioning works well for some workloads, sharing data among all cores works better for others" — the shared tree wins once the load is skewed (Fig. 11) [A: Mao et al. EuroSys 2012, §6.6].

**Bw-tree and its critique.**
- Bw-tree design: mapping table (logical node id -> physical pointer) so a single CAS installs a delta record or a consolidated node; no in-place updates; structure modifications are multi-step and use a help-along protocol; used by Hekaton [A: Levandoski, Lomet, Sengupta, ICDE 2013, via A: Wang et al. SIGMOD 2018 §2; A: Diaconu et al. SIGMOD 2013 §4].
- OpenBw-Tree (a faithful, optimized reimplementation) is 1.1–2.5x faster than the original design but "is still slower than its competitors except the SkipList": ART is "more than 4x faster ... for point lookups" and Masstree and an OLC B+Tree are faster "often by a factor of ~2x"; causes are higher instruction counts and cache misses from delta-chain traversal and mapping-table indirection. Hardware: 2x Intel Xeon E5-2680 v2, 128 GB, 20 worker threads pinned to one socket, YCSB A/C/E with mono-int, rand-int and email keys [A: Wang, Pavlo, Mu, Levandoski, Kaminsky, SIGMOD 2018, §6, Fig. 13–14].
- Under a high-contention insert workload (Mono-HC) the OpenBw-Tree abort rate is 1078.63% (aborts per successful operation) versus 1.05–1.44% for the low-contention key patterns; "Masstree has the best result, followed by ART and then B+Tree" under high contention [A: Wang et al. SIGMOD 2018, Table 2 and §6].
- Skip list (lock-free "No Hot Spot" variant): "high variation and low performance" in the same comparison — the slowest structure measured [A: Wang et al. SIGMOD 2018, §6]. Skip lists trade cache locality for implementation simplicity; there is no evidence in favour of them for an in-memory index in these studies.
- Epoch-based GC scaling: enrolling in an epoch by incrementing a shared counter "becomes bottleneck when there are many threads"; the fix is per-thread epoch state (decentralized epochs) — 1.3x improvement at 20 threads for Mono-Int [A: Wang et al. SIGMOD 2018, §4.2, §5.2 Fig. 10].

**HOT.**
- Idea: vary the number of bits considered per node so that fan-out stays near a maximum k=32; nodes are SIMD-searchable and compact; result: HOT "outperforms other state-of-the-art index structures for string keys both in terms of search performance and memory footprint, while being competitive for integer keys" [A: Binna, Zangerle, Pichl, Specht, Leis, SIGMOD 2018 abstract; the TODS 2022 extension is ACM-paywalled; specific throughput numbers were not extracted in this session — flagged].
- Synchronization: HOT uses ROWEX-style writer locks with lock-free readers (the same family as ART's ROWEX) [A: Binna et al. SIGMOD 2018 §4 as cited by Leis et al.; flagged as not re-read].

**Hash indexes.**
- libcuckoo (optimistic concurrent cuckoo hashing): 2-way set-associative buckets with 4 slots, BFS search for the shortest cuckoo path, readers validate per-bucket version counters (no locks), writers use lock striping; 16-core machine: "almost 40 million insert and more than 70 million lookup operations per second"; 2.5x Intel TBB concurrent_hash_map on write-heavy workloads "while using less than half of the memory for 64 bit key/value pairs"; 4-way buckets reach >90% occupancy [A: Li, Andersen, Kaminsky, Freedman, EuroSys 2014, abstract, §1, §3].
- Maier, Sanders, Dementiev: a lock-free linear-probing table extended with growing, deletion and non-word-sized types is "an order of magnitude faster than the best more general tables" and up to "four orders of magnitude" faster under extreme contention; growing costs about what it costs sequentially [A: Maier, Sanders, Dementiev, ACM TOPC 5(4) 2019, abstract; local extract available as maier.txt].
- Swiss tables: one control byte per slot (1 state bit + 7-bit H2 hash fragment) held in a separate dense array; groups of 16 control bytes are matched with SSE instructions so "very deep probe chains" are cheap; H1 selects the group, H2 filters candidates before key comparison [C: Abseil "Swiss Tables Design Notes"]. Rust's std HashMap is hashbrown, a port of the same design, "around 2x faster than the previous standard library HashMap" [C: hashbrown README].
- Relevance: slates' content-address index is keyed by a uniformly random hash; no hashing step is needed (use hash bits directly as H1/H2), so an open-addressing SIMD-probed table has a single expected cache miss per lookup at 7/8 load — strictly better than any ordered structure for this key type [A: Leis ICDE 2013 sparse-key result; C: Abseil design notes].

**Thread-per-core, single-writer partitions: what the evidence says.**
- The only measured cost of *shared* optimistic indexes is ~10–13% at 16–20 threads (ART-OLC 418 vs 381 cycles/lookup; Masstree 13%) [A: Leis DaMoN 2016; A: Mao EuroSys 2012]. Partitioning therefore does not buy raw index throughput; it buys (a) zero atomics and zero reclamation machinery, (b) deterministic, single-threaded code that is far easier to test, and (c) freedom from the abort storms that lock-free structures show under contention (Bw-tree 1078% aborts) [A: Wang SIGMOD 2018].
- Partitioning loses when load is skewed across partitions (Masstree Fig. 11) and when operations cross partitions (Silo: Partitioned-Store is 1.54x faster than shared Silo at 0% cross-partition transactions, breaks even at ~20%, and is 2.98x slower at 60%) [A: Mao EuroSys 2012 §6.6; A: Tu et al. SOSP 2013 §5.4 Fig. 8].
- slates' key space is naturally partitionable: every filesystem operation names exactly one volume, and a volume's directory tree, inode table and accounting are touched together. Cross-partition operations (clone across owners, global chunk refcounts) are rare or can be made asynchronous. This is the H-Store regime (<20% multi-partition) where partitioned single-writer execution wins [A: Yu et al. VLDB 2014 §5, Table 2].

**Per-use recommendation (evidence-backed).**

| Catalog table | Key shape | Access pattern | Structure | Why |
|---|---|---|---|---|
| Volume catalog (id -> record) | dense u64 | point | ART (single-writer, no sync) or direct-indexed slab | dense keys: ART == hash speed, ordered for listing [A: Leis ICDE'13] |
| Volume by name (owner, name) | short string | point + prefix listing | ART with path compression | strings, prefix scans, 8.1–52 B/key [A: Leis ICDE'13] |
| Directory entries (dir_ino, name) -> ino | u64 \|\| bytes | point + ordered readdir | ART (HOT if profiling shows long names dominate) | ordered by name for readdir; 4x faster than Bw-tree, 2x faster than B+tree for point lookups [A: Wang SIGMOD'18] |
| Inode table (ino -> attrs) | dense u64 | point | slab/array indexed by ino (generation-checked) | O(1), no tree needed; ART only if ids are sparse |
| Chunk index (hash -> location, refcount/epoch) | 256-bit random | point | Swiss-table style open addressing keyed by hash bits, sharded by hash prefix across cores | random keys favour hashing 2x over tries [A: Leis ICDE'13]; SIMD probing [C: Abseil] |
| Lineage DAG (parent -> children) | u64 pairs | point + small scans | ART on (parent_id, child_id) | ordered composite key gives children-of scan for free |
| Leases / attachments (agent -> volume, expiry) | u64 + timestamp | point + expiry order | ART on agent id + hierarchical timing wheel for expiry | expiry needs time order, not a general index |
| Operations log | monotonic u64 | append + range replay | contiguous chunked ring (Vec of fixed blocks) | monotonic keys need no index; ART on u64 only for sparse lookups |
| Cluster membership / placement | small | rare | plain sorted Vec | tens of entries; no index |

Runner-ups and why they lost: B+-tree with OLC (2x slower than ART for point lookups but competitive for scans — acceptable second choice for directory entries) [A: Wang SIGMOD'18]; Masstree (3x more instructions than ART on short keys; wins only on long shared-prefix strings) [A: Leis DaMoN'16]; Bw-tree (slowest of the trees, abort storms) [A: Wang SIGMOD'18]; skip list (slowest overall) [A: Wang SIGMOD'18]; cuckoo hashing (great for a *shared* multi-writer table; unnecessary once each shard is single-writer, and Swiss-style probing is simpler) [A: Li EuroSys'14].

Must-measure: ART vs Swiss-table lookup latency on the actual per-core shard sizes (the crossover is cache-size dependent: 65K-key indexes are ~10x faster than 256M-key ones on the same code) [A: Leis ICDE'13 §V-B].

### 2.2 Concurrency control for metadata transactions

Terms. *OCC*: optimistic concurrency control, execute without locks, validate the read set at commit. *MVCC*: multi-version concurrency control, readers see a consistent snapshot of older versions. *Epoch (Silo sense)*: a coarse time period (tens of ms) that orders commits for durability and reclamation without a global counter. *Partitioned single-writer*: H-Store style, one thread owns a partition and runs its transactions serially. *Deterministic execution (Calvin)*: all replicas agree on the input order first, then execute without further coordination.

**Silo (epoch-based OCC on a shared Masstree).**
- Commit protocol: lock the write set (in global address order), take an epoch snapshot, validate every read record's TID and every scanned node's version, then install writes; reads never write shared memory [A: Tu, Zheng, Kohler, Liskov, Madden, SOSP 2013, §4].
- Epochs advance every 40 ms ("shorter epochs would also work"); TIDs embed the epoch so that logging, group commit and snapshots are ordered by epoch, not by a global counter [A: Tu et al. SOSP 2013, §4.1].
- Throughput: "almost 700,000 transactions per second on a standard TPC-C workload mix on a 32-core machine" (4x 8-core Xeon E7-4830, 256 GB), ~22,000 tx/s/core; per-core throughput at 32 cores is 91% of one core [A: Tu et al. SOSP 2013, abstract, §1, §5].
- Partitioned-Store (H-Store-like variant on the same code) versus shared-memory Silo, TPC-C with varying cross-partition fraction: Partitioned-Store is 1.54x faster with no cross-partition transactions, drops below Silo at "roughly 20%", and at 60% Silo is 2.98x faster [A: Tu et al. SOSP 2013, §5.4, Fig. 8].

**TicToc.** Timestamps are derived from the data read/written ("data-driven"), removing the centralized timestamp allocator; the ACM page was not fetchable (HTTP 405) so numbers are not reproduced here — flagged [A: Yu, Pavlo, Sanchez, Devadas, SIGMOD 2016; not extracted].

**Cicada (multi-version OCC with per-thread clocks).**
- Timestamps come from per-thread `rdtsc`-based clocks that are loosely synchronized (no global counter); versions are inlined into records when small; garbage collection runs eagerly; a global backoff ("contention regulation") prevents abort collapse [A: Lim, Kaminsky, Andersen, SIGMOD 2017, §3].
- On a 28-core machine: 2.07M TPC-C tx/s and 56.5M YCSB tx/s; 3x higher throughput than the next-fastest scheme on contended TPC-C and 1.37x on contended YCSB; at least 5.54% faster on uncontended TPC-C [A: Lim et al. SIGMOD 2017, abstract, §1].
- The paper's stated lesson relevant here: partition-per-core designs "excel under easily-partitionable workloads, but their performance rapidly [degrades]" with cross-partition work; Cicada targets the general case [A: Lim et al. SIGMOD 2017, §2].

**Hekaton (SQL Server in-memory engine).**
- Latch-free hash and Bw-tree indexes, optimistic MVCC with commit-time validation, cooperative epoch-based garbage collection by worker threads [A: Diaconu et al. SIGMOD 2013, §2.1, §4, §6, §8].
- Partitioning was considered and rejected: with per-core partitions a lookup on a non-partitioning secondary index must be sent to every partition, so the design "is not sufficiently robust for the wide variety of workloads" [A: Diaconu et al. SIGMOD 2013, §2.1].

**Wu et al., empirical MVCC evaluation.**
- 4-socket Xeon E7-4820 (40 cores, 128 GB); protocols MVTO, MVOCC, MV2PL, SSI; version storage append-only (O2N/N2O), time-travel, delta; GC tuple-level vs transaction-level, background vs cooperative; index pointers logical vs physical; epochs of 40 ms for memory management [A: Wu, Arulraj, Lin, Xian, Pavlo, VLDB 2017, §7].
- Findings: MVTO "works well on a variety of workloads" (and no surveyed system uses it); "transaction-level GC provided the best performance with the smallest memory footprint"; logical index pointers gave 45% higher throughput under updates; at high thread counts "the main bottleneck ... is the cache coherence traffic from updating the memory manager's counters and checking for conflicts" [A: Wu et al. VLDB 2017, §7, §8].

**Yu et al., "Staring into the Abyss" (1024 simulated cores).**
- Seven schemes on Graphite (tiled in-order cores, 1 GHz): "all algorithms fail to scale" but for different reasons — 2PL: lock thrashing; T/O family (TIMESTAMP, MVCC, OCC): the timestamp allocator (atomic add) becomes the bottleneck, with batching, CPU clocks and hardware counters as mitigations; memory allocation is a hidden bottleneck; OCC pays for local copies and aborts [A: Yu, Bezerra, Pavlo, Devadas, Stonebraker, VLDB 2014, §4.3, §5, Table 2].
- H-STORE (partition-level locking, one thread per partition) is "the best algorithm for partitioned workloads", performing best overall on TPC-C "even with ~12% multi-partition transactions", and "outperforms other approaches when less than 20%" of the workload is multi-partition; it "suffers from multi-partition transactions and timestamp bottleneck" [A: Yu et al. VLDB 2014, §5.5–§5.6, Table 2].

**H-Store / VoltDB.**
- Single-threaded execution per partition, no locking or latching, no buffer pool, no persistent redo log (durability via K-safety replication instead); "a factor of 82 faster on TPC-C" than a commercial RDBMS on 2007 hardware [A: Stonebraker, Madden, Abadi, Harizopoulos, Hachem, Helland, VLDB 2007, §1, §3, §5].

**Calvin (deterministic execution).**
- A sequencer layer batches transaction inputs (10 ms epochs) and replicates the batches (Paxos or async); a scheduler acquires locks strictly in log order, so all replicas execute the same serial-equivalent schedule without a distributed commit protocol; dependent transactions use a reconnaissance read (OLLP) [A: Thomson, Diamond, Weng, Ren, Shao, Abadi, SIGMOD 2012, §3–§4].
- "half a million TPC-C transactions per second on a cluster of commodity machines" (100 EC2 nodes) [A: Thomson et al. SIGMOD 2012, §1, §6].

**Decision for slates: partitioned single-writer execution with deterministic cross-partition ordering.**
- Workload fit: provisioning and metadata operations are one-shot (all parameters known up front), short (bounded path-length work), and touch one volume; that is exactly the H-Store/Calvin sweet spot and outside the regime where shared OCC/MVCC pays off (>20% cross-partition or long interactive transactions) [A: Tu SOSP'13 Fig. 8; A: Yu VLDB'14 Table 2; A: Stonebraker VLDB'07].
- Cross-partition operations (clone into another owner's quota, chunk refcount changes across hash shards, global rename between volumes if ever allowed): assign them a sequence number from the node's replicated log and execute them at each involved partition in log order (Calvin's deterministic lock order collapses to "execute in log order" because each partition is single-threaded). Read-set discovery before sequencing (OLLP) is unnecessary because slates operations name their volumes explicitly [A: Thomson SIGMOD'12 §3.2].
- Bounded-work rule: a single-writer partition is only as good as its longest operation (H-STORE's weakness is a stalled partition); every operation must be O(log n) or chunked into cooperative steps (e.g. destroy of a million-file volume is a background sweep with yields, not one transaction) [A: Yu VLDB'14 §5.6; A: Stonebraker VLDB'07].
- Snapshot/clone cost: with a persistent (copy-on-write) directory tree, snapshot and clone are O(1) at the root plus one lineage edge, so "clone creating N entries" never occurs as a metadata transaction; this removes the main argument for multi-key OCC.
- Async integration: each core runs one executor; a partition is a task that drains an inbound SPSC queue of operations; an operation never awaits while the partition's structures are mid-update (all awaits happen in the network/RPC layer before or after the partition step), preserving the single-writer invariant without locks.
- Timestamps: no global counter. Per-core sequence numbers plus the replicated-log index order everything visible to clients; Cicada's loosely-synchronized `rdtsc` clocks are the fallback only for cross-core ordering of lease expiry, and Yu et al.'s measurement shows a shared atomic counter would be the first thing to saturate at high core counts [A: Lim SIGMOD'17 §3; A: Yu VLDB'14 §4.3].
- Runner-ups: Silo-style epoch OCC on shared OLC indexes (wins if per-volume skew makes some partitions hot; keep as the documented escape hatch — Masstree's data says the shared design costs ~10–13% and tolerates skew) [A: Tu SOSP'13; A: Mao EuroSys'12 §6.6]; Cicada-style MVCC (best general-purpose numbers, but versions + GC machinery add allocation-aware complexity that slates' short operations do not need) [A: Lim SIGMOD'17; A: Wu VLDB'17 §8 on GC coupling].

### 2.3 Memory reclamation without Arc

Terms. *Safe memory reclamation (SMR)*: deciding when a node unlinked from a shared structure can be freed although lock-free readers may still hold a pointer to it. *Grace period*: an interval after which no pre-existing reader can still reference the node. *QSBR*: quiescent-state-based reclamation, each thread periodically announces it holds no references (e.g. at the top of its event loop). *EBR*: epoch-based reclamation, a global epoch counter; a thread announces the epoch when it enters a critical region; garbage retired in epoch e is freed once every active thread has observed e+1 (three limbo lists). *Hazard pointers (HP)*: a thread publishes each pointer it is about to dereference; reclaimers scan all published pointers. *IBR*: interval-based reclamation, each block records birth and retire epochs and each thread reserves an epoch interval, so a stalled thread reserves a bounded set. *Hyaline*: reference counts kept only on batches of retired nodes, so readers pay nothing and any thread can free any batch.

**Costs and guarantees from the literature.**
- Fence and atomic costs on the machines Hart et al. used: a memory fence 78 ns (156 cycles) on a 2.0 GHz PowerPC G5 and 76 ns (110 cycles) on a 1.45 GHz POWER4+; CAS 52/59 ns; a lock 231/243 ns. HP needs one fence per visited node, EBR two fences per operation (enter/exit), LFRC per-node fences plus counter atomics, and "since QSBR has no per-operation fences, its per-operation overhead can be very low" [A: Hart, McKenney, Brown, Walpole, IPDPS 2006 / JPDC 2007, §3.1, §4.2 Table]. The paper's caveats: QSBR and EBR are blocking (a thread that fails or sleeps inside a critical region stalls reclamation for everyone) and the scheme's cost depends strongly on the data structure, workload and preemption [A: Hart et al., §2.1, §7].
- Fraser's EBR: global epoch count; each process observes the epoch on entering a critical region; a limbo list populated two epochs ago is reclaimed once all processes have observed the current epoch; the epoch advances only when "all processes within a critical region have seen the current epoch" — so one slow process inside a critical region blocks the advance [A: Fraser, PhD thesis UCAM-CL-TR-579, 2004, §5.2.3].
- Hazard pointers: bounded garbage (at most H = total hazard pointers unreclaimable at any time, amortized scans over batches of R > H retired nodes) at the price of a store-load fence per protected pointer and the requirement that every hazardous access be identified [A: Michael, IEEE TPDS 15(6) 2004 — not re-read this session; costs corroborated by A: Hart 2006 §2.1.3 and A: Brown PODC 2015 §2].
- DEBRA (distributed EBR): per-thread epoch announcements, amortized epoch checks, per-thread limbo bags; measured "on average 4% slower, and at worst 21% slower" than no reclamation at all, and it "outperforms a highly efficient implementation of hazard pointers by an average of 75%"; DEBRA+ adds POSIX signals that neutralize a stalled thread's critical section (the signalled thread restarts its operation) so that crashed or descheduled processes "can only prevent a bounded number of records from being reclaimed", at +2.5% average overhead [A: Brown, PODC 2015, abstract, §1, §5].
- IBR: threads reserve an epoch *interval*; a block is freed when its (birth, retire) interval does not overlap any reservation; this "avoids the possibility that a single stalled thread may reserve an unbounded number of blocks" while, unlike HP, it "avoids a memory fence on most pointer-following operations"; space per thread is a small constant [A: Wen, Izraelevitz, Cai, Beadle, Scott, PPoPP 2018, abstract, §1, Table 1].
- Hyaline: reference counting only during reclamation (per batch), asynchronous freeing by whichever thread drops the last reference, so the reclamation load is balanced instead of landing on writer threads; "steadily outperforms EBR by 10% in one test and yields 2x gains in oversubscribed scenarios"; the robust variant Hyaline-S adopts IBR/hazard-era "birth eras" to bound memory under stalled threads [A: Nikolaev, Ravindran, PLDI 2021, abstract, §1, §4].
- The Bw-tree study's practical finding: a centralized epoch counter that every operation increments "becomes bottleneck when there are many threads"; decentralized per-thread epochs fixed it (1.3x at 20 threads) [A: Wang et al. SIGMOD 2018, §4.2, §5.2].

**What the Rust crates actually do (sources read this session).**
- crossbeam-epoch. `pub struct Collector { pub(crate) global: Arc<Global> }` — the handle *is* an `Arc`; `Global { locals: List<Local>, queue: Queue<SealedBag>, epoch: CachePadded<AtomicEpoch> }`; each `Local` stores `collector: UnsafeCell<ManuallyDrop<Collector>>`, i.e. an `Arc` clone per registered thread, plus its own `CachePadded<AtomicEpoch>`, `Bag`, `guard_count`, `handle_count`, `pin_count`. Constants: `MAX_OBJECTS = 64` per bag, `PINNINGS_BETWEEN_COLLECT = 128`, `COLLECT_STEPS = 8`. `Local::pin()` publishes the epoch with a SeqCst fence, implemented on x86 as a `compare_exchange` (`lock cmpxchg`) because it benchmarked faster than `mfence`; `Global::try_advance()` walks the intrusive list of all `Local`s and advances only if every pinned participant is in the current epoch; the default collector is `static COLLECTOR: OnceLock<Collector>` with a `thread_local! HANDLE: LocalHandle` unregistered on thread exit [C: crossbeam-epoch `collector.rs`, `internal.rs`, `default.rs`, master as of 2026-09]. Verdict: `Arc` is used for collector lifetime only (one clone per thread, never per operation); the per-operation cost is one full fence at pin plus an amortized scan of all threads at collect. The `Arc` is avoidable in principle (a `'static` collector) but crossbeam's API does not offer that.
- seize. `#[repr(transparent)] pub struct Collector { raw: raw::Collector }`; `raw::Collector { batches: ThreadLocal<CachePadded<UnsafeCell<LocalBatch>>>, reservations: ThreadLocal<CachePadded<Reservation>>, id: usize, batch_size: usize }` (batch_size default 32). No `Arc` anywhere: each retired *batch* carries an `AtomicUsize active` reference count; `try_retire` issues a "heavy barrier" (SeqCst fence), scans the active threads' reservation heads, links the batch into each active thread's reservation list and sets `active`; the batch frees when `active` reaches zero. This is Hyaline; `LocalGuard` is `!Send`, `OwnedGuard` is `Send + Sync`; thread slots grow dynamically [C: seize `src/collector.rs`, `src/raw/collector.rs`, master as of 2026-09].
- haphazard. `pub struct Domain<F> { hazptrs: HazPtrRecords, untagged: [RetiredList; NUM_SHARDS], due_time: AtomicU64, nbulk_reclaims: AtomicUsize, count: AtomicIsize, shutdown: bool }`; no `Arc`; hazard-pointer records are `Box`ed once and recycled through an available list; retired objects go to 8 sharded lock-free lists; bulk reclaim triggers at `RCOUNT_THRESHOLD = 1000` retired objects, or 2x the number of hazard pointers, or ~2 s; a `static SHARED_DOMAIN` provides the global domain (a port of Folly's hazard pointers) [C: haphazard `src/domain.rs`, main as of 2026-09].

**What a per-core scheme for a thread-per-core server looks like.**
- First-order answer: in the partitioned single-writer design of §2.2 the owning core is the only thread that ever dereferences a partition's nodes, so unlinking and freeing are the same instruction; no SMR scheme, no fence, no `Arc`. Cross-core access is by message, not by pointer [A: partitioning premise from Stonebraker VLDB 2007 §4 and Yu VLDB 2014 §2].
- Where shared read-mostly structures remain (cluster map, placement table, chunk-location table if reads must avoid a hop): use QSBR with the event loop as the quiescent point. Each core owns a cache-padded `AtomicU64` "loop generation" that it stores (Release) once per executor iteration; a retiring core snapshots the vector of generations (Acquire loads, N cores) and frees a retired node once every core's generation exceeds its snapshot. Cost: one plain store per loop iteration per core and zero fences on the read path — the cheapest point in Hart's measurements [A: Hart 2006 §3.1]. Garbage is bounded by (retire rate x longest loop iteration), and because an async executor never blocks inside an iteration, the "stalled thread" failure mode of QSBR/EBR is reduced to a long CPU-bound task, which the bounded-work rule of §2.2 already forbids [A: Hart 2006 §2.1; A: Fraser 2004 §5.2.3].
- If a truly preemptible or oversubscribed environment must be supported (the SDK's client-side caches, not the server), use the seize (Hyaline) crate: no `Arc`, balanced reclamation, and robust variants; its per-retire SeqCst fence is paid by writers, not readers [C: seize sources; A: Nikolaev PLDI 2021].
- Rust-specific enforcement without `Arc`: a guard type borrows the per-core context (`Guard<'core>`), is `!Send`, and cannot outlive the loop iteration, so the compiler proves that no cross-core pointer escapes; retired nodes are pushed into a per-core `Vec` tagged with the generation snapshot (no allocation on the hot path). This is DEBRA's "distributed epochs" specialized to one epoch per core [A: Brown PODC 2015 §3].

Decision: no SMR inside partitions; per-core QSBR (loop-generation vector) for the few shared read-mostly tables; seize only in client-side or oversubscribed contexts. Runner-ups: crossbeam-epoch (forbidden by the no-`Arc` rule and pays a fence per pin), hazard pointers (a fence per dereference — 76–78 ns on Hart's hardware — is the wrong trade for read-heavy index traversals), IBR/Hyaline-S (right answer only if stalls are possible, which the executor design rules out).

Must-measure: fence cost on the target CPUs (Apple M-series, Graviton, x86) — the numbers above are 2006 POWER/G5 numbers and only the ordering (QSBR < EBR < HP) is expected to carry over [A: Hart 2006 §4.2].

### 2.4 Durability with no disk

Terms. *Durability (classic)*: a committed write survives crashes of the system. *f+1 replication*: f+1 copies so that any f failures leave one copy. *Failure domain*: a set of components that fail together (host, rack, power feed, availability zone). *Primary-backup*: one replica orders writes and forwards them to backups. *Chain replication*: replicas in a fixed order; writes enter at the head and are acknowledged by the tail. *Consensus-replicated log*: 2f+1 replicas, majority acknowledgement.

**What deployed RAM-resident systems actually do.**
- RAMCloud rejects DRAM replication for durability: "Replicating all data in DRAM would have solved some availability issues, but with 3x replication this would have tripled the cost and energy usage"; it keeps one copy in DRAM and the redundant copies "on disk or flash, which is both cheaper and more durable than DRAM", and recovers a crashed server's 35 GB in 1.6 s on a 60-node cluster by scattering log segments across hundreds of backups; reads of small objects take 5–10 µs over Infiniband [A: Ousterhout et al. SOSP 2011 ("Fast Crash Recovery in RAMCloud"), abstract, §1, §2; A: Ousterhout et al. ACM TOCS 33(3) 2015 — same design, PDF not retrievable this session].
- FaRM keeps data in DRAM with primary-backup replication and makes the DRAM effectively non-volatile with batteries and SSD write-back on power loss ("a new, inexpensive approach to providing non-volatile DRAM"); 140M TATP transactions/s on 90 machines (4.9 TB), recovery from a failure in under 50 ms; the NSDI'14 system did 160M key-value lookups/s at 31 µs on 20 machines [A: Dragojević et al. SOSP 2015, abstract; A: Dragojević et al. NSDI 2014, abstract via MSR publication page].
- Hermes: a *membership-based* protocol (a reliable membership service with leases decides who is alive) rather than majority-based; writes are broadcast with invalidations and logical timestamps, every replica serves linearizable reads locally, any replica can replay an incomplete write after a failure; it beats RDMA-optimized ZAB and CRAQ in throughput at all write ratios and its tail latency at 5% writes is at least 3.6x lower, on five RDMA replicas [A: Katsarakis et al. ASPLOS 2020, abstract, §1, §3].
- Chain replication: a chain of t+1 servers tolerates t failures; all updates go to the head and all queries to the tail; a Paxos-replicated master detects failures and reconfigures the chain; throughput is competitive with primary-backup and recovery is simpler [A: van Renesse, Schneider, OSDI 2004, §2–§4, §5]. CRAQ lets every node answer reads (clean version served locally; a dirty version triggers a version query to the tail), raising read throughput by ~200% for 3-node chains and 600% for 7-node chains versus CR under read-mostly load; membership via ZooKeeper [A: Terrace, Freedman, USENIX ATC 2009, §1, §3].

**Exact meaning of "durable" for slates.** With no disk, a commit can only promise: the record is present in the RAM of at least f+1 processes placed in distinct failure domains, and the system stays available and loses nothing under any f *independent* failures. It cannot promise survival of correlated failures (a power event, a kernel/driver bug, a bad deploy, an operator `kill -9` sweep) — the precise reason RAMCloud went to disk and FaRM added batteries [A: Ousterhout SOSP 2011 §1; A: Dragojević SOSP 2015 abstract]. Consequently:
- The product-level guarantee should be stated as "f-fault-tolerant volatile durability" plus "explicit archive export" as the only real durability; the archive export is what a user must run before trusting data beyond the cluster's lifetime.
- Failure domains must be *declared and enforced by placement* (host, then rack/zone), or f+1 copies on one host are worth one copy [A: CRUSH's failure-domain hierarchy, Weil et al. SC 2006; A: FaRM/RAMCloud placement rules].
- Acknowledgement semantics: a write is acknowledged only after f+1 replicas have it in RAM *and* the membership service agrees those replicas are members of the current configuration (Hermes' "reliable membership", CR's master) — otherwise a partitioned replica's acknowledgement is worthless [A: Katsarakis ASPLOS 2020 §3; A: van Renesse OSDI 2004 §3].
- Data path choice: primary-backup / chain needs only f+1 copies and a small external consensus group for membership; consensus on the data path needs 2f+1 copies of everything. For an all-RAM store the memory cost of 2f+1 versus f+1 is decisive (RAMCloud's argument about 3x replication cost applies to any extra copy). Hence: metadata shards replicate with a primary-backup log to f+1 replicas, and membership/configuration/placement live in one consensus group (the design of RAMCloud's coordinator, CR's master, FaRM's configuration manager, Hermes' membership service). §2.5 refines which log protocol.

**The laptop case: an anchor process holding state in shared memory.**
Precedents for handing live resources across process restarts:
- systemd's file-descriptor store: a service pushes fds with `FDSTORE=1`; systemd keeps duplicates and hands them back on restart, including after crashes; the documentation recommends serializing state into a `memfd` and storing that fd, so that a service restarts "without losing execution context" [C: systemd.io "File Descriptor Store"].
- `memfd_create`: an anonymous RAM-backed file referenced only by fds, passable over Unix sockets (SCM_RIGHTS) or by inheritance, sealable (`F_SEAL_WRITE/SHRINK/GROW`), hugepage-capable, freed when the last reference is dropped [B: Linux man-pages memfd_create(2)].
- nginx: `USR2` starts a new master that inherits listening sockets through the `NGINX` environment variable, `WINCH` retires old workers, `QUIT` retires the old master, `HUP` to the old master rolls back [C: nginx.org "Controlling nginx"].
- Envoy hot restart: listen sockets are passed to the new process over a Unix domain socket; counters and gauges are copied across; existing connections are *not* transferred, they drain (`--drain-time-s`, `--parent-shutdown-time-s`); not supported on Windows [C: Envoy docs "Hot restart"].
- HAProxy seamless reload: the old process hands its listening sockets to the new one over the stats socket (`expose-fd listeners`) so no connection is refused during reload [D: HAProxy blog "Truly seamless reloads", fetch blocked this session — flagged].
- Microreboot: separate process recovery from data recovery by moving important state into dedicated state stores with their own crash-safety; component reboots are then an order of magnitude faster and lose an order of magnitude less work than a full restart [A: Candea, Kawamoto, Fujiki, Friedman, Fox, OSDI 2004, abstract, §2].

Evaluation of the anchor-process design (state lives in a memfd/shm segment owned by a tiny supervisor; the server maps it; a crashed or upgraded server remaps it):
- What the precedents *do* share across restarts: fds (sockets, memfds) and simple counters. What they *do not* share: live, pointer-rich data structures. Envoy explicitly copies stats rather than mapping the old heap; systemd's guidance is "serialize your state into a memfd" [C: Envoy docs; C: systemd.io].
- The hazard is consistency, not mechanism: if the server dies mid-update, a live-mapped heap is in an arbitrary state, and a memory-corruption bug that caused the crash may already have corrupted the "durable" segment — Microreboot's argument for putting state behind a store with defined semantics [A: Candea OSDI 2004 §2].
- Two designs that keep the segment always-consistent: (i) *log-in-shm*: the replicated operations log (same record format as the cluster WAL, checksummed, append-only) lives in the segment; the server's indexes are private and rebuilt by replay on restart, exactly RAMCloud's "log is the truth, indexes are rebuilt" recovery; (ii) *CoW root-switch*: because slates' catalog is copy-on-write, a commit is one root-pointer store; keep the arena in the segment with a header {generation, root, checksum} written last; recovery uses the last valid root and reclaims unreachable arena blocks with a sweep. (i) is simpler and shares code with replication; (ii) gives faster restart but couples the segment to the in-memory layout (an upgrade must understand the old layout).
- Verdict: yes — adopt the anchor process with design (i), plus an optional periodic snapshot (the CoW root serialized into a second memfd) to bound replay time. Portability: Linux `memfd_create` + SCM_RIGHTS; macOS `shm_open`/`mmap` + SCM_RIGHTS over a Unix socket (no memfd, no sealing); Windows named section objects (`CreateFileMapping`) with handle duplication into the supervisor — the sealing and fd-store conveniences are Linux-only, so the design must treat sealing as an optimization [B: memfd_create(2); C: Envoy's "not supported on Windows" caveat shows the platform gap is real].
- What it buys and what it does not: survival of server crashes, panics and binary upgrades on one machine with sub-second replay; nothing against a machine reboot or power loss (RAM is gone) — so the laptop's durability statement is "process-restart survival + explicit archive export", and the 1-replica log in the memfd is literally the f=0 case of the cluster's f+1 RAM replication.

Must-measure: replay throughput (records/s) and snapshot cost on real catalog sizes; the recovery-time budget decides the snapshot interval (RAMCloud sized its recovery to 1–2 s for "continuous availability") [A: Ousterhout SOSP 2011 §1].


### 2.5 Consensus, membership, placement, and the single-node degeneration

Terms. *Raft*: leader-based replicated log; a majority acknowledges each entry [A: Ongaro & Ousterhout ATC'14]. *Joint consensus*: membership change that requires majorities of both the old and the new configuration [A: Ongaro thesis 2014]. *PreVote / CheckQuorum*: a candidate first asks whether it could win, and a leader steps down when it loses contact with a majority, so partitioned nodes do not disrupt a working cluster [A: Ongaro thesis §9.6; A: Howard & Mortier PaPoC'20]. *ReadIndex*: the leader confirms leadership with a heartbeat round before serving a linearizable read [A: Ongaro thesis §6.4]. *Lease read*: the leader serves reads without a round while its lease (bounded by clock assumptions) is valid [A: Ongaro thesis §6.4.1; A: Spanner OSDI'12]. *Flexible Paxos*: only the intersection of the leader-election quorum and the replication quorum must be non-empty, so a replication quorum can be one node when the election quorum is everyone [A: Howard, Malkhi, Spiegelman 2016]. *Rendezvous / jump consistent hashing*: placement functions that move a minimal share of keys when nodes change [A: Thaler & Ravishankar 1998; A: Lamping & Veach 2014].

Decided (with the hecate prior confirmed rather than assumed):
- **Log protocol for metadata shards**: a Raft core in the "CockroachDB-lineage dialect" hecate ratified after its etcd challenge and layer-classification dossier: a pure algorithmic state machine (`step(Message) -> {outbound, to_append, state_delta}`), logical ticks, injected randomness, entries-then-hardstate, no message before covering durable state, PreVote and CheckQuorum always on, ReadIndex reads, explicit configuration-change activation with the etcd #12359 countermeasures, exemplars never dependencies, and the known bug record shipped as an executable conformance suite [C: survey-hecate.md §3.1]. hyperscale's per-job Raft groups (10,000 groups ticked by one loop, quorum from configured size, volatile logs) are the negative example; its AD-52 concepts (deterministic bootstrap list, learners, fence header, tombstones, ephemeral node ids) are adopted [C: survey-hyperscale.md §2, §8.1].
- **"Durable" with no disk**: a durable append means f+1 in-RAM copies on distinct failure domains (primary-backup fan-out to backups selected by the placement map) plus consensus only for the placement/membership/configuration group (RAMCloud coordinator / chain-replication master / FaRM configuration manager shape) [A: Ousterhout SOSP'11; A: van Renesse & Schneider OSDI'04; A: Dragojević SOSP'15]. The metadata shard's own log is replicated primary-backup to f+1 replicas with the leader as sequencer; the shard's *leadership and configuration* are decided by the consensus group; a write is acknowledged when f+1 replicas hold it in RAM and the membership epoch is current (Hermes' "reliable membership" argument) [A: Katsarakis ASPLOS'20].
- **Reads in microseconds**: local reads on the shard owner are served from its in-memory state under a leader lease whose length derives from the measured heartbeat RTT distribution and a clock-drift bound measured between replicas (not a wall-clock axiom: the lease is expressed in the follower's monotonic clock from the moment it granted the vote, as CockroachDB's and Spanner's leases are) [A: Ongaro thesis §6.4.1; A: Corbett OSDI'12]; when the lease is unavailable (just elected, clock-drift check failed) the shard falls back to ReadIndex; snapshot reads by any replica are served from its immutable epoch without coordination.
- **Membership and failure detection**: SWIM with Lifeguard's local-health multiplier and the suspicion formula `max − (max−min)·log(C+1)/log(K+1)`, peer confirmation before suspicion, and gossip piggyback bounded by per-path MTU, exactly as hyperscale built it, with every parameter derived from measured RTT, loss, and convergence (`survey-hyperscale.md` §1.8, §8.4) [A: Das et al. DSN'02; A: Dadgar et al. 2018]; phi-accrual per edge as the suspicion threshold once enough samples exist [A: Hayashibara et al. SRDS'04]; Rapid's stable-membership idea (multi-process cut detection) is the documented upgrade path if churn storms are observed [A: Suresh et al. ATC'18].
- **Placement**: rendezvous (highest-random-weight) hashing over the failure-domain tree with weights from measured free memory per node, so a volume's f+1 replicas land on distinct domains and adding a node moves 1/N of volumes; CRUSH-style hierarchical selection is the same idea with an explicit tree [A: Thaler & Ravishankar 1998; A: Weil et al. SC'06]; jump consistent hash is the runner-up (minimal state, but no weights or hierarchy) [A: Lamping & Veach 2014]; Slicer's key-range assignment is the shape for the metadata shard map when volumes are grouped [A: Adya et al. OSDI'16].
- **Single-node degeneration with zero overhead**: the failure-domain tree has one node, so the placement function returns one replica, the log's replication quorum is the leader itself (Flexible Paxos with an election quorum of one), the consensus group is one voter that self-acks, and SWIM has no peers to probe; the same code runs every branch with N=1 (hecate's "loud degenerate": `R_eff = 1` is announced, never silent) [C: survey-hecate.md §1.2; A: Howard et al. 2016]. On a laptop the log lives in the anchor process's shared-memory segment (§2.4), which is literally the f=0 replica.

### 2.6 Wire protocol

- **Framing and zero-copy**: fixed-layout, little-endian, 8-byte-aligned headers with the length checked before any allocation on both ends (vorpal's wire crate discipline) [C: survey-vorpal.md §5.4]; canonical encoding ("one value, one encoding") with a schema hash in the envelope and append-only evolution enforced at compile time (hecate-wire) [C: survey-hecate.md §4.6]. Among zero-copy libraries, FlatBuffers and Cap'n Proto validate on access (Cap'n Proto bounds-checks every pointer at read; FlatBuffers has an optional verifier), SBE is fixed-layout with generated accessors, and rkyv validates a whole archive up front with `bytecheck` [C: Cap'n Proto encoding spec; C: FlatBuffers docs; C: SBE spec; C: rkyv docs]. slates' messages are small and fixed-layout, so a hand-encoded header plus a derive-generated canonical body (the hecate model) is chosen; rkyv is the runner-up for large structured payloads (manifests), used only behind full validation.
- **Exactly-once for provisioning**: RIFL's recipe: a client id plus a per-client sequence number identifies each RPC; the server keeps a completion record with the result until the client acknowledges (lease-bounded), so retries return the original result rather than re-executing [A: Lee et al. SOSP'15]; hyperscale's `{client}:{sequence}:{nonce}` key with PENDING coalescing is the same idea [C: survey-hyperscale.md §3]. The completion record lives with the volume's owner shard and replicates with its log.
- **Flow control**: credit-based, absolute-offset windows per stream (Kung's credit scheme; HTTP/2's flow-control windows are the modern instance), windows derived from measured bandwidth-delay product and the latency budget per message class; no class's latency bound may contain a term from another class's queue depth [A: Kung, Blackwell, Chapman SIGCOMM'94; B: RFC 9113 §5.2; C: survey-hecate.md §4.5].
- **Multiplexing, cancellation, deadlines**: request ids distinct from trace ids; cancellation is a message that guarantees a terminal completion; every request carries a deadline expressed as a remaining budget, never an absolute wall time across hosts (hyperscale's cross-host monotonic-timestamp bug) [C: survey-hyperscale.md §8.5].
- **Security**: TLS 1.3 (RFC 8446) via rustls is the default between hosts because it is the audited implementation with certificate infrastructure operators already run; Noise (IKpsk2) is the documented alternative hecate chose for its private QUIC and is adopted here only if a measured handshake or per-message cost of TLS proves decisive; on one host, peer credentials (§2.6 of the IPC note) replace transport encryption.
- **Versioning**: protocol major in the frame header; append-only fields within a major; cross-version decode by ancestor schema hash, never tolerant reading; test vectors frozen per major [C: survey-hecate.md §4.6; C: survey-vorpal.md §5.1].

### 2.7 Testing the database

- Deterministic simulation: FoundationDB runs the whole cluster in one process with a simulated network, disks and clocks, injecting faults from a seed, and reports that most bugs are found this way before production [A: Zhou et al. SIGMOD'21]; the Rust ecosystem's `turmoil` (simulated network for tokio) and `madsim` (deterministic runtime) show the shape; slates has its own runtime, so it ships its own simulation driver (`low-latency-ipc-and-runtime.md` §2.3) that replaces time, randomness, the network and the rings with seeded simulations, and a nemesis library (kill, pause, partition, drop, duplicate, reorder, clock jump, replica loss, slow replica, memory pressure).
- Schedule exploration: PCT gives probabilistic guarantees of finding bugs of a given depth by randomizing priorities [A: Burckhardt et al. ASPLOS'10]; `shuttle` implements it for Rust; `loom` exhaustively explores interleavings of small lock-free cores under the C11 memory model [A: Norris & Demsky OOPSLA'13].
- Consistency checking: Jepsen's methodology (nemeses plus history checkers) and Elle's cycle-based anomaly detection over transaction histories [A: Kingsbury & Alvaro VLDB'20]; linearizability checking of the volume catalog's history per Herlihy-Wing with a P-compositional checker [A: Herlihy & Wing TOPLAS'90; A: Horn & Kroening CAV'15].
- Formal model: a TLA+ specification of the replication and lease protocol checked with TLC for small configurations (Ongaro's Raft spec as the starting point) [A: Ongaro thesis; A: Newcombe et al. CACM'15].
- Fault-injection findings that set priorities: most catastrophic failures come from mishandled error paths that a simple test would catch [A: Yuan et al. OSDI'14]; network partitions cause a large share of cloud failures and most are caught by single-partition tests [A: Alquraan et al. OSDI'18]; gray and metastable failures require load-dependent injection [A: Huang et al. HotOS'17; A: Bronson et al. HotOS'21].

## 3. Measured numbers table

| Value | Conditions | Source (tier) |
|---|---|---|
| ART lookup 40/105 cycles (65K keys) and 188/352 (16M) vs hash table 44/191 | Core i7-3930K | Leis et al. ICDE'13 (A) |
| OLC ART 348/418 cycles at 1/20 threads vs lock coupling 418/2787 | Xeon E5-2687W v3 | Leis et al. DaMoN'16 (A) |
| Masstree 8.03M gets/s, 5.78M puts/s, 140M keys, 16 cores | 2012 server | Mao et al. EuroSys'12 (A) |
| OpenBw-Tree: ART >4x faster point lookups; Masstree/B+tree ~2x faster | 2x Xeon E5-2680 v2 | Wang et al. SIGMOD'18 (A) |
| libcuckoo ~40M inserts/s, >70M lookups/s on 16 cores | 2014 server | Li et al. EuroSys'14 (A) |
| Silo ~700K TPC-C tx/s on 32 cores; Partitioned-Store 1.54x at 0% cross-partition, breakeven ~20%, 2.98x slower at 60% | 4x Xeon E7-4830 | Tu et al. SOSP'13 (A) |
| Cicada 2.07M TPC-C tx/s, 56.5M YCSB tx/s on 28 cores | 2017 server | Lim et al. SIGMOD'17 (A) |
| H-STORE best when < 20% multi-partition (VLDB'14 simulation to 1024 cores) | Graphite | Yu et al. VLDB'14 (A) |
| Memory fence 76-78 ns; CAS 52-59 ns; lock 231-243 ns | POWER4+/G5, 2006 | Hart et al. (A) |
| DEBRA 4% avg / 21% worst slower than no reclamation; 75% faster than HP | 2015 | Brown PODC'15 (A) |
| Hyaline +10% over EBR, 2x oversubscribed | 2021 | Nikolaev & Ravindran PLDI'21 (A) |
| crossbeam-epoch: `Arc<Global>` per collector handle; pin = SeqCst fence; 64 objects/bag; collect every 128 pins | crate source | crossbeam-epoch (C) |
| seize: no `Arc`; batch refcount; SeqCst fence per retire batch (32) | crate source | seize (C) |
| RAMCloud recovery of 35 GB in 1.6 s on 60 nodes; 5-10 µs reads | Infiniband cluster | Ousterhout et al. SOSP'11 (A) |
| FaRM 140M TATP tx/s on 90 machines; failover < 50 ms | 2015 cluster | Dragojević et al. SOSP'15 (A) |
| Hermes tail latency ≥ 3.6x lower than ZAB/CRAQ at 5% writes | 5 RDMA replicas | Katsarakis et al. ASPLOS'20 (A) |
| CRAQ read throughput +200% (3 nodes) to +600% (7 nodes) vs CR | 2009 | Terrace & Freedman ATC'09 (A) |

## 4. Recommendation for slates

1. **Indexes**: per the table in §2.1 (ART for dense/ordered keys and names; slab by inode number; Swiss-style hash for content addresses sharded by hash prefix; timing wheel for expiries; contiguous ring for the log). Runner-ups as stated there.
2. **Transactions**: partitioned single-writer execution; cross-partition operations sequenced through the node's log and executed in log order; bounded work per operation (cooperative chunking for large destroys/clones-with-materialization). Runner-ups: Silo-style OCC on shared OLC indexes (escape hatch for skew), Cicada MVCC.
3. **Reclamation**: none inside partitions; per-core QSBR keyed on the executor's loop generation for cross-shard read-mostly tables; seize only in oversubscribed client-side contexts.
4. **Replication and consensus**: primary-backup logs to f+1 in-RAM replicas across declared failure domains for metadata shards; one Raft consensus group (hecate dialect, executable bug-record conformance suite) for membership, configuration and placement; leader leases from measured clocks for microsecond local reads, ReadIndex fallback; SWIM/Lifeguard membership with derived parameters; rendezvous hashing over the failure-domain tree for placement; the laptop is N=1 of the same formulas.
5. **Anchor process**: yes, with the log-in-shared-memory design and optional periodic root snapshots; Linux memfd + fd passing, macOS shm_open + fd passing, Windows named sections + handle duplication.
6. **Wire**: hand-encoded fixed-layout header, canonical derive-generated bodies with schema hash, append-only evolution, RIFL-style completion records for exactly-once provisioning, credit flow control with derived windows, TLS 1.3 between hosts and peer credentials on one host.
7. **Testing**: deterministic cluster simulation on our own driver with a nemesis library; loom for lock-free cores; shuttle/PCT for task schedules; linearizability and Elle checks over histories; a TLA+ model of leases and replication; fault injection prioritized by the OSDI'14/'18 findings.

## 5. Risks, unknowns, must-measure

- Fence and CAS costs on Apple M-series and Graviton; ART vs Swiss-table crossovers at real shard sizes; replay throughput and snapshot cost for the anchor segment; heartbeat RTT distributions and inter-replica clock drift (they size leases); rendezvous weight inputs (free memory per node).
- The primary-backup data path plus separate consensus group is more machinery than "Raft for everything"; the memory argument (f+1 vs 2f+1 copies of every volume's metadata) is what justifies it; if metadata turns out to be small relative to content, the simpler "Raft per shard" is the documented fallback.
- TicToc and HOT numbers were not extracted this session; they do not change the decision.

## 6. Bibliography

- V. Leis et al. ICDE 2013; DaMoN 2016; IEEE DEB 2019 (as cited above).
- Y. Mao, E. Kohler, R. Morris. EuroSys 2012. https://pdos.csail.mit.edu/papers/masstree:eurosys12.pdf
- J. Levandoski, D. Lomet, S. Sengupta. The Bw-Tree. ICDE 2013; Z. Wang et al. Building a Bw-Tree Takes More Than Just Buzz Words. SIGMOD 2018. https://db.cs.cmu.edu/papers/2018/mod342-wangA.pdf
- R. Binna et al. HOT. SIGMOD 2018.
- X. Li et al. EuroSys 2014; T. Maier et al. TOPC 2019; Abseil Swiss Tables; hashbrown.
- S. Tu, W. Zheng, E. Kohler, B. Liskov, S. Madden. Speedy Transactions in Multicore In-Memory Databases (Silo). SOSP 2013. https://people.csail.mit.edu/stephentu/papers/silo.pdf
- X. Yu, A. Pavlo, D. Sanchez, S. Devadas. TicToc. SIGMOD 2016. https://people.csail.mit.edu/sanchez/papers/2016.tictoc.sigmod.pdf
- H. Lim, M. Kaminsky, D. G. Andersen. Cicada. SIGMOD 2017. https://www.cs.cmu.edu/~hl/papers/cicada-sigmod2017.pdf
- C. Diaconu et al. Hekaton. SIGMOD 2013. https://www.microsoft.com/en-us/research/publication/hekaton-sql-servers-memory-optimized-oltp-engine/
- Y. Wu, J. Arulraj, J. Lin, R. Xian, A. Pavlo. An Empirical Evaluation of In-Memory MVCC. VLDB 2017. https://www.vldb.org/pvldb/vol10/p781-wu.pdf
- X. Yu, G. Bezerra, A. Pavlo, S. Devadas, M. Stonebraker. Staring into the Abyss. VLDB 2014. https://www.vldb.org/pvldb/vol8/p209-yu.pdf
- M. Stonebraker et al. The End of an Architectural Era. VLDB 2007. https://www.vldb.org/conf/2007/papers/industrial/p1150-stonebraker.pdf
- A. Thomson et al. Calvin. SIGMOD 2012. https://cs.yale.edu/homes/thomson/publications/calvin-sigmod12.pdf
- K. Fraser. Practical Lock-Freedom. PhD thesis, Cambridge UCAM-CL-TR-579, 2004. https://www.cl.cam.ac.uk/techreports/UCAM-CL-TR-579.pdf
- T. Hart, P. McKenney, A. Brown, J. Walpole. IPDPS 2006 / JPDC 2007; M. Michael. Hazard Pointers. IEEE TPDS 2004; T. Brown. DEBRA. PODC 2015. https://www.cs.toronto.edu/~tabrown/debra/paper.podc15.pdf ; H. Wen et al. IBR. PPoPP 2018; R. Nikolaev, B. Ravindran. Hyaline. PLDI 2021.
- crossbeam-epoch, seize, haphazard sources (2026-09). https://github.com/crossbeam-rs/crossbeam ; https://github.com/ibraheemdev/seize ; https://github.com/jonhoo/haphazard
- D. Ongaro, J. Ousterhout. In Search of an Understandable Consensus Algorithm. ATC 2014; D. Ongaro. Consensus: Bridging Theory and Practice. PhD thesis, Stanford 2014. https://web.stanford.edu/~ouster/cgi-bin/papers/OngaroPhD.pdf
- H. Howard, R. Mortier. Paxos vs Raft. PaPoC 2020. https://arxiv.org/abs/2004.05074 ; H. Howard, D. Malkhi, A. Spiegelman. Flexible Paxos. 2016. https://arxiv.org/abs/1608.06696
- T. Chandra, R. Griesemer, J. Redstone. Paxos Made Live. PODC 2007; I. Moraru, D. Andersen, M. Kaminsky. EPaxos. SOSP 2013; S. Sutra et al. EPaxos Revisited. NSDI 2021.
- J. Corbett et al. Spanner. OSDI 2012. https://www.usenix.org/conference/osdi12/technical-sessions/presentation/corbett
- A. Das, I. Gupta, A. Motivala. SWIM. DSN 2002; A. Dadgar, J. Phillips, J. Currey. Lifeguard. 2018. https://arxiv.org/abs/1707.00788 ; L. Suresh et al. Rapid. ATC 2018; N. Hayashibara et al. The φ Accrual Failure Detector. SRDS 2004.
- D. Karger et al. Consistent Hashing. STOC 1997; D. Thaler, C. Ravishankar. Using Name-Based Mappings to Increase Hit Rates. IEEE/ACM ToN 1998; J. Lamping, E. Veach. Jump Consistent Hash. 2014. https://arxiv.org/abs/1406.2294 ; A. Adya et al. Slicer. OSDI 2016; S. Weil et al. CRUSH. SC 2006.
- J. Ousterhout et al. The RAMCloud Storage System. ACM TOCS 2015; D. Ongaro et al. Fast Crash Recovery in RAMCloud. SOSP 2011. https://web.stanford.edu/~ouster/cgi-bin/papers/ramcloud-recovery.pdf
- A. Dragojević et al. FaRM. NSDI 2014; No Compromises. SOSP 2015. https://www.microsoft.com/en-us/research/publication/no-compromises-distributed-transactions-with-consistency-availability-and-performance/
- A. Katsarakis et al. Hermes. ASPLOS 2020. https://arxiv.org/abs/2001.09804
- R. van Renesse, F. Schneider. Chain Replication. OSDI 2004; J. Terrace, M. Freedman. CRAQ. ATC 2009.
- G. Candea et al. Microreboot. OSDI 2004; systemd File Descriptor Store; nginx Controlling; Envoy Hot restart docs.
- C. Lee et al. Implementing Linearizability at Large Scale and Low Latency (RIFL). SOSP 2015. https://web.stanford.edu/~ouster/cgi-bin/papers/rifl.pdf
- H. T. Kung, T. Blackwell, A. Chapman. Credit-Based Flow Control for ATM Networks. SIGCOMM 1994; RFC 9113 (HTTP/2) §5.2; RFC 8446 (TLS 1.3); Noise Protocol Framework.
- Cap'n Proto encoding; FlatBuffers; SBE; rkyv documentation.
- J. Zhou et al. FoundationDB: A Distributed Unbundled Transactional Key Value Store. SIGMOD 2021; S. Burckhardt et al. PCT. ASPLOS 2010; B. Norris, B. Demsky. CDSChecker. OOPSLA 2013; K. Kingsbury, P. Alvaro. Elle. VLDB 2020; M. Herlihy, J. Wing. Linearizability. TOPLAS 1990; A. Horn, D. Kroening. Faster Linearizability Checking via P-Compositionality. CAV 2015; C. Newcombe et al. How Amazon Web Services Uses Formal Methods. CACM 2015; D. Yuan et al. Simple Testing Can Prevent Most Critical Failures. OSDI 2014; A. Alquraan et al. An Analysis of Network-Partitioning Failures in Cloud Systems. OSDI 2018; P. Huang et al. Gray Failure. HotOS 2017; N. Bronson et al. Metastable Failures. HotOS 2021.

## 7. The EdenFS/Mononoke replication model, and what it changes (2026-09-04)

**What Meta actually does.** EdenFS keeps each checkout's mutable state in a local overlay that is
process-crash safe only ("If the process dies, none of the user's data should be lost", explicitly
not power loss or disk failure), never replicated [C: EdenFS InodeStorage.md]. Durability comes from
committing: commits, trees and blobs are immutable, content-addressed objects served by Mononoke
[C: EdenFS Data_Model.md; C: Mononoke README]. Mononoke's blobstore is a multiplex over N
underlying stores: a put first logs the key in a write-ahead log ("Log the blobstore key and wait
till it succeeds"), then issues puts to all stores and returns success "as soon as `quorum.write`
of these operations were successful"; failed stores are recorded and a background healer replays
the WAL to bring them up to date; the read quorum is `num_stores - write_quorum + 1`, a read
returns as soon as any store has the blob and reports absence only when `quorum.read` stores agree
[C: sapling eden/mononoke/blobstore/multiplexedblob_wal/src/multiplex.rs, fetched 2026-09-04].
Bookmarks, the only mutable pointers, live in a strongly consistent SQL store with a bookmark
update log (from memory; the crate listing fetched this session did not surface the directory,
so this sentence is flagged for verification). Commit Cloud backs up commits automatically and
syncs workspaces between machines (Meta's Sapling announcement; the documentation page was not
reachable this session and this is flagged) [D: Meta engineering 2022].

**The three state classes and their mechanisms.**

| State | Mutability | Meta's mechanism | slates' mechanism (Amendment A-2) |
|---|---|---|---|
| Sealed content: chunks, manifests, snapshots | immutable, content-addressed | WAL-first quorum multiplex (W of N) with a healer; reads any-of | identical: WAL-first put to the W = f+1 placements chosen by rendezvous over the failure-domain tree; healer replays the WAL; reads from any holder; anti-entropy by Merkle manifests |
| Pointers: which snapshot is a volume's head, who owns the lease and with which epoch, placement, membership | small, mutable, must be linearizable | bookmarks in a strongly consistent store | the one Raft consensus group (hecate dialect) |
| Live working state: open extents, unsealed writes, directory nodes born this epoch | large, mutable, single owner | local overlay, not replicated; durability by committing | owner-local (in the anchor segment on a laptop); durability by auto-seal into a snapshot at a cadence derived from the measured mutation rate and the operator's loss-window SLO, then quorum placement of the sealed content; opt-in live op-log shipping for volumes that demand a shorter window |

**Why this is the better shape for slates.** Every operation that matters for correctness across
machines (which snapshot is head, who may write) is a pointer update, and pointer updates are
tiny and rare compared with content; putting them through one consensus group costs nothing
measurable. Content needs no ordering at all: a chunk either exists at its identity or it does
not, so W-of-N puts and any-of reads are sufficient, and the healer makes them eventually complete
(Mononoke runs this at Meta scale). Live state is the only class where a per-write replication
decision is expensive, and EdenFS's model, refined by CitC's snapshot-on-every-save and Commit
Cloud's automatic backup, says: do not replicate live writes, seal often and replicate the seals.
This removes the primary-backup shard log from the default path (D-14) and with it the memory of
remote copies of open extents, the replica-lag backpressure on the write path, and the "Raft per
shard" fallback. It keeps the anchor segment (the laptop's f=0 durability) and makes the fleet's
durability statement precise: "a volume's sealed snapshots are f-fault-tolerant; its live edits
since the last seal are lost with the owner unless the volume opted into live shipping, and the
seal cadence bounds that loss window to a measured, per-volume number".

**Acknowledgement semantics under A-2.** `create`, `snapshot`, `clone`, `destroy`, `attach`,
`detach`, lease changes: acknowledged when the pointer commits in the consensus group (one
round trip in a fleet, in-process on a laptop) and, for snapshot, when the sealed content reaches
W stores (the seal itself is asynchronous; the snapshot id is returned immediately and carries a
`placed` flag that flips when W holds it; a caller who needs durability awaits `placed`). Writes to
open files: acknowledged when applied on the owner (and logged in the anchor segment); no remote
copy by default.

**Owner loss under A-2.** The consensus group expires the owner's lease, promotes the latest
`placed` snapshot as the volume's head on a new owner chosen by placement, and reissues the lease
with epoch + 1; live edits since the last placed seal are lost and reported as such (the loss
window). Volumes with live shipping recover to the shipped log instead.

**Derived constants added.** Auto-seal cadence = the interval that keeps the expected loss
window (measured mutation bytes per second × interval) under the operator's SLO, bounded below by
the measured seal cost so sealing never dominates; W = f+1 from the declared failure-domain tree
(1 on a laptop); healer scan cadence derived from the measured put-failure rate.

**Runner-ups.** The current D-14 (primary-backup f+1 log per shard) remains available as the
opt-in live-shipping policy; full Raft per shard is dropped (2f+1 copies and consensus on the data
path for no benefit once live state is owner-local).
