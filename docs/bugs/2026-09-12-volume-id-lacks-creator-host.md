# The volume id encoded the owner partition, not the creator host — cross-region lookup misrouted

- **Date:** 2026-09-12
- **Area:** §4.8 "Lookup" — the volume id, `verbs::fresh_volume_id` / `owner_of`, and the cross-region
  routing guard (`verbs::home_redirect`, slice 1 `791f2bd`).
- **Severity:** correctness — a multi-region fleet's cross-region lookup resolved the wrong region for a
  real volume; a node in a non-zero region would redirect even its own volumes elsewhere.
- **Found by:** building cross-region volume-read forwarding (slice 2). The end-to-end test — a client
  reading a volume created in another region — was the first path to route a *real* volume id (not a
  synthetic one) through the lookup guard, and it exposed the id layout.

## Description

The design says a volume id carries its creator host: "A volume id carries its creator host. A lookup by
id routes to that host" (§4.8), and the register object is "the creator-routable 128-bit `ObjectId` (high
half = creator host)". `ObjectId::creator()` accordingly reads the high 8 bytes as a `HostId`.

But `fresh_volume_id` filled the high bytes with `state.partition` (the owner shard's index), not the
creator host — a Phase-8 placeholder its own doc flagged ("the shard in the high bytes (the creator host's
place in Phase 8)"). So a volume id named **no creator host**: `ObjectId::creator()` returned a value
derived from the partition and the clock, never the fleet member id.

Consequences:

- **Slice 1's lookup guard misrouted (latent, multi-region only).** `home_redirect` computes the home
  region as `home_of(object, region_of(object.creator()))`. With `creator()` not a fleet member id,
  `region_of` (a lookup in the member-id-keyed `node_regions`) missed and defaulted to region 0. So the
  guard computed home = region 0 for *every* volume. A node in region 0 saw its own volumes as local
  (accidentally correct); a node in region ≠ 0 saw *every* volume — including ones it owns — as homed in
  region 0 and would redirect them. The unit tests passed only because they fed `home_redirect` synthetic
  `ObjectId`s with correct creators; no test had routed a real created volume id through the guard.
- **Slice 2 could not identify the owner node.** Forwarding a cross-region read to the volume's owner
  needs the owner node's fleet id; `creator()` did not yield it, so the forward went to a non-member id
  with no session and failed.

## Root cause

`fresh_volume_id` wrote the owner partition into the high bytes (`bytes[..2]`) instead of the creator
host, so the creator half of the 128-bit id held no host. `owner_of` read the partition from those same
high bytes.

## Fix

`crates/server/src/verbs.rs`:

- `fresh_volume_id` now writes the **creator host** (`state.fleet.host()`, the fleet member id) into the
  high 8 bytes, the owner **partition** into bytes 8-9, and the per-host counter into the low 6 bytes. So
  `ObjectId::creator()` returns the creator's member id, and `region_of(creator)` resolves its region.
- `owner_of` reads the partition from bytes 8-9 (just below the creator host) instead of the high 2 bytes.

The create runs on the owner shard (the request is routed to `owner_of_name(name)` first), so
`state.partition` there is the owner partition; `owner_of(id)` recovers it. Placement (`candidates_for`
/ rendezvous) hashes the whole id and is deterministic regardless of layout, so it stays consistent. The
clock is dropped from the id; the per-host counter (`next_seq`, monotonic within a boot) keeps ids
unique on a host, and cross-boot uniqueness is the ephemeral-membership-id work's concern (task #22),
unchanged by this (the previous monotonic clock reset on restart too).

The only readers of the volume-id partition were `fresh_volume_id` and `owner_of`; every `owner_of`
caller routes through the function; `creator()` (high 8 bytes) is unchanged and now yields the host.

## Test

`crates/server/tests/fleet.rs::a_client_reads_a_cross_region_volume_by_forwarding_to_its_owner`: three
daemons, each its own region; a volume created on node a (region 0) is read by a client on node b (region
1); b forwards the read to a and relays the reply. It failed before the fix (the forward went to a
non-member creator id and never reached a) and passes after — so it is non-vacuous, and it exercises both
the fix (the id now names a's member id) and slice 2's forwarding.

## Sibling sweep

- Client ids also carry the partition in their high bytes (`state.partition`, per `state.rs`), but they
  are a distinct id space, never routed by `creator()` or `owner_of`, so they are unaffected and were
  left as they are.
- Attachment ids carry their owner partition in a separate scheme (`verbs.rs` attachment id doc) and are
  routed by `owner_of_attachment`, not `owner_of`; unaffected.
- `moved_home` / region-promotion routing still resolves through `RootConfiguration::home_of`; with the
  creator host now in the id, a moved or promoted volume whose owner is no longer its creator is still an
  owed follow-on (forwarding to a moved-owner needs the home region's placement), and the guard refuses
  `HomedElsewhere` for the caller to re-route in that case, which is correct.
