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

## Pieces 2 and 3

Not yet started at the time of this record's first commit; see the sections appended below as they land.
