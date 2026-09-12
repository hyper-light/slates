# Root configuration not propagated to non-control shards (cross-region lookup reads stale home)

- **Date:** 2026-09-12
- **Area:** §4.8 "Lookup" / D-14 (cross-region routing), the multi-shard daemon
- **Severity:** correctness — a cross-region request served on a non-control shard reads a stale home
- **Found by:** building the `home_of` lookup guard (cross-region routing slice 1, `791f2bd`); a
  multi-shard daemon exposes it, a single-shard/laptop daemon does not.

## Description

The `home_of` lookup guard (`verbs::home_redirect`, added in `791f2bd`) refuses a request for a
volume homed in another region with `Refusal::HomedElsewhere{region}`, so the caller re-routes. The
guard runs inside `verbs::serve`, which executes on whatever shard a client's request lands on — every
shard serves clients — and reads that shard's committed root configuration (`state.root.configuration()`).

But the root group is driven over the transport from the record-plane coordinator (`run_record_plane`
→ `drive_root_group`), which runs on the **control shard alone**. Its `state::with_state` calls, and
the root group's committed-entry application, touch only the control shard's `s.root`. A region move
or an operator region-loss promotion therefore advances the control shard's root configuration while
every other shard's `s.root` stays frozen at the boot configuration.

Consequence: on a multi-shard daemon, a client whose request lands on a non-control shard reads the
**pre-promotion / pre-move home** — the guard fails to redirect a volume that has been re-homed, or (once
slice 2 forwards on the refusal) would redirect to the wrong region. A single-shard daemon (and the
laptop, R8) has only the control shard, so it is unaffected — which is why slice 1's unit tests and the
single-shard operator-promotion fleet test pass.

## Root cause

Committed configuration is installed on the control shard only:

- The council's placement configuration is installed by `sync_config_from_council`, called from
  `run_record_plane` (control shard) via `with_state`.
- The root configuration is applied by `drive_root_group` (control shard).

The SWIM **failure view** is fanned out to every shard (`fold_peer_state` → `run_on` →
`apply_peer_state`), but after the authority switchover (`36727f0`) `apply_peer_state` advances only
the failure view, not the configuration. Nothing fans the committed **configuration** (root or
placement) out to the other shards. The design's D-7 "one owning shard per volume" means each shard
owns different volumes, but every shard must agree on region homes to answer the lookup guard.

## Impact

- Cross-region lookup (`home_of`) inconsistent across shards after a region move or promotion on a
  multi-shard fleet node. Single-shard / laptop unaffected.
- Latent: no client can yet act on `HomedElsewhere` (slice 2, the cross-region data transport, is
  owed), so today the observable is confined to the guard's decision on a non-control shard.

## Fix (root configuration)

`crates/server/src/fleet.rs`: new `sync_root_to_shards(origin, shards)`, called from
`run_record_plane` each period after `sync_config_from_council`. The control shard reads its committed
root configuration and hands it to every other shard via the existing `run_on` cross-shard call; each
shard **adopts** it (`RootGroup::adopt`, which is version-gated — an equal-or-older configuration is a
no-op), exactly as a root learner adopts a voter's configuration over the wire, here a same-process
copy. Bounded: one control-plane message per non-control shard per period, and no state change on a
converged fleet (the root-commit rate is near zero). No new state — a non-control shard already holds a
`RootGroup` (built at boot); it never votes or receives root Raft traffic, so `adopt` is the only thing
that moves its configuration, and it is safe against the Raft term/log (adopt touches only the
committed `configuration` field).

`crates/server/src/daemon.rs`: `Daemon::region_home_on_shard(shard_index, volume, creator_region)` — a
test observer answering `home_of` on a chosen shard rather than the control shard (the control-shard
`region_home` masked the bug).

## Test (failing-first, non-vacuity-checked)

`crates/server/tests/fleet.rs::a_committed_promotion_reaches_every_shards_lookup_view`: three daemons,
each its own region, **two shards each**; an operator promotes a region on the root leader; every
daemon's control shard re-homes the region's volumes to the mirror (existing behaviour) **and** every
daemon's non-control shard does too (the fan-out). Verified to fail with the fan-out call removed —
only the non-control-shard assertion fails, the control-shard one passes, so the test exercises exactly
the fix.

## Sibling sweep

- **Placement configuration is not fanned out either** (open, tracked in GAPS row 27). A non-control
  owner shard reads a stale placement configuration after a membership change, so verb reads that go
  through `Configuration` (`region_placed`/`place`/`host_epoch`) can be stale there. It is partially
  masked today because the placement *report* (`committed_placement`) reads `placed_heads`, which the
  record plane records per owner shard via `xshard`. Fixing it is entangled with distributing takeover
  (each `install_configuration` returns reassignments the owner shard should drive), so it is a
  separate increment, not folded into this root-only fan-out.
- No other per-shard state read on a client path depends on a control-shard-only configuration: the
  detector's failure view is already fanned (`fold_peer_state`), and `placed_heads` is recorded per
  owner shard.
