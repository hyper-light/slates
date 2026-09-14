# Content replication — the rest of §4.10 (measurement and design record)

Date: 2026-09-13. Branch `agent/content-replication`. Machine: the shared 18-core macOS box (Darwin
25.4.0, rustc 1.98.0), load averages 3.8–7.3 during the runs quoted. Every number carries the command
that produced it. Piecewise, per CLAUDE.md §6: each piece is a failing test first, the minimal change,
and its own commit.

## Piece 1 — the hedge trigger from the measured p95 put latency (landed)

**The design's rule** (§4.8 "Derived constants"): *hedge delay = measured p95 put latency per class*;
(§4.8 mechanism 1): *content is sent to `f + 1` candidates first, hedged to the remaining candidates
after the measured p95 put latency*; (§4.10 failure matrix): *a candidate holder slow: Masked (the hedge
completes the put elsewhere)*. Evidence: Dean & Barroso, "The Tail at Scale" (CACM 2013): a hedged
request is sent "after the request has been outstanding for longer than the 95th-percentile expected
latency for this class of requests".

**What the tree did before.** `put_seal_content` ran one round per coordinator period and *awaited* it
to the round's full progress-extended budget (the consensus budget: one period, extended to the election
timeout while acknowledgements trickle in). The hedge round therefore went out on the *next period after
the first round returned* — a trigger of "one heartbeat after the first round completes or times out",
never the p95; and with a first-round holder that is starved of CPU, the round did not return until its
extended budget expired (about a second), so the seal placed when the starved holder finally answered.
No put latency was measured anywhere: the module doc listed "the hedge trigger from a measured p95" as
owed.

**The failing test first.** `a_slow_first_round_candidate_is_hedged_after_the_measured_p95`
(`crates/server/tests/fleet.rs`): a three-node `f = 1` fleet; the owner seals once (a prompt first round
leaves readings), then the first-round candidate's control shard is held busy for three liveness
budgets (`HEDGE_STARVATION_NS = 3 s`, the same hold the SWIM starvation test uses) and the owner seals
again. The seal must place through the *other* candidate while the first is still held.

```
cargo test -p slates-server --test fleet a_slow_first_round_candidate_is_hedged_after_the_measured_p95 -- --exact --nocapture
```

| Tree | Result |
|---|---|
| unchanged (no readings taken anywhere) | FAILED 6.46 s — `the first seal left measured put-latency readings on the owner: Some((0, None))` |
| readings + trigger, round still awaited to its full budget | FAILED 7.65 s — placed after **3.172 s** against the 3 s hold: the p95 (10.05 ms) was right but the hedge round could not go out while the first round blocked the coordinator |
| readings + trigger + the round's collection stopping at the hedge delay (this piece) | ok 6.07 s / 5.72 s — placed after **349 ms / 354 ms** against the 3 s hold; readings 1 → 2 (p95 10.05 ms) |

The intermediate row is kept on purpose: it is the measurement that showed a hedge is a *second request
while the first is in flight*, which a round awaited to completion can never make — the trigger alone was
not the fix.

**The change.**
- `slates_cluster::collect_bound` times every binding acknowledgement from the round's dispatch and
  returns the readings (`Collected { reusable, timed_out, latencies_ns }`); `put_content` starts that
  clock at the *put* round (the offer round before it is the holder reporting what it lacks, not the
  transfer) and returns `ContentPlaced::latencies_ns`. The record commit gathers its class's readings too
  and does not use them yet (it re-ships idempotently every period).
- `ShardState::put_latency: PutLatency` (`crates/server/src/fleet.rs`): the newest
  `PUT_LATENCY_WINDOW = 200` readings of the content class in arrival order (the acknowledgements of a
  hundred seals at the `f = 1` candidate floor's two remote holders — the smallest window at which the
  nearest-rank p95 resolves to one reading; the oldest reading leaves as the newest arrives, so it is
  bounded); `p95_ns()` through the machine crate's `Sample`/`Percentile::P95` (a new percentile constant
  beside P50/P99/P999, nearest rank).
- `hedge_delay_ns(&PutLatency)` = the p95, or one heartbeat before any reading (the first seal of a boot,
  and the laptop, where no round runs — the same code with an empty window, R8).
- `content_budget(&PutLatency)`: the round's **collection stops at the hedge delay** (base deadline = the
  delay; one extension of `span − delay` granted only to a round still gathering acknowledgements at the
  delay — the ratified late-work rule — so a round with none expires there and is hedged; stall window =
  the delay; poll = the collection cadence), while every holder task keeps the full consensus span so a
  slow holder's acknowledgement still arrives as a straggler.
- `LateReplies::Content { shard, object, snapshot, sequence, manifest, dispatched_ns }`: a straggler's
  bound acknowledgement is **folded into the seal** on its owner shard (`fold_late_content` →
  `fold_content_ack`) and timed into the window — never discarded. Hedging bounds how long the round
  *waits*, not whether a slow holder's verified hold counts.
