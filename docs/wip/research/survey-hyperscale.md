# Survey: hyperscale (Python distributed load-testing framework) — patterns for slates

Source tree surveyed: `/Users/adalundhe/Projects/hyperscale` (same author as slates). All `file:line`
references below are relative to that root unless stated otherwise. `docs/architecture.md` is the
38,790-line master document; `docs/architecture/AD_N.md` are the per-decision files (53 of them);
code lives under `hyperscale/distributed/`.

Status: written incrementally. Sections are appended as they are finished.

---

## 0. House rules the source project imposes on itself (CLAUDE.md / AGENTS.md)

The two files are identical (`CLAUDE.md:1-64`, `AGENTS.md:1-65`). The rules that shaped the
distributed design, and that slates has already adopted in spirit:

- "We *never* create asyncio orphaned tasks or futures. Use the TaskRunner instead" (`CLAUDE.md:26`).
  Every background loop is registered with a runner that owns cancellation and cleanup (see AD-2).
- "We always cleanup - if we store long running task data, we clean it up." and "Memory leaks are
  *unnacceptable* period." (`CLAUDE.md:30-31`).
- "For an architectural or implementation decision, we ALWAYS take the most robust approach"
  (`CLAUDE.md:32`).
- "One class per file. Period." and folder-by-role layout (`nodes/`, `swim/`, `models/`) (`CLAUDE.md:33-34`).
- "When creating a class we try to use init state as configuration and avoid mutating it in method
  calls." (`CLAUDE.md:29`) — configuration is immutable after construction.
- "We *do not* EVER swallow errors" (`CLAUDE.md:25`).
- "FORBIDDEN: Do not use threading module items EVER. ALWAYS defer to the asyncio counterpart"
  (`CLAUDE.md:63-64`). Everything is single-event-loop async; the only escape hatch is
  `run_in_executor` for blocking file I/O (see the WAL section).
- The logger is async and must be awaited, never fire-and-forget (`CLAUDE.md:36`).
- Integration tests are written but never run by the agent; a human runs them (`CLAUDE.md:44-46`).

What this means for slates: the source project's discipline is close to slates' rules already
(no orphan tasks, no leaks, cleanup on every path, config-as-init-state). The two places where
hyperscale is materially weaker than slates' bar are (a) it is full of hardcoded numeric defaults
(catalogued in section 8) and (b) it relies on a thread-pool executor for disk I/O, which slates
does not need because it never touches disk.

---

## 0.1 Index of architectural decisions (first heading of every `docs/architecture/AD_*.md`)

| AD | Title | Lines | Relevant to slates topic |
|----|-------|-------|---------------------------|
| 1 | Composition Over Inheritance | 21 | async discipline / extensibility |
| 2 | TaskRunner for All Background Tasks | 20 | async discipline |
| 3 | Quorum Uses Configured Cluster Size | 27 | quorum / partitions |
| 4 | Workers Are Source of Truth | 19 | recovery |
| 5 | Pre-Voting for Split-Brain Prevention | 19 | leader election |
| 6 | Manager Peer Failure Detection | 20 | membership |
| 7 | Worker Manager Failover | 19 | failover |
| 8 | Cores Completed for Faster Provisioning | 19 | (scheduling; not relevant) |
| 9 | Retry Data Preserved at Dispatch | 19 | retries/idempotency |
| 10 | Fencing Tokens from Terms | 19 | fencing |
| 11 | State Sync Retries with Exponential Backoff | 20 | retries |
| 12 | Manager Peer State Sync on Leadership | 20 | recovery |
| 13 | Gate Split-Brain Prevention | 21 | leader election |
| 14 | CRDT-Based Cross-DC Statistics | 33 | cross-DC state |
| 15 | Tiered Update Strategy for Cross-DC Stats | 27 | backpressure |
| 16 | Datacenter Health Classification | 50 | degradation |
| 17 | Smart Dispatch with Fallback Chain | 68 | routing / degradation |
| 18 | Hybrid Overload Detection (Delta + Absolute) | 147 | backpressure / measured params |
| 19 | Three-Signal Health Model (All Node Types) | 470 | failure detection / LHM gossip |
| 20 | Cancellation Propagation | 61 | cancellation |
| 21 | Unified Retry Framework with Jitter | 115 | retries |
| 22 | Load Shedding with Priority Queues | 95 | load shedding |
| 23 | Backpressure for Stats Updates | 76 | backpressure |
| 24 | Rate Limiting (Client and Server) | 86 | admission control |
| 25 | Version Skew Handling | 80 | protocol evolution |
| 26 | Adaptive Healthcheck Extensions | 440 | adaptive deadlines / LHM |
| 27 | Gate Module Reorganization | 68 | (code layout) |
| 28 | Enhanced DNS Discovery with Peer Selection | 419 | bootstrap / discovery |
| 29 | Protocol-Level Peer Confirmation for Robust Initialization | 248 | bootstrap / SWIM |
| 30 | Hierarchical Failure Detection for Multi-Job Distributed Systems | 505 | SWIM / timers / LHM |
| 31 | Gossip-Informed Callbacks for Failure Propagation | 187 | SWIM |
| 32 | Hybrid Bounded Execution with Priority Load Shedding | 525 | backpressure / bounded queues |
| 33 | Federated Health Monitoring for Cross-DC Coordination | 234 | cross-DC failure detection |
| 34 | Adaptive Job Timeout with Multi-DC Coordination | 559 | leases / timeouts / fencing |
| 35 | Vivaldi Network Coordinates with Role-Aware Failure Detection | 642 | measured timeouts |
| 36 | Vivaldi-Based Cross-Datacenter Job Routing | 267 | routing |
| 37 | Explicit Backpressure Policy (Gate -> Manager -> Worker) | 82 | backpressure |
| 38 | Global Job Ledger with Per-Node Write-Ahead Logging | 338 | WAL / ledger / consensus |
| 39 | Logger Extension for AD-38 WAL Compliance | 1783 | WAL / async I/O |
| 40 | Idempotent Job Submissions | 273 | idempotency |
| 41 | Resource Guards - CPU/Memory Monitoring and Enforcement | 160 | memory limits |
| 42 | SLO-Aware Health and Routing | 269 | measured params |
| 43 | Capacity-Aware Spillover and Core Reservation | 174 | admission |
| 44 | Retry Budgets and Best-Effort Completion | 153 | retries |
| 45 | Adaptive Route Learning | 218 | measured params |
| 46 | SWIM Node State Storage via IncarnationTracker | 220 | SWIM |
| 47 | Worker Event Log for Crash Forensics and Observability | 606 | bounded logs |
| 48 | Cross-Manager Worker Visibility via TCP Broadcast and Gossip Piggyback | 637 | gossip |
| 49 | Workflow Context Propagation in Distributed Jobs | 312 | (job semantics) |
| 50 | Manager Health Aggregation and Alerting | 218 | health |
| 51 | Unified Health-Aware Routing Integration | 1123 | routing |
| 52 | Cluster Creation | 1143 | bootstrap |
| 53 | AD-30 Cross-Layer Death Escalation | 298 | zombie prevention |

---

## 1. Node topology, transports, framing, and the SWIM/Lifeguard mechanics as built

### 1.1 Topology

Three tiers plus a client (`docs/architecture.md:58-93`, per helper report A):

- **Client** submits over TCP to the gate that owns the job (or directly to a manager when no
  gates exist, `docs/architecture.md:8363-8364`) and may run its own TCP listener for pushes.
- **Gates** (optional tier) coordinate across datacenters. Each gate runs SWIM over UDP with its
  peer gates, elects a cluster leader, and owns jobs per-job through a consistent hash ring
  (`hyperscale/distributed/jobs/gates/consistent_hash_ring.py:37,50` — MD5 with 150 virtual
  nodes, `bisect` lookup at `:82-84,117-118`). Gate loops: lease cleanup (`nodes/gate/server.py:6356`),
  job cleanup (`:6452`), rate-limit cleanup (`:6468`), batch stats (`:6479`), windowed stats push
  (`:6497`), resource sampling (`:6511`), discovery maintenance (`:6609`), dead-peer reap (`:6632`),
  peer readmission (`:6732`), orphan check (`nodes/gate/orphan_job_coordinator.py:369`).
- **Managers** (one Raft/SWIM cluster per DC) dispatch workflows, track workers, and are the
  "durability boundary" (`docs/architecture/AD_38.md:27-33`). `nodes/manager/server.py` is 10,914
  lines; its background loops are: peer registration sync (`:1392`), dead-node reap (`:2846`),
  orphan scan (`:2954`), job responsiveness (`:3000`), stats push (`:3131`), windowed stats flush
  (`:3154`), gate heartbeat (`:3216`), rate-limit cleanup (`:3286`), job cleanup (`:3323`),
  unified timeout (`:3379`), deadline enforcement (`:3437`), peer job-state sync (`:3698`),
  resource sampling (`:3735`).
- **Workers** execute load; they never participate in consensus or ack paths
  (`AD_38.md:19,31-33`). Worker loops: pending-result retry (`nodes/worker/server.py:997`),
  resource sampling (`:1022`), manager rejoin watch (`:1036`), pool health (`:1111`), plus
  `nodes/worker/background_loops.py:109-358` (dead-manager reap, orphan check, discovery
  maintenance, progress flush).
- Role-based connection matrix (mTLS SAN claims): Client→Gate; Gate→Manager/Gate/Client;
  Manager→Worker/Manager/Gate/Client; Worker→Manager only (`AD_28.md:136-155`).

Ownership model: every worker has exactly one owner manager; other managers see it as "remote"
with reduced trust, learned via TCP broadcast for critical events and gossip for steady state
(`AD_48.md:9,60-79`). Every job has exactly one leader gate (ring + lease) and one leader manager
per DC (`docs/architecture.md:11543-11555`, helper report range 8406–14200 §1).

### 1.2 Transports and framing

- **UDP carries SWIM only**: probe/ack/ping-req, membership gossip, leadership messages, and the
  Serf-style embedded heartbeats (`docs/architecture.md:4567-4583`). **TCP carries data**: job
  submission, dispatch, progress, state sync, cancellation (`:4587-4600`).
- **Wire format (both transports)**: after decrypt+decompress the payload is
  `address<handler<clock(64B)data_len(4B)data(N B)` for TCP
  (`hyperscale/distributed/server/server/mercury_sync_base_server.py:1007,1794`) and
  `type<address<handler<clock(64B)data_len(4B)data(N B)` for UDP (`:1120,1603,1999`). The
  64-byte "clock" is the Lamport/versioned clock stamp.
- **Receive pipeline** (TCP `process_tcp_server_request`, `:1756-1830`; UDP `:1900-2030`):
  per-peer rate limit (`:1766`) → size cap `MAX_MESSAGE_SIZE = 3 MiB`
  (`hyperscale/core/jobs/protocols/constants.py`) → AES-256-GCM decrypt (`:1776`) → zstd
  decompress (`:1781`) → compression-bomb check: decompressed ≤ 5 MiB and ratio ≤ 100
  (`server/protocol/security.py:108-139`) → header split (`:1795`) → replay guard
  (`:1823`; `core/jobs/protocols/replay_guard.py:41-44`: max age 300 s, max future 60 s,
  100,000-id window, 10,000 sender incarnations) → handler lookup → priority admission
  (`ProtocolInFlightTracker`, `:243`) → task spawn (`_spawn_tcp_response`, `:1271`). Every drop
  reason is counted (`server/protocol/drop_counter.py`).
- **Encryption**: `encryption/aes_gcm.py:2-16,155-191` — HKDF-SHA256 from a shared secret with a
  random 16-byte salt per message, 12-byte nonce, frame `salt|nonce|ciphertext|tag`; key rotation
  via `MERCURY_SYNC_AUTH_SECRET_PREVIOUS` (`env.py:19-20`). TLS restricted to ECDHE-AES256-GCM
  suites (`mercury_sync_base_server.py:733,761,969`); cert verification REQUIRED and hostname
  verification on by default (`env.py:30-34`).
- **Serialization**: cloudpickle for every message (`models/message.py:146-148`), loaded through a
  `RestrictedUnpickler` allowlist (`models/restricted_unpickler.py:111,301,412-441`). A per-process
  random `MESSAGE_INCARNATION` (8 bytes, `message.py:55`) feeds replay detection.
- **SWIM message grammar** (bytes, colon/`>` delimited, `swim/core/constants.py:22-40`):
  `probe:{request_id}>{host:port}` (`swim/health_aware_server.py:5959-5966`),
  `ping-req:{incarnation}:{request_id}>{target}` (`:5856-5863`),
  `alive:{incarnation}:{node_id}>{self}` (`:6016-6023`), `suspect:{incarnation}>{target}`
  (`:6129`), `join`/`leave`; leadership: `pre-vote-req:{term}:{lhm}>{addr}`
  (`swim/leadership/local_leader_election.py:477-482`), `pre-vote-resp:{term}:{0|1}>{cand}`
  (`:822-828`), `leader-claim:{term}:{lhm}>{addr}` (`:577-582`), `leader-vote:{term}>{cand}`
  (`:721-726`), `leader-elected:{term}>{addr}` (`:619-623`),
  `leader-heartbeat:{term}:{seq}:{lease_ms}>{addr}` (`:657-663`), `leader-stepdown:{term}>{addr}`
  (`:675-679`). Request ids are `{node}-{monotonic_ns}-{seq}` — deterministic, not random
  (`health_aware_server.py:5938-5953`).
