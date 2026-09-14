# An anchored daemon's first boot runs one generation ahead of its manifest seed, so a real fleet never forms

Date: 2026-09-14
Area: `crates/server/src/daemon.rs` (`init_shard`, the ephemeral member-id derivation)
Severity: a fleet of anchored daemons — every real deployment (`slates anchor --fleet …`, the Helm
chart, the KIND lane) — never converges its mesh. SWIM probes, the record commit and content placement
all stall. NOT caught by any test: the in-process fleet suite, the sim fabric and the three-process
loopback CLI test all run daemons **without an anchor**, so they never exercised it.

## Symptom (KIND lane, three pods each on a distinct worker node, 2026-09-14)

Every pod Ready, DNS resolving, cross-node UDP flowing, both planes' TLS sessions establishing both
directions — yet the mesh holds at `fleet_peers_probed` 0/1/1, no council leader, `samples 0`, and a
volume never places (`placed=False`; peers `NotFound`).

Instrumenting the probe outcome and the served messages showed the Ping/Ack round trip **works** (acks
at ~0.1 ms) but every probe is *"answered under a different id"*:

```
slates-1 (host 658…): probe of 12209328545412516556 answered under a different id 343910890905646160
slates-2 (host 817…): probe of 15082333039372664391 answered under a different id 658110181894367458
```

`343910890905646160` is slates-0's **actual runtime** member id; `12209328545412516556` is the id its
peers **seeded and probe**. They differ by exactly one generation.

## Root cause

A node's ephemeral member id is `member_id(anchor, generation)` (§4.8 "Recovery", task #22). The
deployment manifest precomputes and every peer seeds the **generation-0** id `member_id(anchor, 0)`
(`crates/server/src/deploy.rs`, the design's "a fresh fleet forms with no exchange"). But `init_shard`
took the generation straight from the anchor's `SUP_GENERATION`:

```rust
let generation = segment.supervision().map(|s| s.generation()).unwrap_or(0);
let host = member_id(origin_anchor, generation);
```

and `AnchorSegment::record_start` does `self.generation.fetch_add(1)` on **every** start — so an
anchored **first boot reads generation 1**, and the node runs as `member_id(anchor, 1)`, one ahead of
the `member_id(anchor, 0)` its peers seed and probe. Every probe is therefore sent to an id nobody
holds and answered under the runtime id; `probe_and_apply` sees `from != peer.host`, never calls
`on_ack(peer.host)`, and the detector ages the peer to dead and retires it. Formation is forced through
the learn-on-contact path (`learn_member` → `follow_current_id`) — which exists for *restarts*, not
initial formation — and does not fully converge (the node all peers learn toward, slates-0, never
switches to acking its own peers). The record plane's commit stalls the same way, so nothing places.

`SUP_GENERATION` counts **starts** (1 on a first boot); the member-id incarnation the design means is
the number of **restarts** — 0 on a first boot. The code used the start count directly. The comment
right above the bug already stated the intended invariant — *"generation 0 (a first boot …) reproduces
the manifest's precomputed gen-0 seed"* — so the code violated its own documented contract.

Why no test caught it: the in-process fleet tests, the sim fabric, and the three-process loopback CLI
test start daemons directly (no `slates anchor`), so `supervision().generation()` is 0, the seed equals
the runtime id, and they converge. Only an **anchored** daemon (generation ≥ 1) hits it — first proven
on the KIND lane's real pods, and it reproduces equally on loopback under an anchor (it is not a
network defect).

## Fix

The member-id incarnation is `boot_incarnation(supervision_generation) = supervision_generation − 1`
(saturating): the number of restarts, 0 on a first boot, so a first boot's id **is** the manifest's
gen-0 seed and a fresh fleet forms with no learn-on-contact; a restart (a higher start count) is a
distinct id the group takes over (task #22 preserved). A daemon run alone (generation 0) is likewise
incarnation 0. `SUP_GENERATION` still counts starts for restart detection and the status `generation`
field — only the member id changes.

## Tests

- Unit (red→green on the invariant): `an_anchored_first_boot_holds_the_manifest_gen_zero_seed`
  (`crates/server/src/daemon.rs`) — `member_id(anchor, boot_incarnation(1)) == member_id(anchor, 0)`
  (an anchored first boot is the seed), `boot_incarnation(2) == 1` (a restart is a new member). Fails
  on the old `generation` derivation (`member_id(anchor, 1) != member_id(anchor, 0)`).
- By use (the charter's gate): the three-pod KIND fleet, which held at `peers_probed 0/1/1` before the
  fix, forms fully after it — `fleet_peers_probed 2` on every pod, one `fleet_council_leads true`,
  `fleet_council_samples 786/1491/789` (measured, not the floor), `rtt_tail ≈ 4–5.7 ms` (the real
  cross-node pod path). Measured 2026-09-14.
- No regression: the in-process fleet suite (see the lane record).

## Siblings

- The learn-on-contact / `follow_current_id` path is correct and still needed for genuine restarts;
  it was simply being asked to do initial formation's work. Unchanged.
- No other site derives a member id from the raw `SUP_GENERATION`: `deploy::plan` uses the constant 0
  seed and `deploy::member_id` is the one derivation, now fed the incarnation everywhere.
