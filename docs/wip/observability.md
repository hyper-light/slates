# Observability (§4.14) — the living record

This is the measurement and implementation record for §4.14 (D-23) of `SLATES_DESIGN.md`. The design
is the law; this file records what is built, how it was proven, the numbers, and what is owed. The two
registry tables below are **generated from the code** by doc-truth tests and must not be edited by hand:
`cargo test -p slates-wire --lib -- --ignored regenerate_the_chokepoint_table` and
`cargo test -p slates-ipc --lib -- --ignored regenerate_the_health_signal_table` rewrite them; a normal
test run only compares and fails on drift.

## The chokepoint-span registry (`slates_wire::observe::Chokepoint`)

<!-- chokepoints:begin -->
| Chokepoint | Dimension | Absence means | Expected producer | Observer | Freshness horizon |
|---|---|---|---|---|---|
| `bridge.request` | `{op}` | unknown | filesystem bridge | owner shard (self-observed) | failover SLO |
| `ring.request` | `{kind}` | unknown | client verb | owner shard (self-observed) | failover SLO |
| `shard.op` | `{verb}` | unknown | client verb | owner shard (self-observed) | failover SLO |
| `log.append` | `{partition}` | unknown | client verb | owner shard (self-observed) | failover SLO |
| `ship.record` | `{object}` | unknown | record replication (f ≥ 1) | owner shard (self-observed) | failover SLO |
| `consensus.step` | `{group}` | unknown | configuration group | owner shard (self-observed) | failover SLO |
| `archive.chunk` | `{codec}` | unknown | archive codec | owner shard (self-observed) | failover SLO |
| `land.entry` | `{action}` | unknown | landing under grant | owner shard (self-observed) | failover SLO |
| `merge.verdict` | — | unknown | client verb | owner shard (self-observed) | failover SLO |
<!-- chokepoints:end -->

## The shard health-signal registry (`slates_ipc::protocol::HealthSignal`)

Registry signals the design's §4.14 catalog does not list yet, held on a reviewed drift list that
only shrinks (`DESIGN_CATALOG_DRIFT` in `crates/ipc/src/protocol.rs`): `shard.clients`,
`shard.deferred`. The registry's `ring.depth` is one value per shard (the clients' rings summed) where
the catalog says `ring.depth{client}`. Owed to the design (an integrator edit of §4.14's catalog).

<!-- health-signals:begin -->
| Signal | Absence means | Freshness | Producer | Observer |
|---|---|---|---|---|
| `catalog.volumes` | degraded | measured at report (age 0) | the shard's volume catalog | owner shard (self-observed) |
| `log.replay_ns` | unknown | measured at boot (age = time since boot) | the last recovery replay | owner shard (self-observed) |
| `lease.expiring` | degraded | measured at report (age 0) | the shard's lease table against the failover SLO | owner shard (self-observed) |
| `ring.depth` | degraded | measured at report (age 0) | the clients' command rings, summed | owner shard (self-observed) |
| `shard.clients` | degraded | measured at report (age 0) | the shard's client slots | owner shard (self-observed) |
| `shard.deferred` | degraded | measured at report (age 0) | the shard's deferred-reply queue | owner shard (self-observed) |
<!-- health-signals:end -->

## What is built (2026-09-13, GAP-A9-12; AC-0.11/T-0.11)

### 1. Truth of the registry

Two closed registries, each the single source of its documentation:

- `Chokepoint` (`crates/wire/src/observe.rs`): name, dimension (`{op}`, `{kind}`, …), what absence
  means (`AbsenceIs`), expected producer, observer, freshness horizon, and `registry_table()`.
- `HealthSignal` (`crates/ipc/src/protocol.rs`): name, absence, freshness basis (`FreshnessBasis`:
  at report / since boot — `shard_report` now ages each signal by this, not by a rule of its own),
  producer, observer, and `registry_table()`. `AbsenceIs` moved down into `slates-wire` and is
  re-exported by `slates-ipc`, so both registries speak one vocabulary.

Doc-truth tests (normal runs only compare; `--ignored regenerate_*` rewrite the block):

- `the_span_roster_is_the_designs_and_its_count_word_is_true` (wire) parses the design's own
  "*Span roster.*" sentence — its count word ("Nine"), its backticked `name{dimension}` tokens in
  order — and asserts they equal the registry. "Nine chokepoints called seven" is now a failing
  assertion against the design text.
- `the_recorded_chokepoint_table_is_the_registry` / `the_recorded_health_signal_table_is_the_registry`
  compare the generated blocks above byte for byte.
- `every_registry_signal_is_in_the_designs_catalog_or_on_the_reviewed_drift_list` (ipc) parses the
  design's "*Health signal catalog.*" sentence; the two names above are the drift it found.

### 2. Causation, enforced by the types

`crates/wire/src/observe.rs`: a `Span` is built only by ending an `OpenSpan`; an `OpenSpan` is opened
only by a shard's `Tracer` — `open_root(request, …)` at an entry point (a ring slot read, a bridge
call: `Cause::Root`), `open_within(&cause, …)` for a child (same request, same trace, `Cause::Span`
naming the parent — a child that disagrees with its cause cannot be constructed), or
`open_unlinked(request, …)` when a cause existed but was not carried across a boundary
(`Cause::Missing`, the explicit "missing causal link"). `SpanContext`'s fields are private. Trace ids
are minted by the tracer (node ‖ partition ‖ counter, 128 bits), never derived from the request word;
span ids carry the partition above a 48-bit counter, so neither collides across shards.