- **Piggyback multiplexer**: seven channels appended to any SWIM datagram and parsed right-to-left:
  `#|m` membership, `#|s` state (heartbeat embed), `#|h` health (AD-19), `#|w` worker state
  (AD-48), `#|x` extension decisions, `#|o` extension outcomes, `#|v` Vivaldi
  (`swim/admission.py:22-30`, `AD_26.md:419-432`, `AD_48.md:626-637`). MTU budget:
  `MAX_UDP_PAYLOAD = 1400`, `MAX_PIGGYBACK_SIZE = 1200` (`swim/gossip/gossip_buffer.py:25-26`);
  default 5 updates per message, hard cap 100 encode/decode (`:156-176,187,275`).
- The full TCP message catalog (JobSubmission, WorkflowDispatch, WorkflowProgress, Provision*,
  StateSync*, DatacenterLease, LeaseTransfer, cancellation and extension messages) is tabulated in
  helper report A §1.3 with `docs/architecture.md:7705-7897` references; AD-25 protocol versioning
  is MAJOR.MINOR with capability sets (`AD_25.md:17-44`).

### 1.3 Probe cycle (as implemented in `swim/health_aware_server.py`)

1. `start_probe_cycle` (`:3723-3777`): starts the hierarchical detector, event-loop health
   monitor and cleanup loop; `protocol_period = udp_poll_interval` (env `SWIM_UDP_POLL_INTERVAL = 1`
   s, `env/env.py:60`); loops `_run_probe_round` then sleeps one period. A `CancelledError` is
   treated as shutdown only if the task's own `cancelling()` count is > 0 — otherwise it is a
   stray cancel from an inner awaitable and the cycle continues (`:3754-3774`). This is a hard-won
   fix: an earlier version silently killed the node's whole failure detector.
2. `_run_probe_round` (`:3779-3881`): if the NETWORK circuit breaker is open, skip the round and
   sleep 1.0 s (hardcoded, `:3786-3791`). Target comes from `ProbeScheduler`, a randomized
   round-robin over an immutable tuple that is swapped atomically on membership change
   (`swim/detection/probe_scheduler.py:28-133`; shuffle routed through the injectable `Random`).
3. Direct probe budget: `base_timeout` = context `current_timeout` (env `SWIM_CURRENT_TIMEOUT = 1`
   s) → `get_lhm_adjusted_timeout` (`:4757-4857`) → `_probe_with_timeout` (`:4237-4373`) which
   retries within a continuous budget from `_compute_direct_probe_budget` (`:4149-4235`):
   `budget = base × (1 + max_extra × warrant × inhibition)` with
   `warrant = (1 − (1 − peer_load_noise)(1 − 1/log2(n))) × peer_reliability`,
   `inhibition = 1 / lhm_multiplier`, `max_extra = lhm_max_multiplier − 1 = 2.0`; so the budget is
   strictly in `[base, 3·base]`. There are "no magic retry counts" (`:4250-4253`) — this is the one
   place where the codebase already does what slates' rules demand. Ack waiting uses a shared
   per-target future wrapped in `asyncio.shield` so concurrent probes to one target (burst mode)
   cannot cancel each other (`:4297-4330`). Sub-microsecond remainders count as expiry
   (`protocol/time_quantum.py:28`, `TIME_REMAINDER_EPSILON_SECONDS = 1e-6`) to avoid a
   frozen-instant livelock observed under simulation.
4. On ack: LHM −1, per-peer reliability record success (sliding window of 8 samples, 60 s TTL,
   ≤10,000 peers, `swim/detection/peer_probe_reliability_tracker.py:56-58`), burst window reset.
5. On timeout: reliability failure recorded; `initiate_indirect_probe` (`:5824-5895`) picks
   `k = 3` proxies (`swim/detection/indirect_probe_manager.py:34`; `max_pending = 100`, TTL 30 s
   at `:37-40`), preferring peers not reported stressed (`:5672-5733`), replacing proxies whose
   send fails; then waits `timeout` and checks for a forwarded ack.
6. Still no ack: LHM +1 only if the target is not already SUSPECT/DEAD (`:3871`; rationale in
   `AD_30.md:485-505` — probing a dead peer is evidence about the peer, not about us), then
   `start_suspicion` (`:5518-5625`), which refuses unregistered or unconfirmed peers
   (`:5542-5550`, AD-29), calls `HierarchicalFailureDetector.suspect_global`, moves the tracker to
   SUSPECT, queues a `suspect` gossip update and broadcasts `suspect` to **all** peers including
   the target, concurrently via `gather` (`:6116-6182`; the target must hear it to refute).
7. AD-53 burst mode (`:3895-4086`): every full direct+indirect failure is appended to a 30 s
   window; when ≥ 2 distinct targets failed (`env.py:79-80`) a bounded-parallel confirmation of all
   other registered, confirmed, non-terminal members runs once (semaphore sized from threshold ×
   fan-out, `AD_53.md:229-231`); each candidate still goes direct → indirect → SUSPECT, and a
   failed confirmation only counts as "burst-dead proof" if a witness proxy was actually consulted
   (`:4068-4084`).

### 1.4 Suspicion, confirmations, refutation, incarnation

- Suspicion timer (`swim/detection/suspicion_state.py:111-139`):
  `timeout = max − (max − min) × log(C+1) / log(K+1)`, `C` = confirmations from **other** nodes
  (the originator's own vote is excluded, `:56,81-84`), `K = required_confirmations` (default 2,
  `swim/detection/hierarchical_failure_detector.py:72`) or `n_members`. Confirmer set capped at
  1,000 with a logical counter beyond that (`:16,71-99`); re-gossip factor 3 (`:63`).
- Brackets: managers `SWIM_SUSPICION_MIN/MAX_TIMEOUT = 1.5 / 8.0` s (`env.py:61-66`); gates
  `30 / 120` s because a false-positive gate death re-homes every job it owns (`env.py:409-421`);
  `HierarchicalConfig` defaults 5 / 30 s (`hierarchical_failure_detector.py:69-70`); no-witness
  suspicion 30 s (`env.py:67`). The bracket is then multiplied by `self_lhm ∈ [1,3]`,
  `peer_load ∈ [1,2.5]` and `vivaldi_quality ∈ [1,1.5]` — worst case 11.25× (`AD_30.md:444-483`).
- Timers never reschedule on confirmation (the AD-30 "timer starvation" fix): global layer is a
  two-level timing wheel (1000 ms coarse / 100 ms fine, invariant `coarse == fine × wheel_size`,
  `swim/detection/timing_wheel.py:92-105`); job layer is one adaptive poll task per suspicion at
  1000 / 250 / 50 ms as the deadline nears, slowed up to 3× by LHM
  (`swim/detection/job_suspicion_manager.py:38-55`). Reconciliation every 5 s; caps 10,000 global,
  1,000 per job, 50,000 total (`hierarchical_failure_detector.py:88-93`).
- Refutation (`health_aware_server.py:5968-6055`): rate-limited to 5 per 10 s window
  (`env.py:83-84`, anti incarnation-exhaustion), bumps own incarnation, sends `alive` to every
  known peer with `PROBE_RETRY_POLICY` (3 attempts, 0.1 s base, 2 s cap, 15 % jitter,
  `swim/core/retry.py:149-153`). A stopping instance never refutes (`:5985-5986,6010-6011`).
  Third-party piggybacked `alive` cannot clear a locally-open suspicion; only a direct `alive`
  from the target or a fresh direct/indirect confirmation can (`AD_53.md:239-245`).
- Incarnation state is the single source of truth (`AD_46.md`): `IncarnationTracker.node_states`
  (`max_nodes = 10,000`, dead retention 3600 s, `swim/detection/incarnation_tracker.py:79-80`),
  conflict rule "higher incarnation wins; same incarnation DEAD > SUSPECT > OK > UNCONFIRMED".
  Zombie rejection: a node marked dead at incarnation `d` must rejoin with ≥ `d + bump` within a
  window (test uses window 60 s, bump 5, `tests/integration/swim/test_failure_scenarios.py:69-73`);
  `IncarnationStore` persists incarnations to disk with a restart bump of 10 (`:161-166`) — a
  disk dependency slates cannot copy (AD-52 §3 instead makes node ids ephemeral).
- Gossip-informed callbacks (`AD_31.md`): a `dead`/`leave` learned by gossip fires the same
  `_on_node_dead` callbacks as direct detection, once, on the NOT-DEAD → DEAD edge.

### 1.5 Local Health Multiplier and degradation

- `LocalHealthMultiplier` (`swim/health/local_health_multiplier.py:28-36,97-132`): score 0–8, every
  event ±1 (probe timeout, refutation needed, missed nack, event-loop lag; successful probe/nack,
  loop recovered). `multiplier = 1 + 0.25 × score ∈ [1.0, 3.0]` — deliberately below the
  Lifeguard paper's `LHM + 1 ∈ [1, 9]` (docstring `:104-125`).
- Event-loop lag monitor (`swim/health/health_monitor.py:66-98`): sleeps 10 ms every 1.0 s and
  measures overshoot; lag ratio > 0.5 counts as lagging, > 2.0 critical; 3 consecutive lags →
  degraded, 5 clean samples → recovered; each fires LHM ±1.
- `get_lhm_adjusted_timeout` (`health_aware_server.py:4757-4857`) composes four reliability
  signals by prob-OR instead of multiplying: `timeout = base × latency_multiplier × (1 + 2.0 × U)`,
  `U = 1 − ∏(1/m_i)` over LHM, degradation, Vivaldi coordinate quality
  (`1 + (1−q) × 0.5`) and peer load (busy 1.25 / stressed 1.75 / overloaded 2.5,
  `swim/health/peer_health_awareness.py:115-117`); `latency_multiplier = clamp(rtt_ucb / 10 ms,
  1, 10)`. The docstring records that the previous multiplicative form blew up "> 90×".
- Graceful degradation: five levels NORMAL/LIGHT/MODERATE/HEAVY/CRITICAL, each with a policy
  `probe_rate, gossip_rate, max_piggyback_updates, timeout_multiplier, should_step_down,
  refuse_leadership, skip_indirect_probing` (`swim/health/graceful_degradation.py:26-80`);
  leaders refuse/step down when degraded; `LEADER_MAX_LHM = 4` gates election eligibility
  (`env.py:92-94`).
- Health gossip (AD-19 Phase D): every heartbeat carries the raw LHM score; managers forward
  `max` worker LHM to gates so cross-DC correlation can tell control-plane stress from data-plane
  overload (`AD_19.md:411-470`).

### 1.6 Gossip dissemination budget

`GossipBuffer` (`swim/gossip/gossip_buffer.py:44,99-152,327-398`): each update is re-sent
`max(1, ⌊3 × ln(n+1)⌋)` times (λ = 3), least-broadcast first via `heapq.nsmallest`; buffer capped
at 1,000 updates, stale after 60 s, overflow evicts the 10 oldest and fires a callback; same
incarnation resolves by priority `dead/leave > suspect > alive/join`. The AD-48 worker-state,
AD-26 extension, and AD-35 Vivaldi buffers copy this discipline (`AD_48.md:135-152`: cap 500,
stale 60 s, 600-byte piggyback share).

### 1.7 Peer confirmation, UNCONFIRMED lifecycle, roles, Vivaldi

- AD-29: a peer can only be suspected after at least one successful bidirectional exchange;
  config/DNS/gossip knowledge does not confirm (`AD_29.md:126-137`). Registration is an
  additional gate (`health_aware_server.py:5534-5544`). Unconfirmed peers are checked by the
  cleanup loop (`:2284`, run from `:2159-2238`) with role-specific passive timeouts
  (`swim/roles/confirmation_strategy.py:33-64`): gate 120 s, 5 proactive attempts 5 s apart, load
  cap 3×; manager 90 s, 3 attempts, 5×; worker 180 s, never probed proactively, 10×.
- Vivaldi (`swim/coordinates/coordinate_engine.py:18-25`): 8 dimensions, `ce = 0.25`,
  `error_decay = 0.25`, gravity 0.01, height adjustment 0.25, smoothing 0.05, error clamp
  [0.05, 10.0] — Serf's defaults. RTT estimates are upper-confidence-bound and clamped, and a
  missing coordinate never excludes a peer (`AD_35.md:435-466`). Federated (gate→DC leader)
  probing uses its own slower constants: 2 s interval, 5 s timeout, 30 s suspicion, 5 failures
  (`env.py:124-133`).

### 1.8 Where each SWIM parameter comes from

| Parameter | Value | Source | Origin |
|---|---|---|---|
| protocol period | 1 s | `env.py:60` | hardcoded env default |
| direct probe base timeout | 1 s | `env.py:59` | hardcoded env default |
| probe min/max timeout | 1 / 5 s | `env.py:57-58` | hardcoded (unused by the continuous budget) |
| direct-probe budget | `[base, 3·base]` | `health_aware_server.py:4149-4235` | **derived** from n, LHM, peer load, reliability |
| indirect proxies k | 3 | `indirect_probe_manager.py:34` | hardcoded |
| suspicion bracket (manager) | 1.5 / 8 s | `env.py:61-66` | hardcoded |
| suspicion bracket (gate) | 30 / 120 s | `env.py:418-421` | hardcoded, justified by fan-out cost |
| suspicion formula | log(C+1)/log(K+1) | `suspicion_state.py:135-139` | **derived** from confirmations |
| required confirmations K | 2 | `hierarchical_failure_detector.py:72` | hardcoded |
| LHM range / weight | 0–8, 0.25 | `local_health_multiplier.py:29,36` | hardcoded |
| gossip λ | 3 | `gossip_buffer.py:44` | hardcoded; broadcasts derived from n |
| piggyback budget | 1200 / 1400 B | `gossip_buffer.py:25-26` | hardcoded from Ethernet MTU |
| refutation limit | 5 per 10 s | `env.py:83-84` | hardcoded |
| burst threshold / window | 2 / 30 s | `env.py:79-80` | hardcoded |
| timing wheel ticks | 1000 / 100 ms | `hierarchical_failure_detector.py:80-81` | hardcoded |
| job poll intervals | 1000/250/50 ms | `job_suspicion_manager.py:42-48` | hardcoded |
| suspicion caps | 10k / 1k / 50k | `hierarchical_failure_detector.py:91-93` | hardcoded |
| dead retention | 3600 s | `incarnation_tracker.py:80` | hardcoded |
| role passive timeouts | 120/90/180 s | `confirmation_strategy.py:35,46,57` | hardcoded |
| Vivaldi ce/decay/dims | 0.25/0.25/8 | `coordinate_engine.py:18-25` | hardcoded (Serf) |
| RTT reference | 10 ms | `health_aware_server.py:4820` | hardcoded |
| event-loop lag sample/expected | 1 s / 10 ms | `health_monitor.py:66-69` | hardcoded |
| job-layer escalation gate | 2 × protocol period | `nodes/manager/server.py:3096-3104` | **derived** from config |

