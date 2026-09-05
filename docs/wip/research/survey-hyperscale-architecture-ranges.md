# Hyperscale `docs/architecture.md` range surveys (helper-agent output, verbatim)

These are the raw reports of helper agents that each read a contiguous line range of
`/Users/adalundhe/Projects/hyperscale/docs/architecture.md` in full. They are kept verbatim as
evidence for `survey-hyperscale.md`. Every claim cites `docs/architecture.md:LINE`.

Ranges covered: 8406–14200 (gate leadership, context consistency, zombies, backpressure, stats),
15016–20164 (federated health, workflow state machine, adaptive timeouts, Vivaldi, routing),
20164–31380 (WAL/ledger/HLC/VSR/single-writer buffers), 31380–38790 (idempotency, resource guards,
retry budgets, SLO routing, spillover, adaptive route learning).

---

## Range 8406–14200

### 0. Execution model
Managers classify workflows as test/non-test via `HookType.TEST` (8474–8477), allocate cores from the TOTAL pool by priority (LOW ≤25%, NORMAL 25–75%, HIGH 75–100%, EXCLUSIVE 100%, AUTO 1–100%; 9479–9483), split VUs evenly with remainder to the last thread (9554–9560), pick workers "via crypto-random (avoid bias)" (8447), require peer-manager quorum before dispatch (8453–8455), and run BFS dependency layers sequentially (9622–9627, 9855–9888).
A workflow is complete only when `WorkflowFinalResult` arrives; `WorkflowProgress` is monitoring-only (10349–10355). Cores are freed in the worker's `finally` regardless of whether the result send succeeded, to prevent core leaks (10266–10282).

### 1. Gate per-job leadership: ring, leases, fencing, reconnection (11543–12137)
Why: a single cluster-leader model bottlenecks at high job volume, so each job gets its own leader gate chosen by consistent hashing (11554–11555). Five components, all marked IMPLEMENTED (11584–11588).