- `SealJob::first_round_at_ns`; `content_work` holds the hedge round back until the first round has been
  outstanding for the hedge delay; `put_seal_content` records the round's readings and the first round's
  dispatch time; `ContentWork::budget` carries the round's budget computed from the owner shard's window.
- Observation: `Daemon::fleet_put_latency(object) -> Option<(usize, Option<u64>)>` (readings, p95), the
  test's non-vacuity counter.

**Measured on this box:** content-class put p95 on loopback, one small file, `f = 1`: **10.05 ms**
(`Some((1, Some(10049833)))`, `Some((1, Some(10052375)))` across two runs). The hedge placed the seal
through the third candidate 349–354 ms after the second snapshot was taken, against a 3 s hold on the
first-round candidate. Of those ~350 ms, the p95 itself is 10 ms; the rest is the round machinery's
cadence — one coordinator period for the first round to expire at the delay, one to dispatch the hedge,
and the head ship after the content places — which is a *cadence* cost of the per-period driver, not the
trigger, and is left as recorded here (a finer driver is a separate change to the record plane's period).

**Other tests run on this piece:** `a_sealed_snapshots_content_replicates_to_the_holder_and_places`
3.97 s / 3.78 s; `a_takeover_successor_serves_the_dead_owners_content_over_nfs` 10.66 s;
`a_volume_on_a_non_control_shard_replicates_its_content_and_places` 3.94 s (each `-- --exact`, one at a
time); `cargo test -p slates-cluster` 146 passed; `cargo test -p slates-server --lib` 20 passed;
`cargo test -p slates-machine --lib stats` 5 passed; `cargo fmt --check`, `cargo clippy --workspace
--all-targets -- -D warnings`, `cargo xtask check` clean. The whole fleet suite and load runs are the
integrator's.

**Per class.** The design says "per class"; the two classes the record plane dispatches are records
and content. Only the content class hedges (records go to *all* candidates at once and re-ship
idempotently), so only its window drives a trigger; the record class's readings are gathered by the same
collector and available when a record-plane hedge is designed.

## Piece 2 — anti-entropy and the healer at the measured put-failure rate (landed)

**The design's rule** (§4.10): *anti-entropy walks Merkle manifests between recorded holders and repairs
only differing subtrees; the healer replays puts that never reached f+1 from the owner's `put_wal`*;
(§4.8 "Derived constants"): *healer cadence from the measured put-failure rate*; (§4.8 "Recovery"): a
restarted node *holds nothing for others until re-replication fills it*.

**What the tree did before.** The replay half already existed in a different name: a seal whose content
never reaches `f + 1` stays in `ShardState::seals` and is re-put every period until it places (that is the
`put_wal` replay for content). What did not exist was any verification *after* placement: a placed seal is
dropped with its archive, and a recorded holder that later loses the content (a restart) is never
noticed — the head record names a holder that holds nothing, and a takeover successor fetching by identity
would find it gone.

**The failing test first.** `a_holder_that_lost_placed_content_is_repaired_by_the_healer`: three nodes,
`f = 1`; seal to placed; the recorded holder **forgets** the manifest (`Daemon::drop_held_content` →
`ContentHold::forget_manifest`, the in-process stand-in for a restart, injected for the same reason the
rejoin test injects a death); the holder must hold it whole again *with no new seal* and the owner's
repair count must move.

```
cargo test -p slates-server --test fleet a_holder_that_lost_placed_content_is_repaired_by_the_healer -- --exact
```

| Tree | Result |
|---|---|
| unchanged (the loss is never noticed) | FAILED 448.39 s — `repairs Some(0) → Some(0)`; the poll ran its 4000-period daemon-time budget out |
| the healer (this piece) | ok **13.52 s** — the holder holds the manifest again, repairs 0 → 1 |

**The change** — replace, don't layer: no second exchange and no second put path.
- `heal_one_placed_snapshot` (`crates/server/src/fleet.rs`), one step per healer period at the end of
  `advance_seals`: the next owned volume (in id order, wrapping — `HealerCursor`) whose head snapshot is
  recorded `Placed` and has no seal in progress is re-opened as a **`healing` `SealJob`** seeded with the
  owner as its only acknowledged holder. The ordinary content rounds then re-**offer** the archive to every
  candidate: the `Offer → Missing` exchange *is* the Merkle diff (the manifest is a Merkle tree, `Node::
  identity` names children by identity, so a differing subtree is a differing chunk set), a holder that
  lost nothing answers with an empty missing set and is put **zero bytes** (it still verifies its whole
  hold and acknowledges), a holder that lost content is put exactly what it lacks, and the placement is
  re-recorded (`record_placed_seals`; the db guard accepts a re-placement). `sealable_head` keeps a healing
  job past its "already placed" drop until its round has run.
- `ContentPlaced::refilled` (`puts_for` reports the holders whose missing set was non-empty): for a
  healing job, a refilled holder that then acknowledged is a **repair**, counted in `ShardState::repairs`
  (`Daemon::fleet_repairs`, the test's non-vacuity counter — a fresh seal could also refill a holder; only
  the healer moves this).
- `PutOutcomes { placed, short }` — every content round's outcome, recorded on the owner shard; the
  measured put-failure rate. `heal_period_ns` = `HEARTBEAT_NS × HEAL_PERIODS_AT_REST × placed / (placed +
  short × HEAL_PERIODS_AT_REST)`: one snapshot per hundred periods while every round places (an idle fleet
  spends one offer round trip per placed snapshot per ten seconds verifying what it placed), tightening in
  proportion to the share of rounds that ended short until, when short rounds are as common as placed
  ones, it is one snapshot per period — the fastest the coordinator runs; never below one period;
  integer arithmetic. `HEAL_PERIODS_AT_REST` is anchored to the put-latency window's seal count so the
  healer covers a window of seals in a window of periods.
- Bounded work: one snapshot per step, one step per period at most; the walk is a cursor over the owned
  volumes, so no per-period work grows with what is placed.

**Measured on this box.** Loss injected → repaired and re-held: within the 13.52 s test (formation,
seal, loss, one healer step at the at-rest cadence — the first step fires at once, before any reading —
re-archive in slices, offer, put of the missing chunks, acknowledgement, re-record). The regressions on the
same machinery: hedge test 6.00 s, seal test 3.85 s, takeover-content test 10.04 s (each `-- --exact`).

**Probation** (the design's "puts a repeatedly late candidate on probation for the group to replace";
threshold "late count over the measured window that exceeds the hedge rate's variance") is **not** in
this piece: it is a configuration-group action (the council replaces the candidate), so it belongs with
the council's membership reconcile, and its threshold needs the hedge rate's variance over the window the
readings now exist for. Reported as the next owed step of §4.10, with the readings it needs in place.

## Piece 3 — the compress-or-not cost model (D-17), and the chunking gate's measurements (landed)

**The design's rule** (§4.11 "Cost model"): *inputs from the profile (codec throughput per level, hash
throughput, memcpy bandwidth, free memory) and from the volume (… expected read count per chunk: high
for attached, ~1 per re-attach for archived); per chunk: zero-detect → Btrfs-style sampled statistics →
LZ4 probe with early exit → predicted savings per level → choose the encoding maximizing `bytes_saved ×
value_of_byte(pressure) − (t_compress + E[reads] × t_decompress) × value_of_cpu(load)`, subject to the
format floor. Hot volumes stay raw unless pressure raises `value_of_byte`; archived volumes compress
once.* D-17 lost the fixed "save 12.5 %" rules. Chunking (research §2.4, ratified): *page-multiple fixed
chunks for large files; FastCDC only for the class of files whose measured size exceeds a threshold and
whose observed dedup gain (bytes saved per byte hashed, tracked per volume) exceeds the measured hashing
cost; the FastCDC parameters … re-derived from the measured file-size distribution of the volume.*