## 2. Leader election, Raft, and the WAL/ledger

Hyperscale has three different "consensus" layers that coexist, plus one that is only designed:

1. **SWIM-tier lease election** (`LocalLeaderElection`) — picks one cluster leader per manager
   cluster and per gate cluster. Used today.
2. **Per-job Raft groups** (`raft/raft_node.py`) — one in-memory Raft group per job, all cluster
   nodes participate. Used today for job-state mutations.
3. **Per-node WAL + tiered ledger** (`ledger/`) — durability tiers LOCAL/REGIONAL/GLOBAL. The
   WAL is real; the regional/global replication layers are partially designed (AD-38 Part 14
   proposes per-job Viewstamped Replication instead of Raft).
4. **AD-52 cluster creation** (proposed 2026-05-12): joint consensus, learners, fencing header,
   watch streams — the state-of-the-art plan the author wants to converge on.

### 2.1 SWIM-tier lease election (`swim/leadership/local_leader_election.py`, `leader_state.py`)

- Constants: `heartbeat_interval = 2.0 s`, `election_timeout_base = 5.0 s + uniform(0, 2.0)`,
  `pre_vote_timeout = 2.0 s`, `lease_duration = 5.0 s` (`local_leader_election.py:53-56`,
  `leader_state.py:57`, mirrored by `env.py:87-91`); first-election jitter up to 3.0 s
  (`env.py:118-120`); `MAX_TERM = 2^53 − 1`, `MAX_VOTES = 1000` (`leader_state.py:16-20`).
- Loop (`:324-392`): a leader steps down when its LHM exceeds `LEADER_MAX_LHM = 4` **and** there is
  another member to hand off to (`:234-247`, a guard added after a single-node cluster went
  leaderless); otherwise it waits `heartbeat_interval` (event-driven wait with a 1 ms floor to
  avoid a sub-quantum livelock, `:291-322`) and sends a heartbeat. A follower whose lease view has
  expired consults the flapping detector for a cooldown, checks eligibility (LHM ≤ 4 and not
  degraded), then runs pre-vote → election.
- Pre-vote (`:437-525`): term is not incremented; needs `⌊n/2⌋ + 1` grants where `n` is the SWIM
  **member count** (not a configured size — this contradicts AD-3, which the Raft layer honours;
  `:501-503`); aborts if a valid leader heartbeat arrived meanwhile or the term moved. Grants are
  refused while the grantor holds a valid lease or if the candidate's LHM is too high
  (`leader_state.py:344-371`).
- Election (`:527-640`): `next_term()` with exhaustion check, vote for self, broadcast
  `leader-claim` carrying LHM, wait the jittered timeout, tally `⌊n/2⌋+1`; on win broadcast
  `leader-elected` and immediately heartbeat. Voters grant at most one vote per term
  (`leader_state.py:288-300`) and refuse candidates above the LHM cap (`:703-712`).
- Heartbeats are `(term, seq, lease_ms)`: followers apply beats only if `(term, seq)` strictly
  advances and adopt the **leader's** lease length, so replayed/reordered beats cannot extend a
  lease and leader/follower cannot disagree on lease duration (`leader_state.py:236-281`).
- Split-brain healing: higher term wins; equal term → lower address wins
  (`leader_state.py:400-421`, `local_leader_election.py:856-878`).
- Fencing token = current term (leader) or `leader_term` (follower); an operation is valid if
  `token >= current_term` (`leader_state.py:374-397`, AD-10). Job-level leadership keeps its own
  fence tokens that increment on every takeover (`tests/integration/raft/test_raft_leadership_failover.py:341-397`).
- Flapping detector escalates the election cooldown after repeated failures
  (`swim/leadership/flapping_detector.py`), and `docs/SCENARIOS.md:35-44` lists the scenarios it
  must survive (LHM-driven step-down, concurrent candidates, pre-vote during a stable lease,
  term exhaustion).

### 2.2 Per-job Raft (`raft/raft_node.py`, `raft_consensus.py`)

- Timing: `ELECTION_TIMEOUT_MIN/MAX = 150/300 ms`, `HEARTBEAT_INTERVAL = 50 ms` (`raft_node.py:36-38`);
  `proposal_timeout_seconds = 5.0` (`:128`). A single tick loop in `RaftConsensus` drives every
  node every 50 ms via the TaskRunner (`raft_consensus.py:103-143`); replication is serial per
  follower, not pipelined (`docs/AD_52_PLAN.md:13`).
- Quorum uses the **configured** cluster size (`:160-165,646-648`, AD-3) — a partition of 1-of-3
  can never elect.
- Persistence: `current_term`, `voted_for` and the log are **volatile by design** — the docstring
  (`:49-82`) explains that AD-52 forbids any local-disk dependency, that per-job groups die with
  the job, that the AD-38 ledger is the durable record, and that the double-vote window after a
  restart is contained by fence tokens at the dispatch boundary. `RaftWAL`
  (`raft/raft_wal.py:8,36-38`: `[crc32][len][term][index][payload]`, 24-byte header; group commit
  500 µs / 500 entries / 4 MiB / queue 5000, `:150-154`) and `SnapshotManager`
  (`raft/snapshot.py:121`: compaction threshold 10,000 entries; `InstallSnapshot` messages defined
  at `:30-69`) exist and are tested but are not wired (`docs/AD_52_PLAN.md:14-17`).
- Log: `RaftLog(max_entries=50_000)` (`raft/raft_log.py:31`); when at capacity `propose` returns
  `(False, 0)` (`raft_node.py:523`). Conflict backtracking by `(conflict_term, conflict_index)`
  (`:420-433,480-486`); commit index advances only through current-term entries (`:488-499`).
- Proposals: a Future per index resolved when the entry is **applied** (`:560-581`); step-down
  fails all pending proposals (`:634-655`); `destroy()` clears everything (`:615-628`). Entry
  timestamps are minted from the shared `HybridLamportClock` so followers apply identical values
  (`:505-539`; `docs/AD_52_PLAN.md:71-115` removed four `time.monotonic()` calls from apply
  handlers and added a byte-equality replay test).
- Bounds: `max_instances = 10_000` Raft groups; `create_job_raft` returns False beyond that
  (`raft_consensus.py:69,150-161`); membership events are replicated through Raft with a
  1,000-entry history per job (`raft/replicated_membership_log.py:75,218-221`).

### 2.3 WAL and ledger (`ledger/wal/*`, `logging/lsn/*`)

- Entry format (`ledger/wal/wal_entry.py:15-16,80-131`): 34-byte big-endian header
  `CRC32(4) | length(4) | LSN(8) | HLC(16) | state(1) | type(1)` + payload; CRC covers everything
  after the CRC field; `from_bytes` raises on CRC mismatch.
- `WALWriter` (`ledger/wal/wal_writer.py`): asyncio-native group commit. The writer task waits up
  to 500 µs for the first request, then drains up to 1,000 entries or 1 MiB (`:77-81,406-438`),
  performs one `append_fsync` through the filesystem seam, and resolves every future
  (`:440-480`). Backpressure is a `RobustMessageQueue` (primary 10,000, overflow 1,000, thresholds
  0.70/0.85/0.95, `:82-88`); rejected submits fail fast with `WALBackpressureError` carrying the
  suggested delay (`:253-305`); state changes are coalesced into one callback task (`:347-385`);
  `stop()` drains with a 5 s grace then cancels (`:221-251`); any background-task exception is
  captured as the writer's terminal error (`:180-204`).
- `NodeWAL` (`ledger/wal/node_wal.py`): dense LSN counter, one HLC tick per append (`:221`), entry
  state machine PENDING → REGIONAL → GLOBAL → APPLIED with explicit `TransitionResult` codes
  (`:271-338`), compaction of APPLIED entries up to a watermark (`:340-364`), replicated-tier
  watermarks restored from checkpoints so they never move backwards (`:449-462`). Recovery reads
  the whole file, parses frames, stops at the first short or CRC-bad frame (torn tail), witnesses
  every HLC, and re-queues entries below APPLIED (`:142-205`). Weakness: every append copies the
  whole pending dict into a `MappingProxyType` snapshot (`:255-257`) — O(pending) per append.
- Clock (`logging/lsn/hybrid_lamport_clock.py:48-79`, `lsn.py:12-17,78-88`): an LSN is
  `(logical_time 48 b, node_id 16 b, sequence 8 b, wall_clock 40 b)` packed into 16 bytes; order is
  logical → node_id → sequence, and wall time is explicitly **not** used for ordering. `generate`
  increments the logical counter on every call; `receive` sets `logical = remote + 1`. This is a
  Lamport clock with a wall-clock annotation, not a true HLC (no `max(physical, remote)` merge and
  no drift bound) — the AD-38 text specifies the real HLC rules (`docs/architecture.md:21160-21212`).
- Durability tiers and commit pipeline (`AD_38.md:260-284`): job create/cancel/complete → GLOBAL
  (50–300 ms), workflow dispatch/complete → REGIONAL (2–10 ms), progress → LOCAL (<1 ms), stats →
  NONE. Regional wait 5 s, global wait 30 s; checkpoint every 100,000 entries or 300 s, keep 3
  (helper report range 20164–31380 §1g,§3). The manager's reap loop is what drives
  `maybe_checkpoint` — "without a caller the WAL never compacts" (`nodes/manager/server.py:2869-2882`).
- Conflict resolution across regions is deterministic: cancellation wins > higher fence token >
  HLC order > lexicographic node id (`AD_38.md:305-311`).
- Data-plane logger (`logging/streams/logger_stream.py`): bounded `asyncio.Queue` (`:149-150`),
  drops with a warning when full (`:636,650`), FLUSH mode by default, FSYNC_BATCH at 10 ms / 100
  entries (`:176-177`), one executor hop per write.

### 2.4 The alternatives the author designed and what they decided

- **Per-job VSR instead of Raft** (`docs/architecture.md:23181-24711`): fence token = view, ring
  backups = replica set, lease expiry = view change; `replica_count 3, quorum 2,
  prepare_timeout 5 s, lease TTL 10 s, renewal 3 s`. Helper report C §1.11 lists the defects in
  the sketch (unbounded `prepare_log`, no quorum on view change, swallowed prepare timeouts,
  `expected_seq` reset race). Not implemented.
- **Raft plan `WAL.md`** (207 lines): the earlier Raft-from-scratch plan (election 150–300 ms,
  heartbeat 50 ms, tick 10 ms; `WAL.md:51`) that produced today's `raft/` package.
- **AD-52 cluster creation** (`AD_52.md`, `docs/AD_52_PLAN.md`): deterministic bootstrap via
  `--initial-members` with a sha256 of the sorted list; joint consensus for every membership
  change; learners promoted when log lag ≤ 256 entries, evicted after 30 min; a 40-byte
  `ClusterRPCFence {cluster_uuid, membership_epoch, sender_node_id, sender_term}` validated
  before any handler runs (rejections WRONG_CLUSTER / STALE_MEMBER / STALE_TERM /
  STALE_MEMBERSHIP); SWIM DEAD → 10 min tombstone → Raft REMOVE; phi-accrual per edge (threshold 8,
  warn 4) seeded from Vivaldi RTT; watch streams with a 16,384-entry ring for resume; disconnected
  mode after 30 s with bounded-staleness reads (5 s dispatch, 10 s routing, linearizable cancel);
  ReadIndex with an opt-in leader lease of `quorum_timeout/2`; snapshot every 10,000 entries;
  ≤ 256 in-flight AppendEntries per follower; ephemeral `uuid4` node ids ("restarts are new
  joins, never rejoins", `AD_52.md:140-148`); an apply-layer determinism boundary (no wall clock,
  no randomness, sorted iteration, no I/O, `AD_52.md:700-719`).
- Why these choices (quoted rationale): pre-vote "doesn't increment term (prevents term
  explosion)" (`AD_5.md:12`); quorum by configured size "A partition with 1 of 3 managers won't
  think it has quorum" (`AD_3.md:12-14`); fencing from terms "Workers can reject stale leader
  operations" (`AD_10.md:12-14`); workers as source of truth so a "New leader can recover without
  distributed log" (`AD_4.md:12-14`); single writer over locks because "the correct concurrency
  primitive is not locks — it's queues" (`docs/architecture.md:29869-29882`); ephemeral identity
  because "Stable identity is an optimization, not a correctness requirement" (`AD_52.md:145`).

## 3. Failure handling matrix

