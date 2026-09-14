# 2026-09-13 — `status --json` and `slates.status` omitted every shard's health signals

## Description

`slates status` (text) printed one block per shard — counters, refusals by kind, the six health
signals with their absence meaning and age, and the telemetry counts — but `slates status --json`
and the MCP `slates.status` tool (`slates_mcp::daemon_json`) carried only `"shards": <count>` beside
the daemon's counters and the fleet block. The JSON form, which §4.12 requires to be *the same
definition* as the text form ("one definition, two surfaces") and which §4.14 requires to expose the
signal definitions "consistently through CLI/MCP", exposed no health signal at all: an agent or
script reading the machine form could not see `catalog.volumes`, `lease.expiring`, `ring.depth`,
`log.replay_ns`, nor what an absent value would mean.

## Root cause

`daemon_json` was written when the report had no per-shard signals and was never widened when
`ShardReport` gained them (A-9 typed absence, the `spans_held`/`spans_dropped` counts, `peers_probed`);
the CLI test `json_daemon_status` only asserts `"pid":` and `"shards":` are present.

## Impact

Every consumer of the machine-readable status since the signals landed: the MCP tool description
("generation, restarts, shard count") was honest about the omission but the parity rule was broken.

## Fix

`crates/mcp/src/lib.rs`: `daemon_json(report, telemetry)` renders `"shards"` as an array of shard
blocks (`shard_json`: the counters, `refusals`, `signals` via `signal_json` — `value` `null` when
absent with `absence` naming the meaning — `spans_held`, `spans_dropped`, `peers_probed`, and the
shard's `telemetry` drain via `telemetry_json`); `gather_daemon_status` is the one gather both the
CLI and MCP use. The MCP tool description says what the report carries.

## Evidence

`cargo test -p slates-mcp --test mcp` — `assert_status_exports_the_registries` on a live in-process
daemon: two shard blocks; `catalog.volumes` numeric with `absence: "degraded"`; nine chokepoints in
roster order; `archive.chunk` `fresh: false`, `latest_age_ns: null`, `absence: "unknown"`,
`expected: false`; a span with `request`, 32-hex `trace`, `span`, typed `cause`. Before the fix the
same assertion fails at `status["shards"].as_array()` ("every shard's block, not a bare count").
After: ok (1.09 s, 2026-09-13).