**What the tree did before.** The archiver stored every chunk raw (`Archive::raw_chunk`); the archive
crate had `compressed_chunk` (LZ4 if smaller) and `zstd_chunk` (level 0) as unconnected constructors
whose docs named the cost model as owed; the boot profile already measured every codec point
(`CodecPoint`: compress and decompress throughput and ratio for LZ4 and zstd levels 1/3/9/19) and
nothing read them.

**The change** — the model as a pure decision, applied by the archiver under a policy derived at boot:
- `crates/archive/src/codec.rs` (new, cfg-free, unit-tested with synthetic profile points): the Btrfs
  sampler with its constants reproduced by name (`SAMPLING_READ_SIZE 16`, `SAMPLING_INTERVAL 256`,
  `BYTE_SET_THRESHOLD 64`, `BYTE_CORE_SET_LOW/HIGH 64/200`, `ENTROPY_LVL_ACEPTABLE/HIGH 65/80`; the
  entropy in integer fixed point); the LZ4 probe with early exit (OpenZFS early abort, Borg `auto`);
  the per-level size prediction from the probe (the machine's own ratio points when both LZ4 and the
  level were measured, else Silesia's 0.73 prior); the maximization; the format floor (a chunk record's
  fixed fields, 82 bytes). Integer arithmetic throughout, so two hosts with one profile decide alike
  and the archive bytes are deterministic under one policy.
- `CodecPolicy { lz4, zstd: Vec<CodecRate>, byte_ns_scaled, value_of_byte_permille,
  value_of_cpu_permille, expected_reads }`; `Archive::chunk_with(bytes, &policy)`;
  `Archive::zstd_chunk_at(bytes, level)` records the chosen level on the chunk (the format's `level`
  byte; the reader needs no level to decode).