Consistent hash ring
- Ring space 0..2^32−1; job hashed onto it; owner = first gate clockwise, backup = next clockwise (11607–11610).
- `ConsistentHashRing(virtual_nodes=150)` (11647). `add_node` inserts 150 keys `hash(f"{node_id}:{i}") % 2**32` and re-sorts (11654–11657); `remove_node` filters + re-sorts (11659–11663); `get_node` bisects, wraps to index 0, raises `NoGatesAvailable` on an empty ring (11665–11673); `get_nodes(key, count=2)` walks clockwise collecting distinct node ids (11675–11687).
- Why: adding/removing a gate moves ~1/N of jobs; deterministic so any node, including the client, computes ownership with no coordination; natural load balancing (11634–11639).
- (helper's note) The hash is Python's builtin `hash()` (11655, 11669), which is per-process randomized for strings; the cross-node determinism claim (11637) requires a stable hash in a Rust port.

Lease-based ownership
- Why: the hash gives the INITIAL owner, the lease confirms ACTIVE ownership; on owner failure the lease expires and the backup claims; "prevents split-brain: only one lease holder at a time" (11705–11710).
- Lifecycle: CLAIMED (fence assigned) → ACTIVE (renewing) → EXPIRED (backup may claim) → CLAIMED by backup with fence+1 (11715–11731).
- `GateJobLease{job_id, owner_node_id, fence_token, lease_acquired=time.monotonic(), lease_duration=30.0, backup_node_id}` (11737–11744). `is_expired` = monotonic now > acquired + duration (11747–11748); `renew()` resets `lease_acquired` (11750–11751).
- Claim on first submission: owner/backup from `get_nodes(job_id, 2)`, `fence_token=1` (11756–11770).
- `claim_expired_lease` returns None unless the lease exists, is expired, and the caller is the recorded backup; new fence = old+1, new backup = `get_nodes(job_id, 3)[2:]` (11772–11790).
- Renewal loop: every `lease_duration/3` (10 s) renew each lease this node owns that has NOT already expired (11795–11802) — a lapsed owner does not self-renew.
- Zombie section restates: lease duration "configurable per-job"; expiry lets other gates take over (12969–12973).
- (helper's note) The range never shows the lease record propagating to the backup; `_job_leases` is a local dict (11769, 11789) and expiry is judged by each node's local monotonic clock.

Fencing tokens
- Why: reject stale updates from an old owner after transfer; "simple, proven pattern (ZooKeeper, etcd)"; no consensus, just monotonic comparison (12006–12009).
- Generation: job created with fence_token=1; every ownership transfer increments (12014–12015, 11785); also incremented on retry, failover, reassignment (12998–13002).
- Carried in JobDispatch, JobFinalResult, JobStatusPush (12024–12042) and `WorkflowDispatch.fence_token` ("Fencing token for at-most-once", 8745).
- Validation (12047–12058): no lease → reject (unknown job); received < current → reject (stale); received > current → accept AND adopt it (`_update_lease_from_newer_token`, "might be from new owner we don't know yet"); equal → accept.
- Stale scenario: DC result with fence=1 hits new owner at fence=2 → rejected; DC retries with the updated fence (12063–12069). Split-brain: a partitioned old owner's fence=1 update is rejected; it "learns it's not owner anymore, stops processing" (12074–12080).
- Gate result handler checks `_owns_job(job_id, fence_token)`; if not owner, recomputes owner from ring, forwards, returns `b'forwarded'` (11869–11878). Managers send `JobFinalResult` straight to `job_info.origin_gate` stored at dispatch (11854–11864); if the ring changed, the old owner forwards and "fence token prevents duplicates" (11883–11890). Why direct routing: fewer hops, lower latency, less load on the cluster leader (11821–11824).
- (helper's note) "Accept any higher token" favours liveness over safety: a receiver adopts a claimant's token without verification.

Delivery guarantees
- Result acceptance is at-most-once per ownership epoch via fencing (8745, 11890), but execution can repeat: worker death (SWIM DEAD) → retry on a different worker, failed worker excluded, FAILED after max retries (10592–10595); workflow error → NO RETRY, error is final (10547–10551, 10587–10590). Rationale: worker failure means work never completed; a workflow error means work completed with an error, so retrying is futile (10597–10599). NO_RESULT (timeout) is treated as failure (10627–10632, 10643).
- Leader failure mid-layer may lose in-flight context; worst case the current layer is re-executed, "idempotent workflows help" (11460–11466) — i.e. at-least-once for execution.
- Progress deduplication keyed by `layer_version` (11519).

Client reconnection
- Why: the client keeps the same ring as the gates, so after a disconnect it knows where to reconnect without asking "who owns my job?" (11908–11912). Ring node ids are `"host:port"` (11923–11925); submit goes to `get_node(job_id)` (11927–11937).
- `reconnect_to_job(job_id, max_retries=3)`: compute owner, send `register_callback`; on ConnectionError/TimeoutError or unsuccessful response sleep `LEASE_DURATION/2` (15 s) then retry; raise `ReconnectFailed` after 3 attempts (11942–11962) → up to 45 s of waiting.
- Gates push `RingUpdate` add/remove to clients (11967–11974). Timeline: crash at t=5, reconnect to the new owner at t=21 (11979–11987).

### 2. Context consistency protocol (10731–11539)
Race (10928–10955): a manager stores context, broadcasts asynchronously, advances the layer, and dispatches a dependent workflow to a worker on a peer that has not yet received the context → stale/missing context.

Alternatives and stated rejections (10963–11074): Redis async replication may lose writes on failover (10975–10976); Raft would need consensus per key update (latency) and an unbounded log (10991–10992); HLC needs synchronized clocks (11008); Cassandra LWW suffers wall-clock skew (11024); vector clocks push merges to the app and grow with writers (11039–11040); CRDTs are type-limited and "eventually" is too slow (11055–11056); pure single-writer makes the leader a bottleneck/SPOF (11072). Matrix at 11084–11106.

Chosen hybrid (11120–11140): single job leader as source of truth + quorum confirmation before advancing (from Raft); QUORUM consistency + LWW for edge cases (Cassandra); context embedded in dispatch + version number (Spanner snapshot reads); single writer per job (Kafka). Key insight: layer N+1 can only depend on layers ≤ N, so layers are natural sync points (11138–11140).

Protocol (11156–11172, 11206–11314)
1. Job leader is the single writer for the job's context; no conflicts by construction (11156–11158).
2. Workers report to their own manager; a non-leader forwards `ContextForward{job_id, workflow_id, context_updates, context_timestamps, source_manager}` to the leader, which alone applies (11216–11231, 11325–11331).
3. Leader applies per key with LWW: timestamp = the worker's per-key Lamport stamp, defaulting to the leader's `_context_lamport_clock`; `source_node` = leader; clock += 1 per batch (11241–11257).
4. Layer completion: leader asserts leadership, increments `_job_layer_version`, snapshots the FULL context into `ContextLayerSync{job_id, layer_version, context_snapshot, source_node_id}`, broadcasts, and `if confirmations < self._quorum_size: raise QuorumTimeoutError`; only then dispatches the next layer (11265–11289). `_quorum_size` is never given a value in this range.
5. Peers reply `ContextLayerSyncAck{job_id, layer_version, applied}` with `applied=False` if stale (11341–11346). Dispatch carries `context_version` and `dependency_context` restricted to declared dependencies (11297–11310, 8745–8747).
- Earlier design in the Manager section: `ContextUpdate{job_id, workflow_name, context_values, source_manager, lamport_clock}` applied only when `lamport_clock > current` (9929–9989); stored values are `key → (value, timestamp)` (9660–9662); `_context_clock: job_id → {workflow_name: lamport}` (9764–9766).
- Manager state: `_job_contexts`, `_job_layer_version` (monotonic per job), `_job_leaders` (set at job acceptance), `_context_lamport_clock` (11180–11193).

Conflict resolution (11358–11386): under a write lock accept if no existing stamp, or new > existing, or equal stamps and `source_node > existing_src` (string-order tiebreak); otherwise reject as stale. "With single-writer (job leader), conflicts should not occur. LWW is defensive programming for edge cases (leader failover, etc.)".

Guarantees (11398–11416): ORDERING (layer N+1 never runs before layer N context reaches quorum), CONSISTENCY (single writer + LWW fallback), DURABILITY (majority holds the snapshot before advance; survives leader loss), NO EXTRA FETCHES (context embedded), VERSION VERIFICATION (worker detects stale dispatches from a lagging manager).

Drawbacks / mitigations (11452–11490): leader bottleneck → sync only at layer boundaries, one leader per job not per cluster; leader failure mid-layer → recover from last quorum snapshot, re-execute layer; quorum unavailable → job blocks, mitigated by "circuit breaker + configurable timeout" returning partial results or a clear failure (no numbers given); larger dispatches → send only dependencies' context, compress; not for fine-grained/streaming updates.
Integration (11504–11536): non-leader forward is an extra hop; quorum trouble marks the DC degraded for routing; cross-DC context dependencies are NOT supported (each DC runs the full job independently); fencing tokens are "synergistic"; cluster leader (SWIM ops) and job leader (per job per DC) are distinct roles — a follower manager can lead jobs.

### 3. Zombie job prevention: config and loops (12941–13084)
Detection (12952–12987): (1) per-workflow user timeout checked during progress updates; (2) SWIM alive→suspect→dead, dead workers trigger reassignment, reap every 15 min; (3) AD-19 progress IDLE→PROGRESSING→STALLED→STUCK, STUCK → investigate/evict, correlation detection prevents cascade evictions; (4) gate lease expiry; (5) orphan scanner every 120 s, 5 s per-worker query timeout; (6) AD-26 extension exhaustion.
Prevention (12998–13019): fence tokens; versioned clock (per-entity Lamport stamps, older values rejected, "ensures consistent ordering across DCs"); worker cancellation polling every 5 s, catching cancellations even if the push failed, with self-termination; quorum confirmation for critical state changes — failed quorum blocks the transition.
Orphan loop (13031–13061): sleep → known = `_workflow_assignments` keys → query every worker → orphaned = known − union(worker-reported) → `_mark_workflow_failed(..., "Orphaned - not found on any worker")`. (helper's note) Prose claims both directions (13027–13028) but the code handles only manager-known-not-on-worker; an unreachable worker returns nothing, so its workflows are marked orphaned unless SWIM reassignment ran first.
Config (13067–13083): four dead-node reap intervals at 900 s; COMPLETED_JOB_MAX_AGE 300 s; FAILED_JOB_MAX_AGE 3600 s; JOB_CLEANUP_INTERVAL 60 s; ORPHAN_SCAN_INTERVAL 120 s; ORPHAN_SCAN_WORKER_TIMEOUT 5 s; WORKER_CANCELLATION_POLL_INTERVAL 5 s.

### 4. Progress backpressure (AD-23) and windowed stats (13285–14172)
Goals: lossless, backpressure-aware, lifecycle-immediate, rate-controlled (13291–13294).
- Subprocess pushes status every 0.1 s and aggregates every 0.05 s (13308–13310). `RemoteGraphManager.get_availability()` is a non-blocking `(assigned, completed, available)` tuple; available = threads − max(assigned − completed, 0), callback when > 0 (13411–13429). Why state not queue: updates are cumulative totals, only the current value matters, and `await queue.get()` blocked when empty "causing 5+ second delays" (13432–13436).
- Buffer `_progress_buffer: workflow_id → latest WorkflowProgress` under an asyncio lock, latest-wins (13445–13458). Why: cumulative counts supersede older ones, no aggregation needed, memory O(active_workflows) (13461–13465).
- Flush loop (13470–13500): sleep effective interval = 50 ms base + `_backpressure_delay_ms`/1000 when a manager signalled delay; if the max backpressure level ≥ `BackpressureLevel.REJECT`, clear the buffer and skip; else copy+clear atomically and send each entry to its job leader, only if any healthy manager exists. Managers signal via a `BackpressureSignal` in progress acks (13584–13586). The enum levels and delay magnitudes are not defined in this range.
- Lifecycle: `_transition_workflow_status` is "the ONLY method that should change workflow status"; sets status, monotonic timestamp, wall-clock `collected_at`, elapsed; sends immediately, bypassing the buffer so short workflows stay visible (13508–13530).
- Routing (13538–13572): send to `_workflow_job_leader[workflow_id]` (the manager that dispatched, not necessarily the primary); on failure iterate healthy managers; the ack carries the current leader address to repair routing; per-manager circuit breaker (13383–13386).
- Before/after (13591–13609): the old inline rate limiter dropped updates, had no backpressure awareness, and competed with the flush loop.
- Env: WORKER_PROGRESS_UPDATE_INTERVAL 0.1 s, WORKER_PROGRESS_FLUSH_INTERVAL 0.05 s (13581–13582).

Time-windowed stats
- Goals: correlate workers by time, one push per window instead of per worker, bounded memory, hierarchical aggregation (13663–13666).
- Bucket = int(collected_at·1000 / 100); a window is closed when now_ms > window_end_ms + 50 ms drift (13752–13764).
- `WindowedStatsCollector(window_size_ms=100, drift_tolerance_ms=50, max_window_age_ms=5000)`, buckets keyed `(job_id, workflow_id, bucket_num)` under an asyncio lock; latest progress per worker per bucket (13789–13823).
- `flush_closed_windows(aggregate)`: closed windows are aggregated (sum completed/failed/rate, merge step stats by name, worker_count) or returned per-worker for gates, then deleted; buckets older than 5 s are deleted WITHOUT pushing ("missed or stuck") (13825–13899).
- Manager loop sleeps STATS_PUSH_INTERVAL (100 ms); `aggregate = not has_gates`; stamps `datacenter` when forwarding to gates (13957–13998). Gate keeps one collector per DC, re-adds per-worker stats with `collected_at = window_start` for alignment under id `"dc:worker"`, then merges identical `(job, workflow, window_start)` across DCs (14006–14061).
- Client: `RateLimiter(max_per_second=20, burst=5)`; over-limit pushes are dropped with `b'rate_limited'`; callback exceptions swallowed (14111–14136).
- Memory: removed on flush, age-based 5 s eviction, `cleanup_job_windows(job_id)` on completion (14156–14171).
- Cross-DC time: VersionedClock = max(local, received) + 1; monotonic time within a node, relative deltas across nodes (13168–13188).

Rate limiting AD-24 (12524–12639): health-gated by HybridOverloadDetector (AD-18): HEALTHY per-op limits; BUSY sheds LOW; STRESSED per-client fair share; OVERLOADED passes only CRITICAL (12546–12550). Priorities CRITICAL 0 (health, cancel, final results), HIGH 1 (submit, dispatch), NORMAL 2 (progress, stats), LOW 3 (12567–12570). SlidingWindowCounter effective = current + previous × (1 − window_progress), chosen as "deterministic ... without the edge cases of token bucket" (12578–12581). Per-op limits per 10 s: stats_update 500, heartbeat 200, progress_update 300, job_submit 50, job_status 100, workflow_dispatch 100, cancel 20, reconnect 10 (12606–12615). Client `CooperativeRateLimiter` honours Retry-After (default 1.0 s) (12622–12630).

Three-signal health AD-19 (12643–12791): liveness = SWIM UDP ping/ack, timeout 1 s, period 10 s, 3 failures (12659–12662); readiness = capacity + overload state, timeout 2 s (12670–12673); progress IDLE/PROGRESSING/STALLED/STUCK (12682–12685). Decisions: ROUTE, HOLD (not ready), INVESTIGATE (STALLED), DRAIN (STUCK), EVICT (not live) (12695–12701). NodeHealthTracker treats simultaneous failures as a likely network issue and withholds eviction (12731–12744). Signals piggyback on SWIM messages (12769–12782).

Adaptive extensions AD-26 (12795–12937): grant = max(min_grant, base_deadline / 2^(count+1)), "logarithmic decay to prevent indefinite delays" (12801–12804): 15, 7.5, 3.75, 1.875, 1.0 (min), 6th denied; cumulative ≤ 29.125 s (12807–12814). Warning when remaining ≤ 1; after exhaustion a 10 s grace period for checkpoint/cleanup, then eviction (12833–12855); `should_evict` = exhausted AND grace expired (12899–12900).

### 5. Gaps to resolve from source before porting
`_quorum_size` value/derivation (11285); `BackpressureLevel` levels and delay values (13478, 13498); lease replication to the backup (11772–11790); a stable hash for the ring (11655); quorum "circuit breaker + configurable timeout" numbers (11473); max retries for worker-death re-dispatch (10595); client `LEASE_DURATION` constant (11960) presumably equals the 30 s dataclass default (11743, 11984).

### 6. Constants table (range 8406–14200)
Kind: L = hardcoded literal/dataclass or constructor default; E = env-configurable default; D = derived at runtime; X = example/measurement only; U = referenced but undefined in range.

| Line | Name / context | Value | Meaning | Kind |
|---|---|---|---|---|
| 11647 | ConsistentHashRing.virtual_nodes | 150 | virtual nodes per gate | L |
| 11607, 11655 | ring space | 2^32 | hash modulus | L |
| 11675, 11758 | get_nodes count | 2 | owner + backup | L |
| 11781 | backup lookup count | 3 | next backup after takeover | L |
| 11743 | GateJobLease.lease_duration | 30.0 s | lease TTL | L (12971 says configurable per-job) |
| 11765, 12014 | initial fence_token | 1 | first ownership epoch | L |
| 11802 | renewal sleep | lease_duration/3 = 10 s | owner renewal cadence | D |
| 11942 | reconnect max_retries | 3 | client reconnect attempts | L |
| 11960, 11984 | reconnect wait | LEASE_DURATION/2 = 15 s | wait for lease transfer | D |
| 8744 | WorkflowDispatch.timeout_seconds | per dispatch | execution timeout (user-set, 12953) | D |
| 9479–9483 | priority core ranges | 25% / 75% / 100% (ceil) | allocation bands | L |
| 8477 | non-test workflow cores | 1 | fixed | L |
| 11285 | _quorum_size | undefined | acks needed for layer sync | U |
| 12595 | RATE_LIMIT_DEFAULT_BUCKET_SIZE | 100 | default limit | E |
| 12596 | RATE_LIMIT_DEFAULT_REFILL_RATE | 10.0 | default refill | E |
| 12597 | RATE_LIMIT_CLIENT_IDLE_TIMEOUT | 300 s | forget idle client counters | E |
| 12598 | RATE_LIMIT_CLEANUP_INTERVAL | 60 s | limiter cleanup cadence | E |
| 12599 | RATE_LIMIT_MAX_RETRIES | 3 | client retries on 429 | E |
| 12600 | RATE_LIMIT_MAX_TOTAL_WAIT | 60 s | client max cumulative wait | E |
| 12601 | RATE_LIMIT_BACKOFF_MULTIPLIER | 1.5 | client backoff factor | E |
| 12608–12615 | per-op limits / 10 s | 500, 200, 300, 50, 100, 100, 20, 10 | stats_update, heartbeat, progress_update, job_submit, job_status, workflow_dispatch, cancel, reconnect | L |
| 12629 | Retry-After default | 1.0 s | when header missing | L |
| 12751–12754 | LIVENESS timeout/period/fail/success | 1.0 s / 10.0 s / 3 / 1 | SWIM liveness probe | E |
| 12756–12759 | READINESS timeout/period/fail/success | 2.0 s / 10.0 s / 3 / 1 | readiness probe | E |
| 12761–12764 | STARTUP timeout/period/fail/success | 5.0 s / 5.0 s / 30 (=150 s) / 1 | slow-start allowance | E |
| 12923 | EXTENSION_BASE_DEADLINE | 30.0 s | healthcheck base deadline | E |
| 12924 | EXTENSION_MIN_GRANT | 1.0 s | floor per extension | E |
| 12925 | EXTENSION_MAX_EXTENSIONS | 5 | extensions before exhaustion | E |
| 12926 | EXTENSION_EVICTION_THRESHOLD | 3 | eviction threshold | E |
| 12927 | EXTENSION_EXHAUSTION_WARNING_THRESHOLD | 1 | remaining count that triggers warning | E |
| 12928 | EXTENSION_EXHAUSTION_GRACE_PERIOD | 10.0 s | grace before eviction | E |
| 12809–12813 | extension grants | 15 / 7.5 / 3.75 / 1.875 / 1.0 s | base/2^(n+1) with 1 s floor | D |
| 13068–13071 | *_DEAD_*_REAP_INTERVAL (4 vars) | 900 s | reap dead workers/peers/gates/managers | E |
| 13074 | COMPLETED_JOB_MAX_AGE | 300 s | keep completed job state | E |
| 13075 | FAILED_JOB_MAX_AGE | 3600 s | keep failed job state | E |
| 13076 | JOB_CLEANUP_INTERVAL | 60 s | job cleanup cadence | E |
| 13079 | ORPHAN_SCAN_INTERVAL | 120 s | orphan scan cadence | E |
| 13080 | ORPHAN_SCAN_WORKER_TIMEOUT | 5 s | per-worker query timeout | E |
| 13083 | WORKER_CANCELLATION_POLL_INTERVAL | 5 s | worker polls for cancellation | E |
| 13308–13310 | subprocess push / aggregate schedule | 0.1 s / 0.05 s | status pipeline cadence | L |
| 13581 | WORKER_PROGRESS_UPDATE_INTERVAL | 0.1 s | status-queue poll | E |
| 13582 | WORKER_PROGRESS_FLUSH_INTERVAL | 0.05 s | buffer flush base | E |
| 13497–13499 | effective flush interval | base + delay_ms/1000 | manager-signalled backpressure | D |
| 13478 | BackpressureLevel.REJECT | undefined | drop buffered progress | U |
| 13752–13753 | WINDOW_SIZE_MS / DRIFT_TOLERANCE_MS | 100 / 50 ms | bucketing constants | L |
| 13791–13793 | WindowedStatsCollector defaults | 100 / 50 / 5000 ms | window, drift, max age | L |
| 14145–14147 | STATS_WINDOW_SIZE_MS / DRIFT / PUSH_INTERVAL | 100 / 50 / 100 ms | manager wiring | E |
| 14150–14151 | CLIENT_PROGRESS_RATE_LIMIT / BURST | 20 /s, 5 | client callback limiter | E |
| 13177 | VersionedClock | max(local, received)+1 | Lamport merge | D |
| 9099 | DNS lookup | 50–200 ms | motivation for optimized args | X |

---

## Range 15016–20164

Covers AD-33 Federated Health Monitor (15016–15243), AD-33 Workflow State Machine (15249–16254), AD-34 Adaptive Job Timeout incl. AD-26 integration and cleanup (16258–18825), AD-35 Vivaldi + role-aware detection (18827–19702), AD-36 Vivaldi routing (19705–19967). AD-35 and AD-36 are marked **Status: Proposed** (18829, 19707).

### 1. Federated Health Monitor (AD-33)
- Why / how it differs from SWIM: gates must judge remote-DC health for routing. SWIM assumes 1–10 ms intra-DC RTT and full membership; cross-DC links are 50–300 ms and gates "don't need full membership semantics" (15018). So FHM is SWIM-style probe/ack with no gossip and no membership (15020, 15050). Scope is gate → DC-leader managers only (15058); bare `xprobe`/`xack`, no ping-req/suspect/dead dissemination (15059); no gossip (15060); suspicion timeout 30 s default vs SWIM's 1.5–8 s (15062); each DC has a separate "external incarnation" independent of internal SWIM incarnations (15064, 15226).
- Messages: `CrossClusterProbe{source_cluster_id, source_node_id, source_addr}` (15073–15078). `CrossClusterAck` carries `datacenter, node_id, incarnation (external)`, `is_leader, leader_term`, `cluster_size, healthy_managers`, `worker_count, healthy_workers, total_cores, available_cores`, `active_jobs, active_workflows`, and self-reported `dc_health` in {HEALTHY, DEGRADED, BUSY, UNHEALTHY} plus `health_reason` (15083–15111).
- State machine `DCReachability` (15117–15140): UNREACHABLE initial; first ack → REACHABLE; REACHABLE → SUSPECTED when `consecutive_failures >= max_failures`; SUSPECTED → REACHABLE on any ack; SUSPECTED → UNREACHABLE when `suspicion_timeout` expires; "leader change" edge returns UNREACHABLE to REACHABLE. Suspicion is driven by a failure count, not a single missed probe.
- Timing (env vars; 15151–15154): probe interval 2.0 s, probe timeout 5.0 s, suspicion timeout 30.0 s, max consecutive failures 5. Rationale (15159–15164): 2 s "reduce cross-DC traffic while maintaining freshness"; 5 s "accommodate 100–300 ms RTT + processing time"; 30 s "tolerate transient network issues"; 5 failures ≈ 10 s before suspected. Defaults are "5–10x higher than SWIM defaults" (15232).
- Integration: every successful probe's RTT feeds `CrossDCCorrelationDetector.record_latency` (15174–15178). On SUSPECTED/UNREACHABLE the gate checks correlation; if `level >= CorrelationLevel.MEDIUM` it delays eviction because several DCs failing at once implies a network problem (15181–15188). Routing calls `get_healthy_datacenters()`, raises `NoHealthyDatacentersError` if empty, then picks by xack capacity (15212–15217).
- Design decisions (15224–15232): no gossip; separate incarnation; aggregate rather than per-node health; leader-only probing.

### 2. Workflow State Machine (AD-33) — failure/retry/cancel subset
- Motivation: status lived in `WorkflowProgress.status`, `sub_workflows` and pending queues with no transition validation; dependents could start before a failed parent retried (15255–15260).
- States (15335–15364): PENDING, DISPATCHED, RUNNING, COMPLETED; FAILED, FAILED_CANCELING_DEPENDENTS, FAILED_READY_FOR_RETRY; CANCELLING, CANCELLED; AGGREGATED. Three-step failure path rationale: FAILED blocks dispatch immediately; FAILED_CANCELING_DEPENDENTS blocks retry until dependents are cleared; FAILED_READY_FOR_RETRY is the only state that may re-enter PENDING (15348–15351).
- `VALID_TRANSITIONS` (15397–15438): PENDING→{DISPATCHED, CANCELLING, FAILED}; DISPATCHED→{RUNNING, CANCELLING, FAILED}; RUNNING→{COMPLETED, FAILED, CANCELLING, AGGREGATED}; FAILED→{FAILED_CANCELING_DEPENDENTS, CANCELLED}; FAILED_CANCELING_DEPENDENTS→{FAILED_READY_FOR_RETRY}; FAILED_READY_FOR_RETRY→{PENDING}; CANCELLING→{CANCELLED}; COMPLETED/CANCELLED/AGGREGATED terminal. Invalid transitions logged and rejected (15441–15444).
- Mechanics: one `asyncio.Lock` serialises all transitions; per-workflow state dict plus an unbounded per-workflow history list of `StateTransition{from,to,timestamp(monotonic),reason}` (15470–15476, 15513–15518). Unknown workflow reads as PENDING (15496). `cleanup_workflow` drops state+history at job cleanup (15535–15538).
- Worker failure (15556–15570): collect DISPATCHED/RUNNING workflows on the dead worker → FAILED → find all dependents → FAILED_CANCELING_DEPENDENTS → pending dependents removed from dispatcher and marked CANCELLED, running dependents sent `WorkflowCancelRequest` (TCP timeout 5.0 s, 15715) via CANCELLING→CANCELLED → FAILED_READY_FOR_RETRY → parent+dependents re-queued in Kahn topological order (15823–15862; falls back to input order on a cycle) → PENDING. Handler is idempotent (16076–16083).
- Dispatch only from PENDING (15881); worker ack `accepted` → RUNNING, otherwise FAILED (15901–15915). Results for workflows not RUNNING are dropped (15941–15946). Job cancel: PENDING → CANCELLED directly; DISPATCHED/RUNNING → CANCELLING → CANCELLED on worker confirmation (15974–16005).
- Inconsistency to resolve in a port: cancel paths do PENDING→CANCELLED (15665–15670, 15982–15986) but the table only permits PENDING→CANCELLING.

### 3. Adaptive Job Timeout (AD-34)
- Inputs are local measurements. `TimeoutTrackingState` (16352–16377) holds `started_at` (monotonic at submission), `last_progress_at` (bumped on every AD-33 state transition and every AD-26 extension grant), `last_report_at`, the job's `timeout_seconds`, and `stuck_threshold` default 120 s (16369). Two checks per job, by the leader every 30 s (17239–17246): overall `now − started_at > timeout_seconds` (16543–16544) and stuck `now − last_progress_at > stuck_threshold` (16556–16557). With extensions: `effective_timeout = timeout_seconds + total_extensions_granted` (17910); not stuck if any extension was granted within `stuck_threshold` (17928–17932). Extensions are additive (17803).
- Strategy selection: `JobSubmission.gate_addr` present → `GateCoordinatedTimeout`, absent → `LocalAuthorityTimeout`; "no configuration needed" (16334–16342).
- Local authority: manager calls `_timeout_job` (16552); idempotent via `locally_timed_out` (16531–16533).
- Gate coordinated: manager sends `JobProgressReport` every 10 s best-effort with `has_recent_progress` = progress in the last 10 s (16736–16739, 16825–16839); on local timeout/stuck sends `JobTimeoutReport`, kept in `_pending_reports[job_id]` until success (16847–16883); if no global decision for >300 s since `last_report_at`, times out locally as "gate_unresponsive_fallback" (16709–16725). Gate `_global_timeout_loop` runs every 15 s (17074) and declares timeout on: elapsed > `timeout_seconds` (+ MAX of per-DC extension totals, 18086–18091); any DC reported timed_out (only DCs with zero extensions, 18105–18110); all DCs silent >180 s (17049–17058, 18127–18131). Each declaration bumps the gate fence and broadcasts `JobGlobalTimeout` (17095–17112).
- Leader transfer: state inside `JobInfo`; new leader calls `resume_tracking`, increments `timeout_fence_token` (16508–16510); `started_at` absolute so deadline unchanged (17400–17416).
- Fencing: `handle_global_timeout` rejects when `msg.fence_token < job.timeout_fence_token` (16788–16796). Caveat: gate stamps its own decision counter while the manager compares against its leader-transfer counter; the doc never reconciles the two number spaces.
- Races: timeout report vs completion ordered by timestamp with corrections (17438–17461). Caveat: the gate compares manager-supplied monotonic timestamps with the gate's own monotonic clock; monotonic clocks aren't comparable across hosts.
- Cleanup: `stop_tracking` idempotent; strategies MUST be removed at terminal state (18825). No TTLs anywhere in AD-34; unbounded: `_pending_reports` (16644), gate `_tracked_jobs`, AD-33 `_last_progress` (17513), per-workflow history lists.

### 4. Vivaldi + role-aware failure detection (AD-35)
- Problem: static 30 s gate→manager timeout is too conservative at 10 ms, false positives at 150 ms, "dangerously aggressive" at 300 ms (18839–18844).
- `VivaldiCoordinate{position: list[float] "typically 4D", height, error}` (18890–18895). Update rule as written: `predicted = distance(A,B); err = measured − predicted; A.position += delta × err × unit_vector(B→A)` (18902–18904). The doc does not specify `delta`, ce/cc, error-update, or initial values; take from Dabek et al. 2004 (19471). Convergence 10–20 rounds ≈ 10–20 s at 1 s probes (18907). Coordinates piggyback on SWIM ping/ack (18911–18931); ~50–80 B/message (18940); 4D ≈ 40 B/peer (19344–19346).
- Adaptive timeout: `timeout = base × latency_multiplier × load_multiplier(LHM) × confidence_adjustment`, `latency_multiplier = min(10, max(1, est_rtt / 10 ms))`, `confidence_adjustment = 1 + vivaldi_error / 10` (18989–19006).
- Role multipliers (18979–18983): Gate 120 s passive timeout, 5 confirmation attempts, Vivaldi on, load cap 3×; Manager 90 s, 3 attempts, 5×; Worker 180 s, no proactive probing, no Vivaldi, 10× ("expendable").
- UNCONFIRMED lifecycle (19036–19041): gossip discovery → UNCONFIRMED; first bidirectional ping/ack → ALIVE; role-aware timeout with no confirmation → removed, NOT marked DEAD; only ALIVE peers may become SUSPECT (19062, 19097).
- Confidence-aware RTT (UCB) (19483–19515): `coordinate_quality = clamp01(min(1, samples/MIN_SAMPLES_FOR_ROUTING) × min(1, ERROR_GOOD_MS/max(err,1)) × staleness factor)`; `rtt_ucb = clamp(distance + K_SIGMA × clamp(err_local + err_remote, SIGMA_MIN, SIGMA_MAX), RTT_MIN, RTT_MAX)`. Missing/low-quality coordinates never exclude a peer.
- Hysteresis (19575–19584; 19845–19865): coordinate-unaware mode, hold-down, improvement ratio, forced switch on degradation, cooldown after failed dispatch. No numeric thresholds given.
- AD-36 scoring: `score = rtt_ucb × load_factor × quality_penalty`; buckets HEALTHY/BUSY/DEGRADED; UNHEALTHY excluded; Vivaldi ranks only inside a bucket (19790–19836).
- Rejected alternatives: static per-DC-pair timeouts (O(n²)); exponential backoff (false positives while learning); ping-only; Vivaldi without roles (19623–19683). Success criteria: <1% cross-DC false positives, <10 s same-DC detection, zero worker probes, prediction error <20% within 20 s (19431–19453).

### 5. Constants table (range 15016–20164)
Kind: C configurable default, H hardcoded, D derived, S symbolic (no value in range), E example/target.

| Constant | Value | Meaning | Kind | Line |
|---|---|---|---|---|
| FEDERATED_PROBE_INTERVAL | 2.0 s | gap between xprobes per DC | C | 15151 |
| FEDERATED_PROBE_TIMEOUT | 5.0 s | single xprobe timeout | C | 15152 |
| FEDERATED_SUSPICION_TIMEOUT | 30.0 s | SUSPECTED → UNREACHABLE | C | 15153 |
| FEDERATED_MAX_CONSECUTIVE_FAILURES | 5 (~10 s) | failures before SUSPECTED | C | 15154 |
| Dependent-cancel TCP timeout | 5.0 s | WorkflowCancelRequest send | H | 15715 |
| stuck_threshold | 120.0 s | manager no-progress → stuck | C | 16369 |
| Manager timeout loop period | 30 s | `_unified_timeout_loop` | H | 17239 |
| Gate-unresponsive fallback | 300 s | since last report → local timeout | H | 16713 |
| Progress report interval | 10.0 s | manager → gate | H | 16736 |
| Gate all-DCs-stuck window | 180.0 s | silence from every DC | H | 17051 |
| Gate global loop period | 15.0 s | | H | 17074 |
| Vivaldi dimensions | 4 | | H | 18892 |
| Vivaldi delta / ce / cc / error update | unspecified | | S | 18904 |
| Gate passive timeout / attempts / load cap | 120 s / 5 / 3× | UNCONFIRMED handling | H | 18981 |
| Manager passive timeout / attempts / load cap | 90 s / 3 / 5× | | H | 18982 |
| Worker passive timeout / attempts / load cap | 180 s / none / 10× | | H | 18983 |
| reference_rtt | 10.0 ms | same-DC baseline | H | 18994 |
| latency_multiplier clamp | [1.0, 10.0] | est_rtt / reference | H | 18997 |
| confidence_adjustment | 1 + error/10 | | H | 19003 |
| Confirmation attempt wait / spacing | 5 s / 5 s | | H | 19150–19159 |
| MIN_SAMPLES_FOR_ROUTING, ERROR_GOOD_MS, COORD_TTL_S, RTT_DEFAULT_MS, SIGMA_DEFAULT_MS, SIGMA_MIN_MS, SIGMA_MAX_MS, K_SIGMA, RTT_MIN_MS, RTT_MAX_MS, ERROR_MAX_FOR_ROUTING | — | UCB/routing thresholds | S | 19491–19509, 19877–19878 |
| A_UTIL, A_QUEUE, A_CB, LOAD_FACTOR_MAX, A_QUALITY, QUALITY_PENALTY_MAX, PREFERENCE_MULT, HOLD_DOWN_S, IMPROVEMENT_RATIO, DEGRADE_RATIO, DEGRADE_CONFIRM_S, QUEUE_SMOOTHING | — | AD-36 scoring/hysteresis | S | 19811–19850 |
| AD-37 backpressure tiers | <70 / 70–85 / 85–95 / >95% fill | NONE/THROTTLE/BATCH/REJECT | H | 19992–19995 |
| AD-38 tier latencies | 50–300 ms / 2–10 ms / <1 ms | global / regional / WAL | E | 20086–20112 |

---

## Range 20164–31380 (AD-38 / AD-39: WAL, ledger, HLC, VSR, single-writer buffers)

Scope note: three overlapping design generations: AD-38 (dedicated `NodeWAL` + ledger, 20164–24756), AD-39 Parts 1–10 (extend the Logger instead, 24758–25920), AD-39 Parts 11–16 (asyncio internals → write coalescing → segmented buffers → single-writer/single-reader queues, 25922–31376). No env-var configuration appears in this range; every tunable is a dataclass/constructor default or a literal.

### 1a. Three distinct on-disk formats
- AD-38 `WALEntry`: 34-byte header = CRC32(4) + Length(4) + LSN(8) + HLC(16 = wall_time_ms u64 + logical_counter u64) + State(1) + Type(1), then payload (20282–20290, 21089). Packed little-endian (21093–21102). CRC covers everything after the CRC field (21104); mismatch raises `ValueError` (21118–21120). State byte = PENDING=0/REGIONAL=1/GLOBAL=2/APPLIED=3/COMPACTED=4 (21060–21066); written once (PENDING), later transitions only in an in-memory `_state_index` (21452, 21645–21659).
- AD-39 `WALWriter` entry: 16-byte header = CRC32(4) + Length(4) + LSN(8), payload msgspec-JSON; CRC covers length+LSN+payload (25169–25195, 27009–27029). LSN is a Snowflake id (25088–25091): unique and monotonic per instance, not dense.
- Part 15/16 segment framing: one 16-byte header per flushed segment: sequence u64 LE + size u32 LE + crc u32 LE, CRC accumulated incrementally over segment bytes only (29974, 30004, 30008–30016). Reader verifies CRC and sequence contiguity (30897–30910).

### 1b. Group commit
- AD-38 `NodeWAL`: `sync_mode=FSYNC_BATCH`, `batch_size=100`, `batch_timeout_ms=10` (21440–21443). Each append makes a Future under an `asyncio.Lock`; flush if pending ≥ batch_size, else a task sleeps `batch_timeout_ms` then flushes (21607–21628). Flush = one `mmap.flush()` (msync) then resolve every Future (21630–21643).
- AD-39 LoggerStream: `_batch_timeout_ms=10`, `_batch_max_size=100` (25096–25097); Part 11 adds `loop.call_later` timer armed on first entry (26204–26246). fsync in executor (26265–26270). Claim: ~10x over per-write fsync, latency bounded to 10 ms (25352–25353).
- Part 12 `WALWriter`: `batch_timeout_ms=5.0`, `batch_max_size=100` (26778–26779); one executor call per batch does every write + one flush + one fsync (26962–27007).
- Part 14 `DoubleBuffer`: flush only when front holds ≥ `segment_count=4` full 64 KiB segments or explicit flush (29140, 29202–29207) — no time trigger.
- Part 15 `SingleWriterBuffer`: drain waits with `flush_interval=0.01 s`; sets `flush_event` when unflushed bytes ≥ `flush_size_threshold=262144` (30243–30291). Triggers: 256 KiB OR 10 ms OR explicit.

### 1c. fsync policy and durability modes
- AD-38 `WALDurability`: MEMORY, WRITE, FSYNC, FSYNC_BATCH (default) (21402–21407). AD-39 `DurabilityMode`: NONE, FLUSH, FSYNC, FSYNC_BATCH (24969–24983); recommended FLUSH for data plane, FSYNC_BATCH for WAL, NONE for tests (25771–25774).
- Estimates: FSYNC ~1–10 ms/write on SSD (25645); FSYNC_BATCH ≈ 10 ms + 1 ms/N per write (25649–25656); throughput NONE ~1M/s, FLUSH ~500K/s, FSYNC ~500/s, FSYNC_BATCH ~50K/s (64-byte entries, NVMe) (25659–25667). End-to-end: <1 ms write, ≤10 ms batch fsync, ~5 ms regional, ~100 ms global (25677–25712).
- macOS "may lie about fsync": Part 13 adds `fcntl(F_FULLFSYNC)` (28116–28137); Part 15 uses F_FULLFSYNC instead of fsync on darwin (30362–30369).
- Rule: every blocking op goes through `run_in_executor` (25969, 26291–26299); AD-38's inline FSYNC (21600) and mmap storage violate it (28464).

### 1d. Durability level per operation
- Job create/cancel/complete/timeout → GLOBAL (50–300 ms); workflow dispatch/complete/cancel → REGIONAL (2–10 ms); worker progress → LOCAL (<1 ms); stats/metrics → NONE (20429–20440). GLOBAL for everything adds 200–300 ms to every op (20425). Node tiers: gates GLOBAL, managers REGIONAL, workers NONE — workers have no WAL and never sit on consensus or ack paths (23083–23129, 24717–24720).

### 1e. Backpressure
- AD-38 `_pending_batch` unbounded (21450).
- Default `run_in_executor` queue unbounded: 10,000 writers = 32 running + 9,968 queued (26483–26515).
- Part 12 `WALWriter`: `buffer_max_size=10000` (26780); `write()` awaits `backpressure_event`; Event cleared when buffer ≥ max; set again when flush takes the buffer (26833–26941). Memory bound = buffer_max_size × avg entry (27400–27425).
- Part 14 `BufferPool`: fixed 16 × 64 KiB; when exhausted allocates "overflow" segments (29027–29043).
- Part 15 `SingleWriterBuffer`: bounded `asyncio.Queue(maxsize=10000)` (30091–30093). `write()` blocks; `try_write()` returns `QUEUE_FULL`; `write_with_timeout()`; `try_write_durable()`. Statuses `SUCCESS/QUEUE_FULL/SHUTDOWN`, "no silent drops" (29944–29948). Caller patterns incl. exponential backoff 3 retries 1 ms × 2^attempt (30423–30492).
- Part 16 `SingleReaderBuffer`: bounded result queue (1000) (30720–30722).
- Circuit breaker: while OPEN, ops queued only if `qsize < 1000` (22903–22910).

### 1f. Recovery
- AD-38 `NodeWAL.open()`: glob segments sorted; mmap; iterate; track max LSN/HLC and pending (state < GLOBAL); `next_lsn = max_lsn+1`; open new segment if last has <10% free (21486–21521). End-of-data detection relies on zero-filled preallocation (21286–21308). Corrupt entries raise uncaught `ValueError` — no torn-tail truncation.
- Node flow: latest checkpoint → restore snapshot → replay WAL from checkpoint LSN → reconcile → ready (20961–20998).
- AD-39 per-entry: PENDING → replay to consensus; REGIONAL → verify with DC; GLOBAL → recovered (25604–25617). Readers raise on truncation/CRC mismatch and yield every 100 entries (26174–26177).
- Part 16: CORRUPTION / SEQUENCE_GAP delivered as statuses; prefetcher stops; "caller decides recovery" (30825–30828, 31078–31085).

### 1g. Checkpoint and compaction
- Checkpoint: header (id, created_at, local/regional/global LSN), state snapshot, indexes (20911–20931, 22060–22068). Triggers: ≥100,000 entries OR ≥300 s (22090–22091). Atomic snapshot → temp file + rename → background compact (22121–22176). `safe_lsn = min(local_lsn, global_lsn)` (22178–22182); only sealed segments with all LSN ≤ safe_lsn unlinked (21679–21710). Keep newest 3 checkpoints (22092). Targets: WAL ≤ 2× active state (23165), recovery < 30 s (23162).

### 1h. Bounded caches
- `JobLedgerStateMachine._history` max 10,000 FIFO (24236, 24264–24267). Buffer pools fixed. CB queue ≤ 1000, 60 s TTL (22904, 22969–22971). Stats cleared each 500 ms flush; gate retention 3600 s (20661).
- Unbounded (porting caveats): `NodeWAL._state_index` never pruned (21452); VSR replica `prepare_log`/`commit_log` grow forever (23580–23581); `WALWriter` metrics counters monotonic.

### 2. Hybrid Logical Clock
- `(wall_time_ms, logical_counter, node_id)` (21160–21162); total order = wall, then logical, then node_id (21207–21212).
- Local tick: `new_wall = max(current_wall, physical_now_ms)`; same wall → `logical += 1`, else 0 (21164–21176). Receive: `new_wall = max(local, remote, physical)`; equal → `max(l_local, l_remote)+1`; local max → `l_local+1`; remote max → `l_remote+1`; physical max → 0 (21178–21205).
- No numeric drift bound; nothing rejects a remote clock far in the future — a port should add one.
- Ticked on every WAL append (21569); restored from max on recovery (21515); conflict-resolution rank #3 (21809–21811); carried in VSR entries (23290).

### 3. Commit pipeline, ordering, session consistency
- Stages: LOCAL (mmapped segment, batched fsync, <1 ms), REGIONAL (Raft/Paxos in DC, quorum 2/3, 2–10 ms), GLOBAL (cross-region, quorum 3/5 regions, 50–300 ms) (20710–20739).
- `commit_job_event(event, required_durability=REGIONAL)` (21882–21886): append FSYNC_BATCH; return if LOCAL; propose to regional consensus, `wait_for(…, 5.0 s)`; on timeout return LOCAL + error; if REGIONAL requested, global replication fire-and-forget; else await global Future 30 s (21904–22000).
- Conflict = same job_id and fence_token; deterministic resolution: cancellation wins > higher fence > HLC > lexicographic node_id (20804–20810, 21774–21814). "Causal+ for reads, linearizable for critical ops" (23163).
- Session levels: EVENTUAL, SESSION (read-your-writes), BOUNDED_STALENESS, STRONG (21009–21014). Reading a job you wrote must hit the authoritative home replica (21019–21039).

### 4. Anti-entropy
- Merkle tree over job-id ranges (20832–20846); exchange root → subtree → range events → merge with conflict rules (20852–20869). Repair FSM CONSISTENT → COMPARING → FETCHING → MERGING → VERIFYING → CONSISTENT (20875–20899). Home region authoritative; job id `{region_code}-{timestamp_ms}-{gate_id}-{sequence}` (20770–20799). No interval/fan-out/hash specified.

### 5. Per-job Viewstamped Replication (23181–24709)
- Why VSR not multi-Raft: per-job leadership already exists (hash ring primary + backups; lease TTL; fencing tokens), so Raft election is redundant (23189–23196). Mapping: fencing token = view; job leader = primary; ring backups = replica set; lease expiry = view-change trigger (23200–23206).
- Messages: `Prepare(job_id, view, seq, data, hlc)`; `PrepareResponse(status ∈ SUCCESS|STALE_VIEW|WRONG_SEQUENCE|NOT_OWNER, current_view, expected_seq)`; `Commit`; `ViewChange`; `ViewChangeResponse`; `NewView` (23493–23559).
- Write path: verify lease → assign seq → parallel Prepare → replica checks view/seq, persists, acks → primary counts acks ≥ `quorum_size` → Commit fire-and-forget → resolve Future → ACK client (23360–23405, 23956–24052).
- View change: next ring node detects lease expiry → acquires lease (view+1) → `ViewChange` → collect `last_prepared_seq` → `start_seq = max+1` → `NewView` (23413–23451, 24110–24175).
- Config: `replica_count=3`, `quorum_size=2`, `prepare_timeout_ms=5000`, `view_change_timeout_ms=10000`; lease TTL 10 s, renewal 3 s (23870–23876, 24621–24655). Perf: write 80–150 ms; local read <1 ms; failover 5–15 s (24444–24480, 24598–24606).
- Porting caveats: `view_change_timeout_ms` unused and no quorum of `ViewChangeResponse`s enforced (24129–24137); replicas' `uncommitted_entries` ignored (24135–24153); `handle_prepare` resets `expected_seq=0` on new view while `handle_new_view` sets `start_seq` (23601–23603 vs 23687–23689).

### 6. Single-writer / single-reader architecture (AD-39 Parts 11–16)
- Motivation: `run_in_executor` costs ~5–25 µs/call plus 1–10 ms/fsync; default pool `min(32, cpu_count+4)`; unbounded queueing (26464–26550). Options: per-write executor ✗, dedicated writer thread (~50–100K/s), write coalescing ✓✓ (~100K+/s, ≤5 ms), io_uring (~1M IOPS) (26560–26643). Benchmark 100K writes: 45 s / 2,200/s / P99 200 ms vs 5 s / 20,000/s / P99 10 ms (27333–27376).
- Portability math: io_uring 10x faster but 4 impls × 3x complexity = 12x maintenance (27687–27696). Avoid io_uring/kqueue/IOCP, mmap+msync (28462–28466). (Note for slates: this is the opposite call from what slates' rules demand; slates is memory-only and has no fsync path at all.)
- Part 15 single writer: "the correct primitive is not locks — it's queues" (29869–29882). Producers → bounded `asyncio.Queue` → one drain task → double buffer → one flush task → 1-thread executor → write + fsync → wake durability waiters (29885–29917). Memory ≈ 1.1 MB fixed (30510–30517). Vs sharded locks: same ~1M/s, zero locks, trivial correctness proof (29921–29929).
- Part 16 single reader mirrors it: one prefetch task, bounded queue, N consumers; parallel chunk readers rejected (31194–31221); random access via sequence→offset index (31223–31341).
- For a Rust port: the "no locks" claim rests on cooperative single-threaded scheduling; maps to an MPSC channel with one owning task.

### 7. Constants table (range 20164–31380)
Kind: D dataclass/constructor default; L literal; E estimate; S stated requirement.

| Constant | Value | Meaning | Kind | Ref |
|---|---|---|---|---|
| `WALEntry.HEADER_SIZE` | 34 B | AD-38 entry header | L | 21089 |
| AD-39 entry header | 16 B | CRC32 + len + LSN | L | 25179 |
| Part 15 segment header | 16 B | seq u64 + size u32 + crc u32 | L | 29974 |
| `NodeWAL.segment_size` | 64 MiB | preallocated zero-filled mmap segment | D | 21440 |
| `NodeWAL.batch_size` / `batch_timeout_ms` | 100 / 10 ms | group-commit triggers | D | 21442–21443 |
| Regional / global wait | 5.0 s / 30.0 s | commit pipeline | L | 21936, 21978 |
| Regional / global quorum | 2/3 nodes; 3/5 regions | | L | 20720, 20727 |
| Stage latency | <1 / 2–10 / 50–300 ms | LOCAL / REGIONAL / GLOBAL | E | 20735–20739 |
| `checkpoint_interval_entries` / `_seconds` | 100,000 / 300 s | | D | 22090–22091 |
| `max_checkpoints_to_keep` | 3 | | D | 22092 |
| CB failure/success threshold | 5 / 3 | | D | 22781–22782 |
| CB open_timeout / half_open probes | 30 s / 1 | | D | 22783–22784 |
| CB queue_max_size / queue_timeout | 1000 / 60 s | | D | 22785–22786 |
| Ack initial_window / max_extensions | 5.0 s / 3 | | D | 22601–22602 |
| Stats worker batch | 100 ms or 1000 events | | D | 22362–22363 |
| Stats manager aggregate / sample | 500 ms / 0.1 | | D | 22364–22365 |
| VSR `replica_count` / `quorum_size` | 3 / 2 | | D | 23873–23874 |
| VSR `prepare_timeout_ms` | 5000 | > max cross-DC RTT ~300 ms | D | 23875 |
| `lease_ttl_seconds` / `renewal_interval_seconds` | 10 s / 3 s | | D | 24646, 24650 |
| `max_history` | 10,000 | ledger history | D | 24236 |
| LoggerStream batch | 10 ms / 100 | | L | 25096–25097 |
| Mode throughput | ~1M / ~500K / ~500 / ~50K per s | NONE/FLUSH/FSYNC/FSYNC_BATCH | E | 25659–25667 |
| `YIELD_INTERVAL` | 100 entries | | L | 26176 |
| Executor overhead | 5–25 µs/call; 1–10 ms/fsync | | E | 26536–26537 |
| `WALWriter.batch_timeout_ms` / `batch_max_size` / `buffer_max_size` | 5.0 ms / 100 / 10,000 | | D | 26778–26780 |
| `BufferPool.segment_size` / `pool_size` | 64 KiB / 16 | | D | 29008–29009 |
| `DoubleBuffer.segment_count` | 4 | | D | 29140 |
| `SingleWriterBuffer.queue_size` | 10,000 | | D | 30085 |
| `flush_interval` / `flush_size_threshold` | 0.01 s / 262,144 B | | D | 30088–30089 |
| `PrefetchBuffer` capacity | 262,144 B | | D | 30631 |
| `SingleReaderBuffer.queue_size` / `chunk_size` | 1000 / 65,536 B | | D | 30716, 30718 |

---

## Range 31380–38790 (AD-40..AD-45)

### 1. Idempotent job submissions (AD-40, 31380–33140)
- Key `{client_id}:{sequence}:{nonce}` (31479): stable client id; monotonic per-client sequence; 8-byte random nonce per process (31490–31492). Same retry → same key; new process → new nonce (31485–31488).
- Status PENDING → COMMITTED | REJECTED | EXPIRED; terminal states immutable (31590–31599). Duplicate handling: PENDING → wait or time out; COMMITTED/REJECTED → cached result; not found → insert PENDING and process (31685–31694).
- Gate cache (fast path): OrderedDict LRU, TTL, waiter futures coalesce concurrent duplicates (31717–31724); PENDING hit awaits future up to `pending_wait_timeout` (31793–31797); on timeout accepted=False "Request pending, please retry" (32843–32849). Eviction pops LRU head; evicted PENDING waiters get TimeoutError (31813–31823). Sweep every `cleanup_interval`; per-status TTL (31929–31958).
- Manager ledger (authoritative): index + WAL + TTL cleanup + per-job VSR (32061–32073). `check_or_reserve` persists PENDING to WAL before index update (32131–32158). Inconsistency: commit/reject docstrings say WAL-first but code mutates first (32170 vs 32179–32185); `max_entries` never enforced in the ledger.
- Cross-DC: `IDEMPOTENCY_RESERVED` / `IDEMPOTENCY_COMMITTED` events in the per-job VSR log so any replica answers duplicates (32430–32519).
- Failure cases: TTL expiry + late retry → duplicate job, explicitly not protected; TTL must exceed client retry window (5 min vs 2 min) (32654–32677). Clock skew → "use HLC for TTL if critical" (33094).
- Client: one key reused across ≤3 retries with 1 s × 2^attempt backoff (32739–32778).
- Sizing profiles (32970–33024); ~200 B/entry; hit rate >5% signals aggressive retries (33013).

### 2. Resource guards (AD-41, 33142–34747)
- Per workflow process tree via psutil; sums cpu_percent and RSS; FD count (33433–33487). Output ResourceMetrics with σ; stale after 30 s (33503–33517).
- Kalman not EWMA (33234–33282): scalar random-walk filter Q=10, R=25, P₀=1000 (33305–33309); adaptive R via innovation window 20, ratio >1.2 → R×1.1, <0.8 → R×0.9, clipped [0.1×, 10×] (33359–33430). Per-signal: CPU Q=15 R=50; memory Q=1e6 R=1e7 (33535–33538).
- Aggregation: worker→manager in heartbeat (version-gated); manager↔manager gossip every 2–5 s; 30 s staleness (33694–33760). cpu_pressure = min(1, Σcpu ÷ max(1, workers×400)) (33969–33970); memory_pressure never computed in shown code.
- Limits absolute per job: default 800% CPU, 16 GiB, 10,000 FDs (34191–34197); thresholds 0.8/0.95/1.0 with grace 10/5/2 s (34145–34156); σ multiplier 2.0 (34277). Enforcer emits only NONE/WARN/KILL_WORKFLOW; THROTTLE and critical_threshold unused (34130–34136, 34152).
- Kill: ResourceKillRequest → worker → SIGTERM to registered root PID (34441–34456, 34530–34533).

### 3. Retry budgets (AD-44, 36923–37722)
- Job requests `retry_budget` and `retry_budget_per_workflow`; manager clamps to env max (ceilings 50/5, defaults 10/3) (37349–37363). Fixed pool for job lifetime, no refill (37644–37647). Each DC's manager has its own full budget (37468–37471).
- Best-effort completion opt-in: `best_effort_min_dcs=1`, deadline default 300 s max 3600 s, check interval 5 s (37328–37380). Completion order: all reported; disabled → wait; completed ≥ min_dcs; deadline (37179–37198).

### 4. SLO-aware health and routing (AD-42, 34749–35925)
- T-Digest (δ=100, buffer 2048) over HDR/P²/sorted/sampling (34813–34849, 35012–35013). Windows 60 s × 5 (35016–35018). Gossip SLOSummary ≈32 B (34953–34963); max 100 job summaries per heartbeat, 30 s TTL (35054–35055).
- Compliance: targets p50 50 ms, p95 200 ms, p99 500 ms; weights 0.2/0.5/0.3; min samples 100; levels <0.8 EXCEEDING, <1.0 MEETING, <1.2 WARNING, <1.5 VIOLATING, else CRITICAL; routing_factor = 1 + 0.4·(composite−1) clamped [0.5, 3.0] (35021–35034, 35380–35413).
- Health: composite = min(manager, resource, slo) (35436). p99 ≥ 5× for 300 s → UNHEALTHY; p95 ≥ 2× or p99 ≥ 3× for 180 s → DEGRADED; p50 ≥ 1.5× for 60 s → BUSY (35037–35045). No hysteresis inside classifier; delegated to AD-36 (35870).
- Routing score = rtt_ucb × load × quality × resource × slo × pref (35626–35628); load = 1 + 0.5·util + 0.3·q/(q+10) + 0.2·cb, cap 5 (35658–35670); quality cap 2; resource = 1 + 0.3·cpu + 0.2·mem cap 2.5; pref 0.9 (35673–35695).

### 5. Capacity-aware spillover (AD-43, 35929–36918)
- 1 workflow per core; manager keeps active dispatches + pending queue; gate aggregates per DC (35955–36014). `estimate_wait_for_cores` simulates cores freeing by expected completion (36108–36145). Heartbeat gains capacity fields, TCP every 10 s (36169–36222, 36646). Gate aggregation tick 5 s; data >30 s old → bucket routing (36604–36608).
- Decision: primary has ≥N free → primary; primary wait ≤ 60 s → queue; fallbacks with immediate capacity and rtt−primary_rtt ≤ 100 ms → lowest penalty; require spillover_wait ≤ 0.5 × primary_wait (36347–36466).

### 6. Adaptive route learning (AD-45, 37724–38790)
- blended = c·observed + (1−c)·rtt_ucb, c = min(1, samples/MIN_SAMPLES) (37792–37794). EWMA with variance: first sample sets ewma=x; then δ = x − ewma; ewma += α·δ; var = (1−α)(var + α·δ²) (37856–37868). Alpha 0.1 (tracker default) vs 0.2 (env default, "recommended") — inconsistency (37905, 38344). Min samples 10; staleness decay max 300 s, factor = max(0, 1 − age/300) (37906–37907, 37944–37949). Only successes recorded (38076–38082); outlier cap 60,000 ms defined but not applied (38357).

### 7. Constants table (range 31380–38790), abbreviated
| Name | Value | Meaning | Ref |
|---|---|---|---|
| pending/committed/rejected TTL | 60 / 300 / 60 s | idempotency cache | 31634–31636 |
| max_entries | 100,000 | cache LRU bound | 31639 |
| cleanup_interval | 10 s | TTL sweep | 31642 |
| pending_wait_timeout | 30 s | | 31646 |
| Kalman Q / R / P₀ | 10 / 25 / 1000 | | 33305–33309 |
| CPU Q/R; memory Q/R | 15/50; 1e6/1e7 | | 33535–33538 |
| metrics staleness | 30 s | | 33516 |
| expected CPU per worker | 400% | pressure denominator | 33969 |
| sigma | 2.0 | 95% CI | 34277 |
| thresholds / grace | 0.8/0.95/1.0; 10/5/2 s | | 34151–34156 |
| default budget | 800% CPU, 16 GiB, 10,000 FDs | | 34193–34195 |
| SLO_TDIGEST_DELTA / MAX_UNMERGED | 100 / 2048 | | 35012–35013 |
| SLO windows | 60 s × 5; eval 300 s | | 35016–35018 |
| SLO targets / weights / min samples | 50/200/500 ms; 0.2/0.5/0.3; 100 | | 35021–35031 |
| SLO factor clamp / weight | [0.5, 3.0] / 0.4 | | 35032–35034 |
| SLO health ratios / windows | 1.5 / 2.0,3.0 / 5.0; 60/180/300 s | | 35037–35045 |
| SPILLOVER_MAX_WAIT / MAX_LATENCY_PENALTY / MIN_IMPROVEMENT | 60 s / 100 ms / 0.5 | | 36588–36596 |
| CAPACITY_STALENESS / AGGREGATION_INTERVAL | 30 s / 5 s | | 36604–36608 |
| RETRY_BUDGET_MAX / PER_WORKFLOW_MAX / DEFAULT / PER_WORKFLOW_DEFAULT | 50 / 5 / 10 / 3 | | 37349–37361 |
| BEST_EFFORT deadline max/default; min_dcs; check | 3600 / 300 s; 1; 5 s | | 37366–37378 |
| ADAPTIVE_ROUTING alpha / min samples / staleness / cap | 0.2 / 10 / 300 s / 60,000 ms | | 38344–38357 |

### 8. Port-relevant caveats
Ledger commit ordering; unenforced ledger max_entries; gate PENDING leak on manager error (32863–32867); missing memory_pressure (33972–33986); vector-clock sum vs any-greater (34014 vs 34038–34041); unused THROTTLE; compliance-band comments (35305–35309); α 0.1 vs 0.2; unapplied latency cap; unused `cores_needed` (36318); trivially-true improvement ratio (36444–36456). T-Digest `merge` by weight expansion (35249–35260) is O(total weight) — use centroid-level merge.
