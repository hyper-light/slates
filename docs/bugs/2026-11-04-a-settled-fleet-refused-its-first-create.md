# A settled fleet refused its first create

This attributes the failure recorded open in
`docs/bugs/2026-11-02-a-fleet-routing-fixture-discarded-its-unexpected-create-reply.md`. Main CI
[41538865390](https://github.com/hyper-light/slates/actions/runs/41538865390) on `8a3fdc63`, Linux job 125597589278,
failed `a_location_round_asks_a_peer_whose_session_was_out_once_it_returns` (`crates/server/tests/fleet.rs`, §4.8
Lookup) at `choose_copyset_routing_case`'s first create. The panic printed no verdict then; the helper has printed it
since.

## What the CI log shows

Read again on 2026-11-04 (`gh api repos/hyper-light/slates/actions/jobs/125597589278/logs`, on kiln-a094a12d). The
fixture's mesh formation poll ended at once (`verdict=held wall=0.001s`, `periods=12`). The test then connected to the
owner and its first `Create` was answered in 0.000 s, the panic. So the owner refused the create at once, before any
work. For a daemon that has just started, the refusals that answer at once are:

- `ConsensusNotInitialized`, refused by `dispatch` before the verb runs whenever the shard's `consensus_ready` is unset;
- the admission budgets, which a 1 MiB create cannot exhaust on a fresh daemon;
- `AlreadyExists`, which a fresh daemon's unique name cannot meet;
- the durability gate, which is off, since these fleets declare no durability policy.

The lease gate does not apply to a create.

## Reproduced

The fixture hands a test its fleet from `start_mesh_with`, once `settle_initial_consensus` has seen every council and
root report the committed membership. A diagnostic test started this history's five-node, two-region fleet and, the
moment the fixture returned, created a volume through every node. On adacachy (AMD Ryzen 9 9950X3D2, Linux
7.2.9-2-cachyos), 2026-11-04, in a capped unit pinned to two CPUs with four busy loops inside it, 1 of the 200 creates
over 40 fleets was refused `Refused { refusal: ConsensusNotInitialized }` (run 37, node 3). The other 199 were
created. Log: `~/.cache/slates-agents-scratch/fixtures/diagcreate.log`.

## Cause

`settle_initial_consensus` checks the committed membership through observers that read the council and root
directly. It checks a node's root only where that node is a root voter (the lowest id of each region). What admits a
placement write is a different word, the shard's `consensus_ready`. The control shard sets it in
`sync_config_from_council`, once a coordinator period, from the root's and the council's initialization and the
node's membership. So the fixture can hand over a fleet in which a node's readiness has not caught up: one
coordinator period after its council committed, or longer on a root learner still importing the root. This
history's owner is the highest id of its region, a root learner. A create in that window is refused
`ConsensusNotInitialized`. The product behaves as designed (the refusal is typed and comes before any effect; the
CLI fleet test retries exactly it). The fixture's precondition was incomplete.

## Exact edits

- `crates/server/src/daemon.rs`: `Daemon::consensus_ready()`, an observer of the control shard's readiness.
- `crates/server/tests/fleet.rs`: `settle_initial_consensus` also waits, within its existing bound, until every node
  reports ready, and its failure report prints each node's readiness. New test:
  `a_settled_fleet_admits_a_create_on_every_node`, the diagnostic made permanent.
- The routing history's assertions, its candidate budget and its typed panic are unchanged.

## Verification

On adacachy, 2026-11-04. Every run was in a capped user unit pinned to the CPUs named, with that many busy loops
inside it. Before is this branch with the regression test and the old settle step; after is the fix. The binaries were
copied aside (`fleet-settle-before`, `fleet-settle-after`) and run by
`~/.cache/slates-agents-scratch/fixtures/fleetloop.sh <runs> <busy loops> <binary> <tag> <test>`.

| run | load | before | after |
|---|---|---|---|
| the create on every node right after the settle (the diagnostic, then the test) | two CPUs, four busy loops | 1 refused in 40 fleets, then 0 in 200 | 0 in 100 |
| the same | one CPU, two busy loops | 0 in 100 | — |
| the routing history | two CPUs, four busy loops | 0 in 50 | 0 in 50 |
| the routing history | one CPU, two busy loops | 0 Create failures in 50 (1 other failure, below) | — |
| the whole fleet target | four CPUs | 86 of 86, six times (four busy loops) | 87 of 87 once (none) |

The refusal is rare under load here: 1 in 340 fleets before the fix. So 0 in 100 after carries little weight on its
own. The fix holds by construction: the settle step now waits on the very word whose absence produced the refusal,
and the whole fleet target shows that no history's settle waits forever on it.

## Another failure of the same history, still open

On one CPU with two busy loops the routing history failed 1 of 50 runs another way (run 47, log
`~/.cache/slates-agents-scratch/fixtures/fleet-runs/c1-47.log`). The foreign node's lookup was refused
`OwnerUnreachable { region: 0 }`, and its counters were `fleet.forward.no_session` 4,
`fleet.forward.session_never_returned` 4, `fleet.owner_location.forward_unsent` 4, `fleet.owner_location.direct` 1
and `fleet.owner_location.route_dropped.refused` 1. The successor counted `fleet.owner_location.owned_unmaterialized`
1. The held session did not come back within the forward's wait. That is the condition the test holds out on
purpose; not seen at two CPUs in 100 runs. It is not attributed: it is recorded open in GAPS.

Attributed 2026-11-05: the lookup ran before the foreign node held the dead owner dead, so it forwarded to the
creator without a round. `docs/bugs/2026-11-05-a-lookup-before-the-owners-death-was-known-never-ran-its-round.md`.