Wired in `crates/server`: `serve_client` opens the `ring.request` root at the slot read and the
verb's `shard.op` opens within it (`run_recorded`), `log.append` within `shard.op`, `merge.verdict`
and `land.entry` within the current verb; a same-node forward carries the ring context in its spawn
closure (`send_forward`, `PendingForward.cause`) so the owner's `shard.op` is caused by the origin's
ring span — one trace across the shard boundary; a cross-node forward (no trace context on the fleet
envelope yet) opens unlinked. The origin's ring span of a forwarded or scattered request is kept in a
bounded map (`forwarded_rings`, the clients' credit; past it, shed and counted) and ended when
`deliver`'s reply is written — closing the previously owed "ring.request for a forwarded reply". The
NFS `bridge.request` root carries the RPC xid under the mount's port as its replay identity.

Proven by use (`crates/server/tests/daemon.rs`, `telemetry_scenario`): one `Create`, both rings
drained, the request's spans found by its id: `ring.request` `Root`; `shard.op` caused by the ring
span (the owner was the other shard in the run — the cause crossed the shard boundary); `log.append`
caused by `shard.op` and within it on the owner's clock; one trace on all three, not the request
word; three distinct span ids; `missing_links` 0.

### 3. Emission: the operator-facing export

`crates/server/src/telemetry.rs` and the `Telemetry { partition }` verb (`slates-ipc`, appended
last in both body enums): a bounded drain of one shard's ring. Bounded end to end: a reply rides one
bulk chunk (4096 bytes), so the quota is derived at boot from the encoded sizes —
`telemetry_spans_per_reply = (4096 − 679) / 68 = 50` spans (measured by
`the_reply_quota_is_exactly_what_one_chunk_holds`: a report at the quota fits, one more does not).
The batch carries typed loss markers: `shed_before` (spans the ring shed since the previous drain,
lost before this batch), `dropped_total` (since boot), `remaining` (left for the next drain),
`missing_links`. Freshness: every chokepoint's newest span is judged against the horizon (the
failover SLO, `horizon_ns` in the report); older, or none, is `fresh: false` with the registry's
absence word — the last sighting is stated as an age, never reported as a live value — and
`expected` says whether a producer of it runs on this host (a laptop's `ship.record`,
`consensus.step`; the archive codec nowhere yet).

