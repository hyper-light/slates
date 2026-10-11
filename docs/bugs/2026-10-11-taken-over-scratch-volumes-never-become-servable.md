# Taken-over contentless (scratch) volumes never become servable (2026-10-11)

## Description

After a host dies and its objects are taken over, a **contentless volume** (a scratch volume created but never
sealed — no content manifest) has its head and catalog **registers adopted** by the successor, but the successor
**never builds the servable db catalog entry** for it. A client reading such a volume gets a **retryable
`Overloaded`** forever from its owner (`owned_unmaterialized`), and a client whose read is **forwarded** to the
owner gets a false **`NotFound`** — telling it a volume that exists does not.

This surfaces only at scale/under pressure: the single-volume takeover tests check head **placement**
(`poll_head_placed`), not Status **serving**, and the content-volume tests write "hello" (so they have a manifest
and take the content path). No existing test reads a *scratch* volume's Status after takeover, so the gap was
uncaught.

## Reproduction (rigorous, under pressure)

`crates/server/tests/fleet.rs::a_client_reading_a_recovering_object_sees_what_a_successor_answers`
(`cargo test --release -p slates-server --test fleet a_client_reading_a_recovering_object -- --ignored --nocapture`):
3-node `f=1` fleet, **1024 scratch volumes** provisioned on A, A killed, then a low-id and a high-id object read
with one-period backoff from the moment A is retired until served or 60 s.

Result (this M-series box, 2026-10-11): **neither object served in ~41 s (411–418 periods).** Counters (temporary
instrument `fleet.materialize.head_only_dropped` added at the `manifest=None` branch):

- `fleet.materialize.head_only_dropped` = 530 + 494 = **1024** — *every* scratch volume dropped from
  materialization.
- Owner-local object: `head_placed=true, catalog_placed=true`, `fleet.owner_location.owned_unmaterialized ≈ 390`
  → `Overloaded` the whole window.
- Owner-remote object: fully adopted on its owner (`head_placed=true, catalog_placed=true`), yet the forwarding
  node returns `NotFound` ≈ 385 times, correlated with `fleet.owner_location.route_dropped.refused ≈ 385` and
  `no_session ≈ 438`.
- `fleet.takeover.adopted ≈ 1000–1060` — adoption of the registers nearly completes; serving does not.

## Root cause

**Bug 2 (confirmed — the primary bug).** `crates/server/src/fleet.rs::materialize_pending`: a head whose
`manifest` is `None` (a contentless/scratch volume) is **removed from `pending_materializations` without building
the db entry** ("nothing to serve beyond the head"):

```rust
let Some(manifest) = head.manifest else {
  state::with_state(|s| s.pending_materializations.remove(&object));
  continue;
};
```

The servable db `VolumeRecord` is built only by `materialize()` → `verbs::materialize_taken_over(...)`, which
**requires a content `archive`** (it restores the archive, reserves space). A scratch volume has no archive, so
`materialize_taken_over` is never reached, and the db entry is never created — so `find(state, volume)` fails
forever, and every verb on the volume hits `owned_unmaterialized` → `Overloaded`. The register adoption
(head + catalog) is irrelevant to `find`, which reads the db catalog.

**Bug 1 (observed — possibly a consequence of Bug 2).** A read forwarded to a taken-over object's owner returns a
false `NotFound` (not a retryable refusal). The forward reaches an owner that *also* has Bug 2
(adopted-but-unmaterialized), so the owner refuses; the route is dropped (`route_dropped.refused`) and the
client's next locate round, under session pressure (`no_session`), ends in `NotFound`. Whether Bug 1 is wholly a
consequence of Bug 2 or an independent false-`NotFound`-on-forward-failure path is **not yet isolated** — it will
be re-measured after Bug 2 is fixed.

## Impact

A freshly provisioned-but-unwritten volume (every OCI/container volume before its first write; every `scratch`
mount) **vanishes from service when its owner dies** — permanently `Overloaded`/`NotFound` until written or
re-created. Condition 3/4/11 (fast, correct failure recovery): a taken-over scratch volume is a lost object from
the client's view. This is a correctness/availability bug, not merely a latency tail.