| Failure | Detection | Response | Cleanup path | Where |
|---|---|---|---|---|
| Worker process dies | SWIM direct → indirect → SUSPECT → DEAD (1.5–8 s bracket); AD-53 burst mode when ≥2 targets fail in 30 s; AD-30 job-layer escalation after 60 s silence + 2 s timer, gated by "no successful probe in 2 protocol periods" | Manager marks its DISPATCHED/RUNNING workflows FAILED, cancels dependents, re-queues in topological order, re-dispatches to a different worker with a new fence token; AD-44 budget caps retries (10 per job, 3 per workflow) | Registry entry reaped after `MANAGER_DEAD_WORKER_REAP_INTERVAL` 900 s, checked every 60 s; AD-48 `dead` broadcast to peer managers; LHM/extension trackers cleared on `remove_worker_state` | `health_aware_server.py:3779-3881`; `nodes/manager/server.py:3000-3129`; `AD_33` state machine; `env.py:308-319` |
| Worker alive, workflow stuck | AD-34 stuck check: no progress for 120 s (leader, every 30 s); AD-26 extension exhaustion (5 grants, 30/15/7.5/3.75/1 s) + 10 s grace; AD-41 resource kill at 100 % of budget after 2 s | `_timeout_job`, or eviction of the worker's deadline, or `ResourceKillRequest` → SIGTERM | Same as above; timeout strategy removed at terminal state | `nodes/manager/server.py:3379-3436`; `AD_26.md`; `AD_41.md:130-160` |
| Manager follower dies | SWIM among managers; `_active_manager_peers` tracks quorum availability (AD-6) | Leader continues; rejoining follower syncs via state sync (3 retries, 0.5 s × 2ⁿ backoff, AD-11) | Dead peer reaped after 900 s | `AD_6.md`, `AD_11.md`, `env.py:311-319` |
| Manager leader dies | Lease (5 s) expires at followers; pre-vote (2 s) → election (5–7 s) | New leader rebuilds from workers (AD-4) and peers (AD-12), resumes timeout tracking with `timeout_fence_token + 1` (AD-34), takes over job leadership and notifies gates/workers (AD-31) | Workers keep executing; if no `JobLeaderWorkerTransfer` arrives within `WORKER_ORPHAN_GRACE_PERIOD` 5 s (checked every 1 s) they cancel orphaned workflows; clients wait 15 s (`CLIENT_ORPHAN_GRACE_PERIOD`) | `local_leader_election.py:324-392`; `env.py:236-305`; `tests/integration/raft/test_cancellation_failover.py:294-427` |
| Gate dies | Gate-tier SWIM with 30/120 s bracket; per-job lease (30 s, renewed every 10 s) expires | Ring backup claims the lease with `fence + 1`; takeover broadcast to gates and managers; clients recompute the owner from the ring and reconnect (3 tries × 15 s) | Gate dead-peer reap 120 s (check 10 s); orphaned jobs failed after `GATE_ORPHAN_GRACE_PERIOD` 10 s (check 2 s) | helper report 8406–14200 §1; `env.py:395-421`; `nodes/gate/orphan_job_coordinator.py` |
| Intra-DC network partition | Probe timeouts on both sides; suspicion runs the max bracket when no witness is reachable ("witness-less" rule) | Quorum from configured size (AD-3) stops the minority from electing; pre-vote stops term storms; a minority leader's lease cannot renew; fence tokens reject its stale operations; minority submits are refused with a leader hint | On heal: higher term wins, equal term lower address yields; refutations via incarnation bump; `partition_healed` callbacks fire (`CrossDCCorrelationDetector`) | `raft_node.py:646-648`; `leader_state.py:400-421`; `health_aware_server.py:4068-4084`; `docs/SCENARIOS.md:106-122` |
| Cross-DC partition / correlated DC failures | `FederatedHealthMonitor` (2 s probe, 5 s timeout, 5 failures → SUSPECTED, 30 s → UNREACHABLE) + `CrossDCCorrelationDetector` (window 30 s; LOW/MEDIUM/HIGH at 2/3/4 DCs and 50 %; failure confirmation 5 s, recovery confirmation 30 s; flap = 3 changes in 120 s → 300 s cooldown; latency ≥100/500 ms and LHM ≥3 as secondary signals) | Eviction is **held** when correlation ≥ MEDIUM ("likely network, not DC failure"), 60 s backoff; routing falls back HEALTHY > BUSY > DEGRADED, UNHEALTHY fails the job (AD-17) | Per-DC state cleared on recovery confirmation | `env.py:122-133,625-694`; `AD_16.md`, `AD_17.md`; `swim/health/federated_health_monitor.py` |
| Mass crash / cascading failure | AD-53 burst confirmation; `NodeHealthTracker.should_evict` refuses to evict when > 50 % of nodes look dead ("systemic failure, holding eviction") | Bounded-parallel confirmation instead of the serial probe walk; LEAVE fast path for graceful scale-down | Normal reap | `AD_53.md:187-262`; `AD_19.md:358-383` |
| Zombie job (work running with no owner) | Gate lease expiry; AD-34 overall timeout `started_at + timeout + extensions`; manager orphan scan every 120 s comparing manager-tracked vs worker-reported workflows (5 s query timeout); worker cancellation poll every 5 s; job responsiveness loop | Re-queue orphans (`requeue_workflow`), cancel via four-phase idempotent cancellation (AD-20), fence-token rejection of stale dispatches | Completed jobs purged after 300 s, failed after 3600 s (every 60 s); cancelled-workflow records after 3600 s; pending leader transfers after 60 s | `nodes/manager/server.py:2906-2950`; `env.py:176-179,277-294`; `AD_20.md` |
| Zombie node (old process, stale identity) | Rejoin below `death_incarnation + bump` inside the zombie window is rejected; per-process `MESSAGE_INCARNATION` + replay guard (300 s age, 60 s future, 100k ids); AD-52 fence header rejects `WRONG_CLUSTER`/`STALE_MEMBER` | Drop the message, count it | Dead entries retained 3600 s then evicted | `test_failure_scenarios.py:57-127`; `replay_guard.py:41-44`; `AD_52.md:303-326` |
| Duplicate submission (lost response) | Client idempotency key `{client_id}:{seq}:{nonce}`; gate LRU cache (100k, TTL 60/300/60 s, pending wait 30 s) + manager WAL-backed ledger | Return the cached result; coalesce concurrent duplicates on one future | Sweep every 10 s | `AD_40.md`; `env.py:102-108` |
| Retry storm / thundering herd | — | Full/equal/decorrelated jitter everywhere (AD-21); registration jitter 0–5 s sized so N=50 workers stay under ~10 connects/s; recovery jitter 0.05–0.5 s with ≤5 concurrent recoveries; dispatch cooldowns 0.25–5 s; retry budgets (AD-44) | — | `env.py:203-216,489-517`; `AD_21.md` |
| Overload | see §4 | priority load shedding, backpressure signals, rate limits | — | §4 |
| Adversarial input | size caps, compression-ratio check, restricted unpickler, cluster/env id and mTLS role checks, DNS CIDR allow-list and IP-change alerts (5 per 300 s) | reject and count | — | `security.py`; `AD_28.md:103-155`; `env.py:711-729` |

Two audit documents matter for the port. `docs/architecture/AUDIT_DISTRIBUTED_2026_01_11.md`
found: unbounded `defaultdict` collections for cancellation/results state (§1.1), per-peer lock
dicts that are never removed (§1.2), unbounded latency sample lists (§1.3), `list.pop(0)` ring
buffers (§1.4), a lock-creation race fixed by `setdefault` (§2.1), eviction outside the lock
(§2.2), the `add_done_callback` registration race (§2.3), no documented lock ordering (§3.1),
557 bare `except: pass` sites across 116 files (§4.1), silently swallowed callback errors (§4.2),
`Event.wait()` without timeouts (§5.2), and 47 raw `asyncio.create_task` calls (§5.4). The gate
compliance report (`docs/architecture/compliance/gate_compliance_2026_01_13.md:12-19`) later
marks the gate module compliant with all 35 applicable ADs. Every one of those audit categories
is a class of bug slates' rules already forbid; the list is a useful checklist for the Rust review.

## 4. Backpressure, load shedding, admission control, and degradation

Hyperscale stacks six independent mechanisms. Each has fixed thresholds; only the overload
detector self-calibrates.

### 4.1 Protocol admission: priority-aware bounded immediate execution (AD-32)

`ProtocolInFlightTracker` (`server/protocol/in_flight_tracker.py:196-327`) runs inside the sync
`datagram_received`/`data_received` callbacks. Every handler name is classified into
CONTROL / DISPATCH / DATA / TELEMETRY (`:47-148`; SWIM probes, cancellation, leadership, join/leave
are CONTROL). Ungrouped CRITICAL is never shed; the `swim` admission group has its own cap of
1,000 so SWIM is isolated from data-plane shedding but still bounded (`:205-211,280-327`);
HIGH 500 / NORMAL 300 / LOW 200 under a global 1,000 (`env.py:767-778`). Rejected UDP is silently
dropped, rejected TCP gets an error with Retry-After (`AD_32.md:118-120`). The design rationale
is explicit: a consumer-loop queue "adds latency even at 0 % load — deadly for SWIM", so the
server executes immediately and bounds by counters instead (`AD_32.md:24-28`). Recommended
per-node limits: gate 2000/1000/600/400, manager 5000/2500/1500/1000, worker 500/250/150/100
(`AD_32.md:421-425`).

### 4.2 Client side: per-destination `RobustMessageQueue` (AD-32 Part 2)

One queue per destination so a slow DC cannot head-of-line block a fast one
(`AD_32.md:275-381`). `RobustQueueConfig` (`reliability/robust_queue.py:75-94`): primary 1,000,
overflow ring 100 (newest preserved), thresholds 0.70/0.85/0.95, suggested delays
throttle 50 ms / batch 200 ms / overflow 100 ms / reject 500 ms; states HEALTHY → THROTTLED →
BATCHING → OVERFLOW → SATURATED (`:44-50`). Server-level defaults are 500 / 100 with at most
1,000 tracked destinations, LRU-evicted (`env.py:781-785`). The same queue backs the WAL writer
(§2.3).

### 4.3 Data-plane backpressure signals (AD-23, AD-37)

`StatsBuffer` (`reliability/backpressure.py:80-101,204-220`): HOT ring 1,000 entries / 60 s,
WARM 360 ten-second aggregates / 1 h, COLD 1,440 one-minute aggregates / 24 h; backpressure level
from HOT fill: NONE < 0.70 ≤ THROTTLE < 0.85 ≤ BATCH < 0.95 ≤ REJECT (`record` drops at REJECT,
`:158-186`). `BackpressureSignal.from_level` suggests 100 / 500 / 1000 ms (`:422-451`) and is
embedded in `WorkflowProgressAck`. The worker takes the **max** level across all managers, adds
`WORKER_BACKPRESSURE_{THROTTLE,BATCH,REJECT}_DELAY_MS = 500/1000/2000` to its 50 ms flush
interval, and at REJECT clears its buffer (`env.py:193-196`, `nodes/worker/background_loops.py:320-358`,
`AD_37.md:29-71`). Message classes: CONTROL never backpressured, DISPATCH shed under overload,
DATA explicitly backpressured and batched, TELEMETRY shed first (`AD_37.md:20-27`).

### 4.4 Load shedding driven by hybrid overload detection (AD-22, AD-18)