The status surfaces read it through one gather (`slates_mcp::gather_daemon_status`): `slates status`,
`slates status --json` and the MCP `slates.status` drain every shard's ring. `daemon_json` now
carries every shard's block — counters, refusals, health signals with `absence`, and `telemetry` —
where before it carried only a shard count (the parity defect,
`docs/bugs/2026-09-13-status-json-omitted-shard-signals.md`).

Proven by use: `telemetry_scenario` (server): as many `Status` verbs on the owner shard as its ring
holds (`region.slots`, derived from the quick profile's Little's-law ring entries — 256 on one run,
64 on a more loaded one; the scenario reads the derived value) → the next drain marks the loss
(`shed_before` 258 = `dropped_total` on the 256 run; 66 on the 64 run), carries ≤ 50 spans per
batch, and the batches (6 and 2) drain what was held plus the drain verbs' own spans (266 of 256;
66 of 64) with `remaining` reaching 0 and no loss between back-to-back drains; `archive.chunk`,
`ship.record`, `consensus.step` typed absent/unknown, no age, not expected. `the_mcp_surface_serves_the_tools`
(mcp): `slates.status` — the same `daemon_json` the CLI prints — shows two shard blocks, a numeric
`catalog.volumes` with `absence: degraded`, nine chokepoints in roster order, `shard.op` fresh after
the lifecycle verbs, `archive.chunk` `fresh: false`, `latest_age_ns: null`, `absence: unknown`,
`expected: false`, and a span with `request`, a 32-hex `trace`, `span` and a typed `cause`.

### Commands and results (2026-09-13, Darwin 25.4.0 arm64, shared box under two other builds)

- `cargo test -p slates-wire --lib` → 29 passed, 1 ignored (the regenerate writer).
- `cargo test -p slates-ipc --lib` → 7 passed, 1 ignored.
- `cargo test -p slates-server --lib` → 25 passed (telemetry: `fixed part 679 bytes, span record
  68 bytes, quota 50 spans per 4096-byte chunk`).
- `cargo test -p slates-cli --bin slates` → 18 passed.
- `cargo test -p slates-server --test daemon the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals -- --exact`
  → ok, 15.24 s (prints `telemetry: ring capacity 256 spans, reply quota 50; after 256 status verbs
  the owner shard 0 shed 258 (dropped_total 258), held 256, drained 266 over 6 batches`); a second
  run under heavier load derived a 64-slot ring: `shed 66 (dropped_total 66), held 64, drained 66
  over 2 batches`, ok, 16.07 s.
- `cargo test -p slates-mcp --test mcp` → ok, 1.09 s.
- `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask check`
  (structural ok over 26 crates, literals ok, unsafe ok) → clean.

Found on the way and fixed (failing test first): an acknowledgement never released a completion
(`docs/bugs/2026-09-13-acknowledge-keyed-on-ephemeral-member-id.md`).

### Owed

- `ship.record`, `consensus.step`, `archive.chunk` emitters: gated on their subsystems being live
  (a laptop runs no replication or consensus; the archive codec pass is Phase 7); the registry
  reports them typed absent with `expected` false meanwhile.
- Cross-node trace propagation: the fleet forward envelope carries no trace context, so an owner on
  another node opens the verb unlinked (`Cause::Missing`, counted in `missing_links`).
- `Degraded` for a chokepoint: every chokepoint is event-driven, so absence is `Unknown`; a
  cadence-driven chokepoint would declare `Degraded` in `Chokepoint::absence`.
- The daemon status scatter has no deadline: a shard that does not answer hangs `status` rather
  than reporting its signals absent/degraded (the shard-level "drop a producer" case).
- The whole `DaemonReport` rides one 4096-byte chunk and grows ~315 bytes per shard (before this
  change; the per-shard block is unchanged here): past ~12 shards `status` is refused
  `BadRequest("reply too large for the bulk area")` by `send_reply`. The telemetry export was kept
  off that reply for exactly this reason; the status report itself needs the same paging.