## Proposed fix (optimization hierarchy: the work must happen, just without content it doesn't have)

- **Bug 2:** the `manifest=None` branch must **build a head-only db entry** from the adopted head + catalog
  registers — a content-free materialization that creates the `VolumeRecord` (as a normal scratch `Create` does:
  `BaseRecord::Scratch`, empty content, the catalog's name/owner/policy) via `Op::VolumeCreated`, then removes it
  from `pending_materializations`. No archive restore, no space reservation. Likely a `materialize_taken_over`
  head-only path (mirrors the green variant's db-entry creation at `verbs.rs:7051`), guarded by the recovery
  oracle.
- **Bug 1:** re-measure after Bug 2. If a false `NotFound` persists, a forward whose owner is unreachable/refusing
  must return a **retryable** refusal (`Overloaded`/`OwnerUnreachable`), never `NotFound`.

## Validation plan

- Turn the reproduction into a gating assertion (a scratch volume must be Status-servable within a bounded number
  of periods after takeover; `NotFound` must never appear for an existing object).
- `crates/server/tests/recovery.rs` (the crash-at-every-step oracle) must stay green — it is the arbiter for
  takeover/materialization changes.
- Remove the temporary `head_only_dropped` counter, or keep it renamed as a permanent "head-only materialized"
  observability counter.

## Fix (landed 2026-10-11)

`crates/server/src/verbs.rs` — `materialize_taken_over` now takes `archive: Option<&Archive>`; the archive restore
and `populate_restored` run only when an archive is present. A contentless volume creates the same `VolumeRecord`
(`BaseRecord::Scratch`, empty), reserves its credits, `Volume::create`s the empty tree, and seals the head
snapshot (identity `None`) — identical to a fresh scratch `Create`, just with the adopted catalog's metadata.
`crates/server/src/fleet.rs` — `materialize` no longer early-returns on `manifest=None`; it threads an optional
archive through and calls `materialize_taken_over(..., opened.as_ref())`. `materialize_pending` routes a
contentless volume to `materialize` (which builds the head-only entry) instead of dropping it.

**Bug 1 is mostly, but not wholly, a consequence of Bug 2.** Its *permanent* form is gone — once the owner
materializes, the forward succeeds and the object serves (both ends at ~24/33 periods). But a **small transient
false `NotFound` remains** in the early settle window (0–4 replies across runs): before the successor has processed
the committed retirement (`resolve_held_objects` → `track_object_owner`/`departed_owners`), it has no basis to
answer "mine, retrying", so `find()` answers `NotFound` for an object it already *holds a copy of*. This is a
pre-existing ownership-propagation race, revealed — not caused — by this fix, and it is **open** (Bug 1 below).

## Validation

- Regression gate `crates/server/tests/fleet.rs::a_taken_over_scratch_volume_becomes_servable` (1024 scratch
  volumes, kill A, read a low-id and high-id object through takeover): **before** — neither served in ~41 s,
  `head_only_dropped = 1024`, `NotFound ≈ 385`; **after** — both served at ~24 / ~33 periods (~2.4 / 3.3 s), with a
  handful of retryable `Overloaded` and a small transient `NotFound` (0–4) in the settle window (Bug 1).
- `crates/server/tests/recovery.rs` (crash-at-every-durable-step oracle, the arbiter): **39 passed, 0 failed** —
  the head-only materialization is recovery-correct.
- The temporary `fleet.materialize.head_only_dropped` counter was removed with the fix (the branch no longer drops).

## Bug 1 (open) — transient false `NotFound` in the settle window

A client reading a taken-over object in the window between the retirement committing and the successor processing
it (`resolve_held_objects`) can get `NotFound` for an object that exists. The fix is to not answer `NotFound` for
an object this node holds a copy of or is a recovery successor for (`holder_records`/the retirement's lineage) —
return a retryable `Overloaded`/`OwnerUnreachable` instead. To be root-caused and fixed next; the gate's
`not_found == 0` assertion lands with it.

## Status

**Bug 2 FIXED 2026-10-11** (taken-over scratch volumes are servable). **Bug 1 OPEN** (transient settle-window false
`NotFound`, pre-existing, now visible).