`HybridOverloadDetector` (`reliability/overload.py:51-74`, `env.py:432-454`): fast EMA baseline
α = 0.1 plus a slow EMA with a 15 % drift threshold, current window 10 samples, trend window 20,
minimum 3 samples, delta thresholds 20 / 50 / 100 % above baseline, absolute rails 200 / 500 /
2000 ms, CPU and memory thresholds 0.70 / 0.85 / 0.95, rising-trend threshold 0.1; state =
worst of the three signals. `LoadShedder` sheds by state: HEALTHY nothing, BUSY LOW, STRESSED
NORMAL+LOW, OVERLOADED everything but CRITICAL (`reliability/load_shedding.py:48-55,241-275`).
The delta component is the only self-calibrating mechanism in the codebase ("Fixed thresholds
cause flapping and require per-workload tuning", `AD_18.md:12-15`).

### 4.5 Rate limiting (AD-24)

Server side: `SlidingWindowCounter` (`reliability/rate_limiting.py:33`) with per-operation limits
per 10 s window — stats_update 500, heartbeat 200, progress_update 300, job_submit 50,
job_status 100, workflow_dispatch 100, cancel 20, reconnect 10 (`:272-281`) — and a token-bucket
variant with per-op refill rates (`:874-883`); health-gated so that OVERLOADED passes only
CRITICAL. Env defaults: bucket 100, refill 10/s, idle client cleanup after 300 s every 60 s;
client retries 3 with 1.5× backoff and a 60 s total wait (`env.py:478-486`); client progress
callbacks 100/s with burst 20 (`env.py:602-603`); Retry-After default 1 s.

### 4.6 Timeouts that adapt

- LHM-scaled probe/suspicion timeouts and degradation multipliers (§1.5): LIGHT policy is
  probe rate 0.9, gossip 0.8, 4 piggyback updates, timeouts ×1.2
  (`swim/health/graceful_degradation.py:47-56`), escalating through MODERATE/HEAVY/CRITICAL to
  step-down and refused leadership.
- Circuit breakers: `CIRCUIT_BREAKER_MAX_ERRORS 3 / WINDOW 30 s / HALF_OPEN 10 s` (`env.py:135-138`)
  in the worker/manager clients, but `health/circuit_breaker_manager.py:22-24` defaults to
  5 / 60 s / 30 s — two configurations for one concept.
- AD-26 adaptive healthcheck extensions: base deadline 30 s, min grant 1 s, max 5 extensions,
  grant `max(min, base / 2^count)`, eviction after 3 failed cycles, 10 s exhaustion grace
  (`env.py:528-537`). Phase H turned it into a statistical decision: multi-counter progress
  snapshots, a BOCPD throughput witness with a hierarchical false-positive budget of 1 %, a
  worker-side trigger at 75 % of the deadline every 5 s, and a Beta-posterior tuner keyed by
  workflow class (`AD_26.md:238-440`, `env.py:538-569`). The deadline itself is derived:
  explicit override → workflow timeout → `duration × 1.5` (`AD_26.md:254-264`).
- AD-34 timeouts: overall `started_at + timeout + extensions`, stuck 120 s, manager loop 30 s,
  gate loop 15 s, gate-unresponsive fallback 300 s, all-DCs-silent 180 s (helper report
  15016–20164 §3).

### 4.7 Budgets and capacity

- Retry budgets (AD-44): job budget default 10 / max 50, per-workflow 3 / 5, fixed for the job's
  lifetime (`env.py:345-349`); best-effort completion after `min_dcs` or a 300 s (max 3600 s)
  deadline checked every 5 s (`:351-355`).
- Resource guards (AD-41): 1 s sampling, warn/throttle/kill at 0.70/0.85/1.0 of budget, 5 s
  grace, Kalman Q = 10 / R = 25 (`AD_41.md:150-160`); manager gossip of resource views every 2–5 s.
- Capacity-aware spillover (AD-43): wait ≤ 60 s at the primary before spilling, ≤ 100 ms extra
  latency, spillover must cut wait by ≥ 50 %, capacity data stale after 30 s (`env.py:423-427`).
- Raft group cap 10,000 and WAL queue caps (§2) are the control-plane budgets.

### 4.8 What is genuinely measured today (the seeds for slates' "derive everything" rule)

EMA/slow-EMA latency baselines (AD-18); per-peer probe reliability windows; peer-load class from
gossip; Vivaldi RTT with error/confidence; event-loop lag ratio; BOCPD change-points and Beta
posteriors for extensions (AD-26 H6/H8); Kalman-filtered CPU/memory (AD-41); T-Digest latency
windows (AD-42); EWMA-with-variance observed dispatch latency (AD-45); the continuous direct-probe
budget; the `2 × protocol period` escalation gate. Everything else in §8's table is a literal.

## 5. Async discipline, cleanup, cancellation — and the Rust mapping

### 5.1 What the TaskRunner actually does (`hyperscale/distributed/taskex/`)

- `TaskRunner.run(call, *args, alias, timeout, schedule, repeat, keep, max_age, keep_policy)`
  (`task_runner.py:148-212`) registers one `Task` per name and returns a `Run`; a `Run` executes
  the coroutine via `asyncio.ensure_future` (`run.py:342-343`) with an optional `wait_for`
  timeout (`:514-517`), or a sync callable through a thread/process executor guarded by a
  semaphore of `MERCURY_SYNC_TASK_RUNNER_MAX_THREADS = cpu_count` (`:522-548`, `env.py:24`).
- History and cleanup: each `Task` keeps at most `keep` runs (default 10, `task.py:83-87`) or
  those younger than `max_age`; a cleanup loop every `MERCURY_SYNC_CLEANUP_INTERVAL = 0.25 s`
  cancels and drops evicted runs (`task_runner.py:449-461`, `task.py:174-213`). Callers can
  request `keep_policy="COUNT_AND_AGE"` for one-shot work (`health_aware_server.py:3930-3937`).
- Schedules: `repeat="ALWAYS"` creates a **new Run object every period** inside one
  `ensure_future`'d loop (`task.py:369-408`); `stop_schedules` flips a flag.
- Waiting: `wait(token)` polls the run status every cleanup interval instead of awaiting a
  future (`task_runner.py:310-350`).
- Shutdown: cancels every run and schedule, then cancels **and awaits** the cleanup task (a fix
  for a leaked task, `:404-427`); `abort()` cancels without awaiting (`:429-447`).
- Task ids come from a per-runner Snowflake generator so ordering is monotone and replay is
  deterministic (`task_runner.py:57-65`, `task.py:98-104`).
- Weak points to avoid: exceptions inside a run are stored on `run.error` and never re-raised
  unless someone `wait`s (`run.py:552-559`); the cleanup loop swallows everything
  (`task_runner.py:454-461`); `cancel()` swallows (`run.py:292-316`); tasks are keyed by function
  name so two unrelated callers sharing a name share a `Task`; `ensure_future` binds to the running
  loop at creation (the exact coupling the Phase 6b lint forbids elsewhere).

### 5.2 The discipline around it

- Injected time and randomness: every sleep, `wait_for`, monotonic/wall read and every
  non-crypto random draw goes through a `Clock` / `Random` Protocol (`runtime/clock.py:35-96`,
  `runtime/random_source.py:30-63`) so the simulation harness can drive virtual time and seeded
  randomness. Crypto randomness stays on `secrets`.
- Lints in `tests/simulation/lints/`: no raw `asyncio.create_task`/`ensure_future` in production
  (`test_no_raw_asyncio_task.py:1-60`, with an expected-violations ratchet), no direct
  `time`/`random`, no direct disk I/O, no phantom attributes.
- Genuine-cancel discrimination in long-lived loops (`health_aware_server.py:3754-3774`) and
  terminal-abort barriers re-checked after every await so a stopping node never emits a message
  (`:5985-6011,6295-6303`).
- Sub-quantum deadline remainders count as expiry (`protocol/time_quantum.py`) and every wait has
  a 1 ms floor (`local_leader_election.py:291-322`) — both learned from livelocks under simulation.
- Every long-lived container is bounded and has an eviction path with a callback or counter:
  gossip 1,000; suspicions 10k/1k/50k; incarnations 10k; replay window 100k; idempotency 100k;
  Raft log 50k and 10k groups; stats 1000/360/1440; in-flight 1,000; queues 1,000+100; peers
  1,000/10k; extension ledger `2 × max_extensions` per workflow (`AD_26.md:335-343`). TTL sweeps
  run in the reap/cleanup loops (§3).
- Cancellation propagation (AD-20): four phases Client → Gate → Manager → Worker with a fenced,
  idempotent `JobCancelRequest`; each layer acks before propagating; the worker also polls for
  cancellation every 5 s as a backstop; per-workflow `asyncio.Event`s cut execution; pending
  cancellations and completion events survive leader change
  (`tests/integration/raft/test_cancellation_failover.py:594-676`).
- Snapshot-then-iterate everywhere a dict may mutate during an await (`task_runner.py:91-94`,
  `indirect_probe_manager.py:126-133`).

### 5.3 Mapping onto Rust structured concurrency for slates

| Hyperscale mechanism | Rust equivalent for slates |
|---|---|
| `TaskRunner.run` returning tokens; runner owns all background tasks | One owner per subsystem holding a `JoinSet` (or `tokio_util::task::TaskTracker`) plus a child `CancellationToken`; `shutdown()` cancels and **joins**; forbid bare `tokio::spawn` outside that module with clippy `disallowed-methods` (the same ratchet as the Python lint) |
| Run history with `keep` / `max_age` and a 0.25 s cleanup poll | Not needed: a finished task's result flows through its `JoinHandle`/`oneshot`; nothing to sweep |
| `repeat="ALWAYS"` → new Run per period | One long-lived task with `tokio::time::interval` (`MissedTickBehavior::Delay`) in `select!` against the cancel token; the period is a value read from the measured-parameter source each tick |
| `wait(token)` polling | `JoinHandle.await` or `Notify`; never sleep-poll |
| Errors stored on `Run` and swallowed | `Result` propagates to the owner; the owner decides restart-or-fail; a supervisor task logs and counts, never `except: pass` |
| `Clock` / `Random` Protocol seams | A `Clock` trait with a `Copy` handle (real: monotonic + `tokio::time`; sim: virtual) passed by value into each component; `SmallRng` seeded per task from one root seed; `tokio::time::pause()`/`advance()` in tests |
| Shared ack future + `asyncio.shield` | `watch`/`Notify` so many waiters observe one ack; each waiter's own timeout only cancels its wait; cancel-safe by construction |
| Terminal-abort barrier (`if not self._running` after awaits) | `select!` on the cancel token around every await; transport `send` refuses after shutdown |
| `asyncio.Lock` around state + awaits inside | An owning task per piece of state (actor) fed by a **bounded** `mpsc`; readers get owned snapshots or generation numbers over a `watch` channel — no locks, no `Arc` |
| `MappingProxyType` copy per append | Persistent/COW map or generation-stamped snapshots sent on change; never O(n) copy on the hot path |
| `RobustMessageQueue` primary + overflow ring, graduated states | `mpsc` with capacity from Little's law (measured arrival rate × latency budget); `try_send` on shed paths (UDP), `send().await` on backpressure paths (TCP); overflow `VecDeque` with fixed capacity; queue state derived from measured drain rate |
| `run_in_executor` for blocking I/O | Irrelevant to slates (memory only); CPU-heavy work (compression, hashing) → `spawn_blocking` behind a semaphore sized from measured throughput |
| `ProtocolInFlightTracker` counters in sync callbacks | Per-priority `Semaphore` permits (`try_acquire` on the receive path) with permit counts derived from measured handler service time |
| GIL-"atomic" counters and copy-on-write tuples | Single-owner state; where lock-free is proven necessary, `AtomicU64` counters and `crossbeam` epoch structures checked with `loom` |
| Cancellation events per workflow | `CancellationToken` tree: job → workflow → slate provisioning; drop = cancel |

## 6. Bootstrap and discovery

### 6.1 Today (AD-28, `hyperscale/distributed/discovery/`)

- Five layers (`AD_28.md:27-100`): DNS/static seeds with positive and negative caches → security
  validation (cluster id, environment id, mTLS role claims) → locality tiers (same DC < 2 ms,
  same region < 50 ms, global; fall to the next tier below `min_peers_per_tier`) → weighted
  rendezvous hash top-K then power-of-two choices by EWMA latency → a sticky pool of primary +
  backup connections with health-based eviction.
- `DiscoveryConfig` defaults (`discovery/models/discovery_config.py:34-213`): default port 9000,
  `dns_timeout 2.0 s`, `dns_cache_ttl 30 s`, negative cache base TTL 30 s with exponential
  backoff to 300 s capped at 10 failures (`discovery/dns/negative_cache.py:44-51`), DNS CIDR
  allow-list and IP-change anomaly detection (5 changes / 300 s, reject on violation),
  `min_peers_per_tier 3`, `candidate_set_size 8`, `primary 3`, `backup 2`, `ewma_alpha 0.2`,
  baseline 10 ms, eviction at error rate > 5 %, 3 consecutive failures, or latency > 3× baseline,
  `probe_timeout 0.5 s`, `max_concurrent_probes 10`, backoff 0.5 s × 2 up to 15 s with 25 %
  jitter, refresh every 60 s, promotion jitter 0.1–0.5 s, connection max age 3600 s.
  `env.py:696-760` overrides several with different numbers (DNS timeout 5 s, TTL 60 s,
  candidate set 3, EWMA α 0.3, baseline 50 ms, latency multiplier 2×, min peers 1, port 9091) —
  two sources of truth.
- Parallel probing: the resolver runs up to `max_concurrent_probes` resolutions concurrently
  (`discovery/discovery_service.py:192`); `discover_peers` is cached unless forced (`:275-304`).
- Node wiring: managers seed peer discovery from `config.seed_managers` and allow dynamic
  registration only when no seeds exist ("a solo manager is a valid topology",
  `nodes/manager/discovery.py:69-92`); workers run a maintenance loop that decays failure counts
  and re-resolves DNS every 60 s (`nodes/worker/discovery.py:52-72`); gates keep a
  `datacenter_managers` map plus SWIM gossip (`docs/AD_52_PLAN.md:50`).
- Worker registration: a random 0–5 s initial jitter sized from the observation that 50 workers
  at 0.25 s produced ~200 connects/s and blew the accept backlog (`env.py:205-216`); then
  `RetryExecutor` with FULL jitter, 5 retries, 0.25 s base, and a per-manager circuit breaker
  (`nodes/worker/registration.py:102-171`, `env.py:203-204`); a liveness watchdog every 2 s
  downgrades a manager silent for 20 s, and a rejoin loop backs off `2 s × LHM` with jitter
  (`env.py:157-174`, `nodes/worker/cluster_connection.py:387-465`).
- Cluster formation: `CLUSTER_STABILIZATION_TIMEOUT 10 s` polled every 0.5 s, first-election
  jitter ≤ 3 s, `MANAGER_STARTUP_SYNC_DELAY 2 s` (`env.py:110-120,253-255`). AD-29's
  confirmed/unconfirmed model exists precisely because simultaneous starts produced false DEADs
  within 2.5 s (`AD_29.md:14-26`); a peer still unconfirmed after 60 s is logged
  (`AD_29.md:216-222`) and removed after the role passive timeout (§1.7).

### 6.2 The target design (AD-52)

Seed locators `tcp://`, `dns://`, `dns-srv://`, `file://`, `exec://` resolved inside the process,
re-resolved every 60 s with 0–20 % jitter and on any connection failure, bounded to 64 candidates
by weighted rendezvous sampling (`AD_52.md:77-95`); a universal environment contract of "launch
with config, TCP reachability, a seed string" and an explicit list of forbidden assumptions
(stable IPs, hostnames, disk, NTP, DNS honesty, port numbers, node-id continuity;
`AD_52.md:51-73`). Bootstrap is separate from join: founding nodes agree on
`sha256(sorted(initial_members))`, pre-vote at term 1 after a quorum of `BootstrapHello`s, commit
`EnterJoint(∅ → founders)` then `LeaveJoint`, mint `cluster_uuid`, and exit non-zero on any
mismatch (`AD_52.md:152-228`). Joiners handshake, follow `leader_hint`, become learners, and are
promoted when within 256 entries (`AD_52.md:232-262,330-381`). DC clusters register with the gate
cluster with generation markers and a 1 h grace for old generations; a manager cluster that boots
before its gates waits in `WAITING_FOR_GATES` with jittered backoff (`AD_52.md:769-812`).
Implementation status: Phase 0 (HLC into Raft apply) and the Phase 1 skeleton items are
specified in `docs/AD_52_PLAN.md:62-800`; the cluster package does not exist yet.

### 6.3 Failure scenarios covered

DNS unavailable → static seeds / file locator; all seeds down → exponential backoff to 15 s and
re-resolution on failure; wrong cluster or environment → rejected before any field is processed
(`AD_28.md:103-134`); peers that never answer → never suspected, warned at 60 s, removed by role
timeout; bootstrap split (different member lists) → hash mismatch abort; registration storm →
jitter formula `N / J` connects per second; owner-manager death → worker re-registers elsewhere
(AD-7); pod restart with new IP → new node id, old one evicted via tombstone (AD-52 §3, §8).

## 7. How distributed behaviour is tested

- **Layout**: `tests/integration` (24 files, 14,684 lines: `gates/`, `manager/`, `worker/`,
  `raft/`, `swim/`, `slo/`, `extensions/`); `tests/simulation` (28,870 lines: `harness/`,
  `scenarios/phase3_faults`, `scenarios/phase4_network`, `vopr*`, `soak/`, `lints/`, `oracle/`);
  `tests/end_to_end` (32,113 lines of numbered `gate_manager/` and `manager_worker/` sections);
  `tests/unit`; `tests/framework` (a JSON-driven scenario runner with actions such as
  `start_cluster`, `stop_nodes`, `restart_nodes`, `submit_job`, `await_manager_leader`,
  `assert_condition`).
- **Integration style**: real components wired in-process. SWIM tests are scenario coroutines
  that build an `IncarnationTracker` / `HierarchicalFailureDetector` /
  `CrossDCCorrelationDetector`, drive them with explicit incarnations and sleeps, and print
  PASSED/FAILED (`tests/integration/swim/test_failure_scenarios.py:57-170`: zombie rejection,
  zombie window expiry, incarnation persistence across restart, partition-healed callbacks). Raft
  tests are pytest classes asserting membership removal, quorum shrink after leave, "proposal
  requires leadership", idempotent group creation, backpressure at `max_instances`, cleanup on
  destroy, independent groups per job (`tests/integration/raft/test_raft_leadership_failover.py:109-534`).
  Cancellation/failover tests cover orphan detection from dead-manager sets, grace periods,
  fence-token acceptance/rejection on transfer, per-workflow locks against races, pending
  cancellations surviving leader change, gate fan-out to DCs, and state cleanup
  (`tests/integration/raft/test_cancellation_failover.py:166-885`).
- **Simulation harness**: `ClusterHarness` launches real node processes under a supervisor with
  a port allocator; `FaultMatrix` composes partition, delay, drop, bandwidth, reorder, duplicate
  and TCP-reset rules per link and protocol, plus kill/pause/resume of processes
  (`tests/simulation/harness/fault_matrix.py:64-156,989-1132`). An `InvariantChecker` polls every
  100 ms: safety invariants must hold continuously; liveness invariants expose a monotonic
  progress counter and fail when it regresses and stalls past a staleness budget; a diagnostic
  snapshot is dumped **before** the violation is raised (`tests/simulation/harness/invariants.py:1-130`).
  Harness budgets: stabilization 30 s, stop 30 s, condition 15 s, workload 120 s, reap 15 s/node
  (`tests/simulation/harness/timeouts.py:10-31`). SIM mode swaps in a `SimulationLoop`,
  virtual clock, seeded random, simulated filesystem and in-process/fake transports
  (`tests/simulation/harness/sim/`), with VOPR-style fault plans, a chaos runner, soak swarms and a
  cluster-trace oracle (`tests/simulation/vopr*`, `soak/`, `oracle/`).
- **Scenario taxonomy** (`docs/SCENARIOS.md`): 12 categories — leadership, node failure/rejoin,
  network conditions, partitions, clock anomalies, membership churn, workload patterns, resource
  pressure, adversarial messages, persistence/recovery, continuous invariants, observability —
  with nine standing invariants (at most one leader per DC, at most one job leader per job,
  monotonic fence tokens, unique sub-workflow tokens, acknowledged jobs reach a terminal state,
  cancelled jobs free cores within budget, `available + reserved ≤ total`, member-count
  convergence, cluster-id isolation; `:211-229`) and a ranked "first five" list (`:253-263`).
  The root `SCENARIOS.md` (1,723 lines) is a saved chat transcript of scenario brainstorming
  (gate peer reaping, quorum step-down, gate↔manager cases), not a specification.
- **Process rule**: agents write integration tests but never run them; a human does
  (`CLAUDE.md:44-46`).
- **For slates**: keep the three ideas that carry the value — a continuously polled invariant
  set with safety/liveness split, a composable fault matrix over an injected transport, and
  seeded determinism through injected clock/random. In Rust: `tokio::time::pause` plus a
  `turmoil`-style simulated network, `loom` for any lock-free structure, `proptest` state-machine
  tests for the incarnation/lease/fence state machines, and clippy `disallowed-methods` as the
  lint ratchet.

## 8. Verdicts and the consolidated constants table

### 8.1 PORT — bring into slates' server/db as-is in spirit

| Pattern | Why it is worth keeping |
|---|---|
| Lifeguard-correct SWIM core: direct → k indirect → SUSPECT → DEAD, suspicion `max − (max−min)·log(C+1)/log(K+1)` with the originator excluded, refutation by incarnation bump, `dead/suspect > alive` priority in gossip, λ·ln(n+1) rebroadcasts, MTU-bounded piggyback (§1.3–1.6) | Proven at scale (Serf/memberlist), every rule here was added after a measured failure; the code is close to the paper |
| Peer confirmation before suspicion (AD-29) + registration gate + UNCONFIRMED lifecycle (AD-35) | Eliminates the whole class of boot-time false positives without a grace timer |
| Gossip-informed callbacks on the NOT-DEAD → DEAD edge (AD-31) | Cluster-wide consistent reaction without extra messages |
| Timer discipline from AD-30: confirmations mutate state, never reschedule timers; one timing wheel; adaptive polling | Immune to the confirmation-storm starvation bug |
| Single source of truth for node state with incarnation conflict rules (AD-46) | O(1) per node, no queues, no duplicate caches |
| LHM ±1 scoring with a bounded multiplier, event-loop lag feeding LHM, and prob-OR composition of uncertainty signals | Bounded by construction; the multiplicative alternative "blew up > 90×" |
| Continuous direct-probe budget derived from n, LHM, peer load and reliability (`health_aware_server.py:4149-4235`) | The one already-measured parameter; the template for slates' rule |
| Pre-vote, term-as-fence, monotone heartbeat sequence with leader-granted lease length, deterministic tiebreak | Cheap split-brain prevention; each piece closes a specific observed hole |
| Quorum from configured membership, never from the live count (AD-3); fence tokens on every mutation; reject lower, accept equal | Standard, and the reason partitions are safe |
| WAL entry framing `CRC | len | LSN | clock | state | type`, group commit with per-write futures, bounded queue with graduated states, recovery that stops at the torn tail | Correct framing and batching model even when the "disk" is a replication quorum |
| Deterministic apply layer: timestamps minted by the leader's clock into the entry, no wall clock/random/I/O in apply (AD-52 §15, plan item 0.1 replay test) | Byte-equal replicas; a must for a replicated DB |
| Priority admission with a never-shed control class and a bounded control reserve; per-destination queues; CONTROL/DISPATCH/DATA/TELEMETRY taxonomy (AD-22/32/37) | Keeps failure detection alive under overload; isolates slow peers |
| Hybrid overload detection (EMA delta + absolute rails + resources) and correlation-aware eviction holding (AD-18, AD-33 correlation, AD-19 systemic check) | Self-calibrating and cascade-resistant |
| Three-signal health (liveness / readiness / progress) | Separates "busy" from "dead" — the same distinction slates needs for agents' slates |
| Idempotency key `{client}:{seq}:{nonce}` with PENDING coalescing (AD-40) | At-most-once provisioning for retrying agents |
| Injected clock/random, lint ratchets, invariant checker, fault matrix, genuine-cancel discrimination, terminal-abort barriers, deadline epsilon (§5.2, §7) | This is how the author found most of the bugs listed above |
| AD-52 concepts: deterministic bootstrap list, joint consensus, learners, `ClusterRPCFence` header, tombstone before REMOVE, ephemeral node ids, watch streams, disconnected mode | State-of-the-art membership; matches slates' single-node-to-global requirement |

### 8.2 ADAPT — bring with changes

| Pattern | Change slates should make |
|---|---|
| Every numeric default in `env.py`, dataclass defaults and literals (§8.4) | Replace with values derived from measured RTT, service time, arrival rate, membership size and memory budget; the only operator inputs are SLO targets and the membership list |
| SWIM-tier election (`LocalLeaderElection`) | Use configured size for pre-vote/vote quorum (it uses live member count, `local_leader_election.py:501-503`); derive election timeout and lease from measured heartbeat p99; fold into the Raft layer rather than running two elections |
| Per-job Raft groups with all nodes in every group (10k groups, serial replication, volatile) | One replicated log per placement group / shard for slates, pipelined `AppendEntries` with a measured in-flight window, learners + joint consensus (AD-52), in-memory snapshot compaction — persistence via replication quorum, not disk |
| WAL + tiered durability (LOCAL/REGIONAL/GLOBAL) | slates never touches disk: LOCAL becomes "in this process's log", REGIONAL/GLOBAL become replication quorums; keep framing and CRC on the wire; keep LSN, but make the clock a real HLC (`max(physical, remote)` + bounded drift), which the code does not implement (§2.3) |
| Group-commit batching parameters (500 µs / 1,000 / 1 MiB) | Adaptive batching: close a batch when the transport is ready to send, bounded by measured replication RTT and the latency SLO — no fixed timer |
| Backpressure thresholds 0.70/0.85/0.95 and suggested delays 50–2000 ms | Derive from measured drain rate: signal when projected time-to-full is below the measured round-trip; delays = measured service time × queue depth |
| Priority limits 1000/500/300/200 and per-op rate limits | Little's law from measured handler service time and the latency budget; rate limits from measured capacity |
| Gate hash ring (MD5, 150 virtual nodes, lease in a local dict, "accept any higher fence token") | Stable non-crypto hash (xxh3/SipHash), virtual-node count derived from node count and balance target, lease records replicated in the log, higher tokens accepted only when they match replicated lease state |
| Text-delimited SWIM messages and base64-cloudpickle heartbeats | Length-prefixed binary schema with a version byte; no code-carrying serialization (cloudpickle is remote code execution guarded by an allow-list) |
| Vivaldi (Serf defaults) with a 10 ms reference RTT | Keep the engine; reference RTT = measured intra-cluster median; UCB constants from measured error distribution |
| Adaptive healthcheck extensions (AD-26 Phase H) | Port the statistical version (progress witnesses, BOCPD, posterior tuner); drop the fixed 30 s base — for slates the "workflow" is an agent's operation with a measured duration distribution |
| Cleanup/reap intervals (900 s, 3600 s, 300 s, 60 s) | Derive from measured rejoin-time and client-fetch distributions; make the sweep event-driven with a measured fallback |
| Dead-node retention and zombie windows (3600 s, 60 s, bump 5/10) | Retention from measured rejoin distribution; the zombie rule becomes unnecessary with ephemeral node ids + epoch fence (AD-52) |
| Retry policies (3 attempts, 0.5 s base, 30 s cap; PROBE 3/0.1/2.0) | Keep the jitter families (AD-21); base from measured RTT, cap from the operation's deadline, attempts from the budget |
| Discovery pool (K=8, 3 primary, 2 backup, EWMA α 0.2, evict at 5 % / 3 failures / 3× baseline) | Keep rendezvous + power-of-two; sizes from node count; eviction thresholds from measured error-rate baseline and variance |
| Circuit breakers (two different configs) | One implementation; thresholds from measured error rate; half-open interval from measured recovery time |

### 8.3 AVOID — do not bring

| Pattern | Reason |
|---|---|
| Thread-pool `run_in_executor`, `asyncio.Lock` around awaited sections, "GIL-atomic" counters, copy-on-write tuple swaps as a lock-free story | Python event-loop artefacts; slates uses owning tasks and bounded channels |
| `ensure_future`/`create_task` fire-and-forget (47 sites in the audit), sleep-polling waits (`TaskRunner.wait`), exceptions stored and swallowed (557 `except: pass` sites) | Violates slates' no-orphan / no-swallow rules |
| Disk-backed incarnation store, WAL files, worker event logs, mmap/msync, fsync tuning (AD-39 Parts 11–16) | slates is memory-only; the whole I/O-thread design is moot |
| Duplicated and contradictory configuration (env vs dataclass defaults for discovery, two circuit-breaker configs, two LHM multiplier tables, AD-26 grant formula off-by-one, α 0.1 vs 0.2 in AD-45) | A single measured-parameter source with one owner per value |
| `time.monotonic()` values compared across hosts (AD-34 gate vs manager reports) and LWW on wall clocks | Only HLC or leader-minted timestamps cross a host boundary |
| Unbounded structures noted in the audits (defaultdict state, lock dicts, latency lists, VSR `prepare_log`, AD-34 `_pending_reports`, ledger `_state_index`) | Every container in slates has a bound and an eviction path |
| Per-job Raft group with every node participating; 10,000 groups ticked every 50 ms by one loop | O(groups) work per tick; use sharded logs |
| The VSR sketch as written (no view-change quorum, swallowed prepare timeouts, seq reset race) | Use Raft/joint consensus (AD-52) instead |
| "Accept any higher fence token" without verification | Favors liveness over safety; verify against replicated lease state |
| Fixed 15-minute reaps, 24 h retention, 5-minute job caches, 2 s control-plane poll loops | Replace with event-driven paths plus measured fallbacks |
| cloudpickle + allow-list unpickler as the wire format | Code execution surface; use a schema |

### 8.4 Consolidated table of hardcoded constants and what slates should measure or derive instead

Origin key: **E** env default (`hyperscale/distributed/env/env.py`), **L** literal/dataclass
default in code, **D** doc-only constant (`docs/architecture.md`, cited by helper reports),
**R** derived at runtime already.

| Area | Constant | Value | Where | Origin | slates: measure / derive |
|---|---|---|---|---|---|
| SWIM | `SWIM_UDP_POLL_INTERVAL` (protocol period) | 1 s | `env.py:60` | E | `period = max(k·p99(RTT), scheduler quantum)`; k from the target detection latency (operator SLO) |
| SWIM | `SWIM_CURRENT_TIMEOUT` (probe base) | 1 s | `env.py:59` | E | per-peer p99 RTT from the reliability/Vivaldi tracker × margin from measured variance |
| SWIM | `SWIM_MIN/MAX_PROBE_TIMEOUT` | 1 / 5 s | `env.py:57-58` | E | bounds from measured RTT distribution (p50, p99.9) |
| SWIM | direct-probe budget | `[base, 3·base]` | `health_aware_server.py:4149-4235` | R | keep; cap from LHM saturation |
| SWIM | indirect proxies k | 3 | `indirect_probe_manager.py:34` | L | `k = ⌈log(target_fp) / log(p_loss)⌉` from measured per-link loss, bounded by n−2 |
| SWIM | indirect `max_pending` / TTL | 100 / 30 s | `indirect_probe_manager.py:37-40` | L | pending = n × probes-in-flight; TTL = probe budget |
| SWIM | `SWIM_SUSPICION_MIN/MAX_TIMEOUT` | 1.5 / 8 s | `env.py:61-66` | E | min = k × period; max from measured gossip convergence time (λ·ln n periods) |
| SWIM | `GATE_SWIM_GLOBAL_MIN/MAX_TIMEOUT` | 30 / 120 s | `env.py:418-419` | E | same formula scaled by measured cost of a false positive (jobs per gate) |
| SWIM | `GATE_SWIM_JOB_MIN/MAX_TIMEOUT` | 5 / 30 s | `env.py:420-421` | E | as above |
| SWIM | `HierarchicalConfig.global_min/max_timeout` | 5 / 30 s | `hierarchical_failure_detector.py:69-70` | L | as above (duplicate source) |
| SWIM | `global_required_confirmations` K | 2 | `hierarchical_failure_detector.py:72` | L | from n: number of independent witnesses reachable in one gossip round |
| SWIM | `SWIM_NO_WITNESS_SUSPICION_TIMEOUT` | 30 s | `env.py:67` | E | max bracket × measured partition-heal p95 |
| SWIM | `job_min/max_timeout` | 1 / 10 s | `hierarchical_failure_detector.py:75-76` | L | from measured progress-report interval |
| SWIM | timing wheel ticks | 1000 / 100 ms | `hierarchical_failure_detector.py:80-81` | L | resolution = min timeout / 10 |
| SWIM | job poll intervals / thresholds | 1000/250/50 ms; 5 s/1 s | `job_suspicion_manager.py:42-48` | L | fractions of remaining time (already adaptive); base from tick resolution |
| SWIM | `max_lhm_backoff_multiplier` | 3.0 | `job_suspicion_manager.py:51` | L | = LHM max multiplier (derived) |
| SWIM | reconciliation interval | 5 s | `hierarchical_failure_detector.py:88` | L | event-driven on death; fallback = max bracket |
| SWIM | suspicion caps | 10k / 1k / 50k | `hierarchical_failure_detector.py:91-93` | L | memory budget ÷ measured entry size |
| SWIM | `MAX_CONFIRMERS` | 1000 | `suspicion_state.py:16` | L | = n |
| SWIM | `regossip_factor` | 3 | `suspicion_state.py:63` | L | = λ |
| SWIM | LHM `max_score` / `MULTIPLIER_WEIGHT` | 8 / 0.25 | `local_health_multiplier.py:29,36` | L | keep score; max multiplier from measured tail-latency ratio under load |
| SWIM | event-loop sample / expected sleep / lag thresholds / counts | 1 s / 10 ms / 0.5, 2.0 / 3, 5 | `health_monitor.py:66-98` | L | baseline lag measured at start; thresholds as multiples of baseline σ |
| SWIM | peer-load multipliers busy/stressed/overloaded | 1.25 / 1.75 / 2.5 | `peer_health_awareness.py:115-117` | L | from measured RTT inflation per reported state |
| SWIM | peer-health stale / max peers | 30 s / 1000 | `peer_health_awareness.py:120-123` | L | stale = k × heartbeat interval; max = n |
| SWIM | reliability window / TTL / max peers | 8 / 60 s / 10k | `peer_probe_reliability_tracker.py:56-58` | L | window = probes per suspicion bracket; TTL = bracket; max = n |
| SWIM | gossip λ | 3 | `gossip_buffer.py:44` | L | from measured convergence: smallest λ reaching all members within target periods |
| SWIM | gossip `max_updates` / stale age / evict batch | 1000 / 60 s / 10 | `gossip_buffer.py:47,50,327` | L | updates = n × churn per convergence window; stale = convergence time |
| SWIM | `MAX_PIGGYBACK_SIZE` / `MAX_UDP_PAYLOAD` | 1200 / 1400 B | `gossip_buffer.py:25-26` | L | path-MTU discovery per peer |
| SWIM | piggyback updates per message / cap | 5 / 100 | `gossip_buffer.py:156-187` | L | from MTU budget ÷ measured update size |
| SWIM | worker-state gossip cap / stale / share | 500 / 60 s / 600 B | `AD_48.md:148-151` | D | as above |
| SWIM | refutation rate limit | 5 per 10 s | `env.py:83-84` | E | from measured suspicion rate + margin |
| SWIM | `BURST_FAILURE_THRESHOLD/WINDOW` | 2 / 30 s | `env.py:79-80` | E | threshold = baseline failure EWMA + 3σ; window = LHM-stretched probe round |
| SWIM | burst probe concurrency | threshold × fan-out, capped | `AD_53.md:229-231` | R | keep |
| SWIM | `max_nodes` / dead retention | 10k / 3600 s | `incarnation_tracker.py:79-80` | L | n; retention from measured rejoin p99 |
| SWIM | zombie window / rejoin bump / restart bump | 60 s / 5 / 10 | `test_failure_scenarios.py:69-73,161-166` | L | unnecessary with ephemeral ids + epoch fence |
| SWIM | role passive timeouts / attempts / spacing / load caps | 120/90/180 s; 5/3/0; 5 s; 3/5/10× | `confirmation_strategy.py:33-64` | L | from measured startup-time distribution per role |
| SWIM | unconfirmed warning | 60 s | `AD_29.md:220` | D | = startup p99 |
| SWIM | Vivaldi dims / ce / error decay / gravity / height / smoothing / error clamp | 8 / 0.25 / 0.25 / 0.01 / 0.25 / 0.05 / [0.05, 10] | `coordinate_engine.py:18-25` | L | keep algorithm constants; validate against measured prediction error |
| SWIM | RTT reference / latency-multiplier clamp / quality weight | 10 ms / [1,10] / 0.5 | `health_aware_server.py:4820-4830` | L | reference = measured intra-cluster median RTT |
| SWIM | cleanup interval / orphaned suspicion timeout | 30 s / 300 s | `resource_limits.py:225,243` | L | = bracket max |
| SWIM | `PROBE_RETRY_POLICY` | 3 / 0.1 s / 2 s / 0.15 | `swim/core/retry.py:149-153` | L | base = RTT p50; cap = probe budget |
| Cross-DC | federated probe interval / timeout / suspicion / failures | 2 / 5 / 30 s / 5 | `env.py:124-133` | E | from measured cross-DC RTT distribution |
| Cross-DC | correlation window / thresholds / fraction / backoff | 30 s / 2,3,4 / 0.5 / 60 s | `env.py:629-646` | E | window = federated probe × failures; thresholds from DC count |
| Cross-DC | failure/recovery confirmation, flap threshold/window/cooldown | 5 s / 30 s / 3 / 120 s / 300 s | `env.py:649-661` | E | from measured state-change rate baseline |
| Cross-DC | latency elevated/critical, samples, window | 100 / 500 ms, 3, 60 s | `env.py:664-675` | E | from measured cross-DC RTT baseline (× multiples) |
| Election | heartbeat / election base / jitter / pre-vote / lease | 2 / 5 / 2 / 2 / 5 s | `local_leader_election.py:53-56`, `env.py:87-91` | L,E | heartbeat = k × period; election = heartbeat p99 × margin; lease = k × heartbeat |
| Election | `LEADER_ELECTION_JITTER_MAX` | 3 s | `env.py:118` | E | = election timeout spread |
| Election | `LEADER_MAX_LHM` | 4 | `env.py:92-94` | E | relative: LHM above cluster median + σ |
| Election | `MAX_TERM` / `MAX_VOTES` | 2^53−1 / 1000 | `leader_state.py:16-20` | L | u64 / n |
| Raft | election timeout / heartbeat | 150–300 ms / 50 ms | `raft_node.py:36-38` | L | ≥ 10 × broadcast RTT p99 (Raft paper); heartbeat = election/3 |
| Raft | proposal timeout | 5 s | `raft_node.py:128` | L | commit-latency p99 × margin |
| Raft | `RaftLog.max_entries` | 50,000 | `raft_log.py:31` | L | memory budget ÷ measured entry size |
| Raft | snapshot threshold | 10,000 | `snapshot.py:121` | L | from measured apply rate and snapshot cost |
| Raft | `max_instances` | 10,000 | `raft_consensus.py:69` | L | not applicable (sharded logs) |
| Raft | membership history per job | 1,000 | `replicated_membership_log.py:75` | L | churn × retention window |
| Raft | RaftWAL batch µs / entries / bytes / queue | 500 / 500 / 4 MiB / 5000 | `raft_wal.py:150-154` | L | adaptive batching (§8.2) |
| Raft (AD-52) | learner promote lag / max lifetime | 256 / 30 min | `AD_52.md:376-381` | D | lag = entries applied per replication RTT; lifetime from measured catch-up rate |
| Raft (AD-52) | tombstone retention | 10 min | `AD_52.md:398-401` | D | from measured partition-heal p99 |
| Raft (AD-52) | watch ring / disconnected threshold / staleness bounds | 16,384 / 30 s / 5 s, 10 s | `AD_52.md:483,529,522-525` | D | ring = churn × reconnect p99; thresholds from watch RTT |
| Raft (AD-52) | in-flight AppendEntries / snapshot interval | 256 / 10,000 | `AD_52.md:737,747` | D | bandwidth-delay product; apply rate |
| Raft (AD-52) | phi thresholds | 8 / 4 | `AD_52.md:411` | D | from measured inter-arrival distribution (that is what phi-accrual does) |
| WAL | header size | 34 B | `wal_entry.py:15` | L | keep |
| WAL | batch timeout / entries / bytes | 500 µs / 1,000 / 1 MiB | `wal_writer.py:79-81` | L | adaptive batching |
| WAL | queue / overflow / thresholds | 10,000 / 1,000 / 0.70,0.85,0.95 | `wal_writer.py:82-87` | L | Little's law; thresholds from drain rate |
| WAL | stop grace | 5 s | `wal_writer.py:234` | L | = in-flight batch latency p99 |
| WAL | regional / global wait | 5 s / 30 s | `docs/architecture.md:21936,21978` | D | replication RTT p99 × margin |
| WAL | checkpoint entries / seconds / keep | 100,000 / 300 s / 3 | `docs/architecture.md:22090-22092` | D | from measured memory growth rate |
| WAL | AD-39 batch / buffer / segment / pool | 5–10 ms / 100 / 10,000 / 64 KiB / 16 | helper report 20164–31380 §7 | D | not applicable (no disk) |
| Logger | queue max / batch max | `DEFAULT_QUEUE_MAX_SIZE` / 100 | `logger_stream.py:108-109,149-150` | L | Little's law |
| Admission | global / swim / high / normal / low | 1000 / 1000 / 500 / 300 / 200 | `env.py:767-775`, `in_flight_tracker.py:205-211` | E,L | permits = target concurrency = measured service time × target throughput |
| Admission | warn threshold | 0.8 | `env.py:776-778` | E | drop |
| Queues | outgoing queue / overflow / max destinations | 500 / 100 / 1000 | `env.py:781-785` | E | Little's law; destinations = n |
| Queues | RobustQueue delays | 50 / 200 / 100 / 500 ms | `robust_queue.py:91-94` | L | measured service time × depth |
| Queues | `MESSAGE_QUEUE_MAX_SIZE/WARN` | 1000 / 800 | `env.py:520-523` | E | Little's law |
| Backpressure | stats hot / warm / cold sizes and ages | 1000 / 360 / 1440; 60 s / 1 h / 24 h | `backpressure.py:85-96` | L | ingest rate × retention SLO |
| Backpressure | thresholds | 0.70 / 0.85 / 0.95 | `backpressure.py:99-101`, `env.py:612-616` | L,E | time-to-full vs measured drain latency |
| Backpressure | signal delays | 100 / 500 / 1000 ms | `backpressure.py:426-428` | L | measured drain time |
| Backpressure | worker delays | 500 / 1000 / 2000 ms | `env.py:194-196` | E | as above |
| Backpressure | progress ratios normal/slow/degraded | 0.8 / 0.5 / 0.2 | `env.py:620-622` | E | from measured throughput distribution per workload class |
| Overload | EMA α / windows / min samples / trend / drift | 0.1 / 10, 20 / 3 / 0.1 / 0.15 | `overload.py:51-74` | L | α from sample rate and desired time constant |
| Overload | delta thresholds | 0.2 / 0.5 / 1.0 | `env.py:440-442` | E | from measured baseline variance (σ multiples) |
| Overload | absolute bounds | 200 / 500 / 2000 ms | `env.py:444-446` | E | SLO input only |
| Overload | CPU / memory thresholds | 0.7 / 0.85 / 0.95 | `env.py:448-454` | E | from measured headroom vs. latency inflation |
| Rate limit | per-op limits per 10 s | 500/200/300/50/100/100/20/10 | `rate_limiting.py:272-281` | L | measured capacity per op |
| Rate limit | bucket / refill / idle / cleanup / retries / max wait / multiplier | 100 / 10 / 300 s / 60 s / 3 / 60 s / 1.5 | `env.py:478-486` | E | capacity; retry from budget |
| Rate limit | client progress / burst | 100 /s / 20 | `env.py:602-603` | E | consumer capacity |
| Circuit breaker | errors / window / half-open | 3 / 30 s / 10 s and 5 / 60 s / 30 s | `env.py:135-138`, `circuit_breaker_manager.py:22-24` | E,L | error-rate baseline; half-open = measured recovery time |
| Retries | `RetryConfig` attempts / base / cap | 3 / 0.5 s / 30 s | `reliability/retry.py:63-65` | L | base = RTT; cap = deadline; attempts = budget |
| Retries | state sync retries / base | 3 / 0.5 s | `AD_11.md:17-19` | D | as above |
| Retries | recovery jitter min/max, max concurrent, semaphore | 0.05–0.5 s / 5 / 5 | `env.py:493-502` | E | jitter = spread over measured accept rate; concurrency from measured capacity |
| Retries | dispatch per-worker / workers / cooldowns | 3 / 16 / 0.25–5 s, 0.5 s | `env.py:503-517` | E | from measured dispatch latency |
| Retries | retry budgets default/max, per-workflow | 10 / 50; 3 / 5 | `env.py:346-349` | E | from measured failure rate and SLO |
| Leases | job lease / cleanup | 30 s / 10 s | `job_lease.py:29,80-81`, `env.py:97-100` | L,E | lease = k × heartbeat p99; renew at lease/3 (already derived) |
| Leases | VSR lease TTL / renewal | 10 s / 3 s | `docs/architecture.md:24646-24650` | D | as above |
| Zombies | orphan scan / worker query timeout | 120 s / 5 s | `env.py:574-579` | E | scan = f(dispatch rate); timeout = RTT p99 |
| Zombies | cancellation poll | 5 s | `env.py:177-179` | E | event-driven; fallback from push-failure rate |
| Zombies | job timeout check / stuck threshold | 30 s / 120 s | `env.py:343`, `AD_34.md:118` | E,D | check = f(progress interval); stuck from measured progress-gap distribution |
| Zombies | progress report / gate loop / fallback / all-silent | 10 s / 15 s / 300 s / 180 s | helper report 15016–20164 §3 | D | from measured report-interval and cross-DC RTT |
| Zombies | responsiveness threshold / check | 60 s / 15 s | `env.py:326-331` | E | from measured progress-gap p99 |
| Cleanup | dead reaps (manager worker/peer/gate) / check | 900 s / 60 s | `env.py:308-319` | E | rejoin p99; event-driven check |
| Cleanup | worker dead-manager reap / check | 900 s / 60 s (env) vs 60 s / 10 s (loop defaults) | `env.py:150-155`, `background_loops.py:75-76` | E,L | as above; one source |
| Cleanup | gate dead peer reap / check | 120 s / 10 s | `env.py:405-406` | E | as above |
| Cleanup | completed / failed job age, interval | 300 s / 3600 s / 60 s | `env.py:278-284` | E | client fetch-latency p99 |
| Cleanup | cancelled workflow TTL / interval / timeout | 3600 s / 60 s / 60 s | `env.py:287-294` | E | as above |
| Cleanup | worker orphan grace / check; pending transfer TTL | 5 s / 1 s; 60 s | `env.py:239-250` | E | grace = election + takeover p99 (measured) |
| Cleanup | client orphan grace / check / freshness | 15 s / 2 s / 10 s | `env.py:297-305` | E | as above |
| Cleanup | gate orphan grace / check | 10 s / 2 s | `env.py:398-403` | E | as above |
| Cleanup | quorum step-down consecutive failures | 3 | `env.py:407` | E | from heartbeat loss baseline |
| Cleanup | versioned-clock entity max age | 300 s | `lamport_clock.py:301` | L | entity lifetime |
| Discovery | DNS timeout / cache TTL / negative TTL base, max, failures | 2 s (env 5) / 30 s (env 60) / 30 s, 300 s, 10 | `discovery_config.py:73-79`, `negative_cache.py:44-51`, `env.py:707-708` | L,E | honour DNS TTL; negative backoff from measured resolver failure rate |
| Discovery | candidate K / primary / backup / min per tier | 8 (env 3) / 3 / 2 / 3 (env 1) | `discovery_config.py:142-155`, `env.py:739-751` | L,E | from n and target fan-out |
| Discovery | EWMA α / baseline latency | 0.2 (env 0.3) / 10 ms (env 50) | `discovery_config.py:158,175`, `env.py:742-747` | L,E | α from sample rate; baseline measured |
| Discovery | eviction error rate / consecutive failures / latency multiplier | 0.05 / 3 / 3× (env 2×) | `discovery_config.py:166-172`, `env.py:748-750` | L,E | from measured error-rate baseline and variance |
| Discovery | probe timeout / max concurrent / backoff / jitter / refresh / promotion jitter / max age | 0.5 s / 10 / 0.5→15 s ×2 / 0.25 / 60 s / 0.1–0.5 s / 3600 s | `discovery_config.py:179-206` | L | RTT p99; fd budget; measured change rate |
| Discovery | IP change max / window | 5 / 300 s | `env.py:721-726` | E | from measured churn |
| Discovery | `DISCOVERY_PROBE_INTERVAL` / failure decay | 30 s / 60 s | `env.py:757-760` | E | from peer count and RTT |
| Bootstrap | registration retries / base / initial jitter | 5 / 0.25 s / 5 s | `env.py:203-216` | E | jitter = N ÷ measured accept rate (formula already in the comment) |
| Bootstrap | pool startup timeout | 60 s | `env.py:234` | E | measured spawn time × N |
| Bootstrap | liveness check / staleness / rejoin backoff | 2 s / 20 s / 2 s | `env.py:166-174` | E | k × heartbeat; backoff = RTT-based |
| Bootstrap | stabilization timeout / poll / startup sync delay | 10 s / 0.5 s / 2 s | `env.py:112-117,253` | E | event-driven on quorum confirmation |
| Bootstrap (AD-52) | seed refresh / jitter / max candidates / bootstrap window | 60 s / 20 % / 64 / 5 s | `AD_52.md:92-95,1037-1039` | D | change rate; RTT |
| Bootstrap (AD-52) | DC registration grace | 1 h | `AD_52.md:801` | D | in-flight reference lifetime |
| Transport | `MAX_MESSAGE_SIZE` / decompressed / ratio | 3 MiB / 5 MiB / 100 | `core/jobs/protocols/constants.py` | L | from measured payload distribution (p99.9 × margin) |
| Transport | replay max age / future / window / incarnations | 300 s / 60 s / 100k / 10k | `replay_guard.py:41-44` | L | age = RTT p99.9 + clock drift bound; window = rate × age |
| Transport | TCP backlog / UDP rcvbuf / max concurrency | 4096 / 4 MiB / 4096 | `env.py:18,28-29` | E | from measured connection rate and burst size |
| Transport | request timeout / connect / retries | 30 s / 5 s / 3 | `env.py:11,15-16,22` | E | RTT-derived |
| Transport | TaskRunner cleanup interval / threads / keep | 0.25 s / cpu_count / 100 | `env.py:17,24-25` | E | not applicable (JoinSet) |
| Hash ring | virtual nodes / ring space | 150 / 2^32 | `consistent_hash_ring.py:50` | L | from node count and load-balance target |
| Hash ring | client reconnect retries / wait | 3 / lease/2 | `docs/architecture.md:11942-11962` | D | from measured takeover time |
| Idempotency | pending / committed / rejected TTL, max entries, cleanup, pending wait | 60 / 300 / 60 s, 100k, 10 s, 30 s | `env.py:102-108` | E | TTL = client retry window p99 + margin (measured); entries = rate × TTL |
| Extensions | base deadline / min grant / max / eviction / grace | 30 s / 1 s / 5 / 3 / 10 s | `env.py:528-537` | E | from measured operation-duration distribution (they already use `duration × 1.5`) |
| Extensions | timeout multiplier / FPR budget / trigger / lookahead | 1.5 / 0.01 / 5 s / 0.75 | `env.py:544-569` | E | multiplier from measured duration variance; FPR is an SLO input |
| Resources | sample / thresholds / grace / Kalman Q, R | 1 s / 0.7,0.85,1.0 / 5 s / 10, 25 | `AD_41.md:150-160` | D | Q,R from measured innovation variance (adaptive filter already does this) |
| Resources | default budgets CPU / memory / FDs | 800 % / 16 GiB / 10,000 | helper report 31380–38790 §2 | D | from host capacity |
| Routing/SLO | T-Digest δ / buffer, windows, targets, weights, min samples, factor clamp, health ratios/windows | 100 / 2048; 60 s × 5; 50/200/500 ms; 0.2/0.5/0.3; 100; [0.5, 3]; 1.5/2,3/5; 60/180/300 s | helper report 31380–38790 §4 | D | targets/weights are SLO inputs; windows from measured request rate; ratios from measured baseline |
| Routing | spillover wait / penalty / improvement / staleness / aggregation | 60 s / 100 ms / 0.5 / 30 s / 5 s | `env.py:423-427` | E | from measured queueing and RTT |
| Routing | adaptive α / min samples / staleness / cap | 0.2 (tracker 0.1) / 10 / 300 s / 60 s | `env.py:358-362` | E | α from sample rate; staleness from measured change rate |
| Stats | window / drift / push / max age | 50 / 25 / 50 / 5000 ms | `env.py:584-595` | E | window from measured collection jitter; drift from RTT |
| Stats | worker update / flush intervals | 0.05 / 0.05 s | `env.py:141-146` | E | from consumer capacity |
| Stats | manager / gate batch intervals, heartbeat | 0.25 s / 0.25 s / 5 s | `env.py:263-265,373-385` | E | from measured fan-in |
| Timeouts | worker/manager/gate TCP short / standard / forward | 2 / 5 / 3 s | `env.py:199-202,365-370,387-391` | E | RTT p99 × margin per peer |
| Timeouts | health probes liveness/readiness/startup | 1 s/10 s/3/1; 2 s/10 s/3/1; 5 s/5 s/30/1 | `env.py:460-473` | E | RTT-derived; startup from measured boot time |
| Timeouts | `TIME_REMAINDER_EPSILON_SECONDS` | 1 µs | `time_quantum.py:28` | L | keep (float artefact bound) |
| Timeouts | `MERCURY_SYNC_DUPLICATE_JOB_POLICY`, misc pool sizes | replace / 1 / 100 | `env.py:47-51` | E | from capacity |

### 8.5 Contradictions worth resolving before porting

- Two LHM multiplier tables (`1 + 0.25·score` vs the ×1.25/1.5/2/3 degradation table,
  helper report A §2.3) and two circuit-breaker configurations (§4.6).
- Env defaults disagree with dataclass defaults for discovery (§6.1) and worker reap loops
  (`env.py:150-155` vs `background_loops.py:75-76`).
- The SWIM-tier election uses the live member count for quorum; Raft uses the configured size.
- AD-26's grant formula was off by one (`AD_26.md:245-252`); the H1 fix is in code.
- AD-34 compares monotonic timestamps across hosts and never reconciles gate vs manager fence
  counters (helper report 15016–20164 §3).
- The "HLC" is a Lamport clock with a wall annotation, not the HLC the AD-38 text specifies (§2.3).
- The AD-33 state table forbids PENDING → CANCELLED but two cancel paths do it (helper report §2).
- Doc says gossip-hashed ring uses Python `hash()`; code uses MD5 — both unsuitable, pick a stable
  fast hash.

Sources of the consolidated numbers: this file's §1–§7, the four helper range reports in
`survey-hyperscale-architecture-ranges.md`, and the partial helper notes for lines 1–8405 and
23181–38790 (`arch-part-A.md`, `arch-part-C.md` in the session scratchpad), each of which cites
`docs/architecture.md` line numbers for its own table.