- `SnapshotArchiver::new(…, codec)` stores every distinct chunk through `chunk_with`;
  `DaemonConfig::codec` is derived at boot from `profile.codecs` and logged, the byte's neutral worth
  from `profile.memcpy` (largest probed size); `start_seal` passes it. A profile that measured no codec
  yields `CodecPolicy::raw_only()` — the same path with no candidates (R8).
- The **neutral exchange rate**, found by measurement: the objective compares bytes to nanoseconds, and
  the first cut priced a byte at one scaled nanosecond — a dimensionless coincidence that made every
  chunk raw. The derived rate: a byte is worth its measured memcpy cost (`byte_worth_from_memcpy`), so a
  codec pays when the bytes it saves would have cost more to move than to compress and decompress.

**Failing tests first and what they measured** (the archive crate; `cargo test -p slates-archive`):

| Test / arm | Verdict | Why |
|---|---|---|
| 8 KiB structured text, neutral (byte = 1 memcpy at 10 GB/s = 0.1 ns; read once) | **Raw** | LZ4 saves ~4 KB ≈ 400 ns of moves; compressing costs 8192 × 1.76 ns ≈ 14 µs — ~50× more. The design's "hot volumes stay raw unless pressure raises `value_of_byte`", measured. |
| the same text, `value_of_byte` = 100× | **LZ4** | the LZ4/zstd-1 crossover with these points: LZ4's speed and zstd-1's ratio tie within a few µs; LZ4 wins by a hair |
| the same text, `value_of_byte` = 10 000× | **Zstd(level)** | the ratio codec's extra saving dwarfs every cost |
| the same text, `value_of_byte` = 10⁶× | **Zstd(19)** | the strongest measured level when bytes are all that matter |
| 8 KiB xorshift noise, any value | **Raw** | the sampler's core set ≥ 200: incompressible, no probe run |
| all-zero 8 KiB | **Raw** (hole) | zero-detect first |
| 40 bytes of `A`s | **Raw** | the format floor: no saving can exceed the record's 82 metadata bytes |

The first assertion I wrote — "text under measured points is zstd at neutral" — was **wrong** and the
model was right: at neutral a chunk read once is cheaper to move than to compress by ~50× with the
profile-shaped points; the test now asserts the design's rule (raw at neutral, zstd when bytes are
precious) as its non-vacuous contrast, and the by-use archive test (`crates/archive/tests/archive.rs`)
does the same and shows the chosen level recorded on the chunk, the identity unchanged, a
policy-built archive round-tripping, and two archives under one policy encoding identically.

**Chunking.** The design gates FastCDC on two *measured per-volume* quantities nothing tracked:
`Walked { bytes_hashed, bytes_saved_by_dedup, raw_bytes, stored_bytes, file_sizes[65] }` on the
archiver now records them per walk (the dedup gain per byte hashed; the file-size distribution by
power-of-two class). FastCDC itself is **not** built: deriving its threshold and parameters needs those
measurements to have data over real volumes, and a fixed FastCDC regime would be a magic policy (R3).
Fixed page-multiple chunks remain, which the design names as the rule for large files.

**Owed, named from the measurement, not invented:** (1) the archive class's `value_of_byte` — a placed
byte occupies `f + 1` holders' RAM for the snapshot's retention, so its worth is that retention over one
memcpy time, per copy ("archived volumes compress once"), a derivation from the retention horizon and
`f`; (2) the live `value_of_byte(pressure)` and `value_of_cpu(load)` signals (the reserve's headroom;
the runtime's load) — until they are wired the daemon stores at neutral, i.e. raw, which is the design's
rule for a copy read once; (3) FastCDC derived from `Walked` once volumes have been measured; (4) the
regression's online update (zstd `--adapt`'s idea) — today the prior/point ratio is static per boot.

**On the final tree** (2026-09-13 19:38–19:40, load 4.5–5.9): `cargo test -p slates-archive` 48 passed;
`-p slates-vfs` 97 passed; `-p slates-server --lib` 22 passed; singly `-- --exact`: seal 3.96 s, hedge
6.63 s, healer 14.50 s, takeover-content 9.22 s (the daemon archiving under its real profile's codec
points at neutral); `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo xtask check` clean.

## Benchmark for the integrator (a quiet box)

Not run here (the box is shared). The cost model's per-chunk cost at the profile's own points:

```
cargo test -p slates-archive --release --lib codec -- --nocapture
```

and the seal-to-placed wall time with compression on versus `CodecPolicy::raw_only()` is the number
to record in `BENCHMARKS.md` once the archive-class `value_of_byte` derivation lands (at neutral both
store raw, so the A/B is not yet informative).
