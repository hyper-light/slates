# A fleet with no probe session counted itself meshed (2026-11-07)

Contracts: §4.8 (the direct probe mesh: every member a node keeps direct contact with has a formed probe session before
anything acts on the fleet), R8 (a laptop is the fleet of one, meshed at once), CLAUDE.md §4 (a check that passes on an
empty input certifies nothing; a non-vacuity counter beside it).

## Description

`Daemon::fleet_meshed` answered whether every member the node keeps direct contact with had a formed probe session. It
read those members from the council's configuration (`FleetNode::members`). From the bootstrap until the council admits
the others, that configuration names the bootstrapping node alone, so the answer was "meshed" over no peer at all, and
`form_and_settle` and `assert_fleet_forms` in `crates/server/tests/fleet.rs`, which bootstrap and then wait on it,
could pass on a fleet with no session. Measured: one node of a three-node manifest, its peers never started, bootstrapped
alone, answered `fleet_meshed` true with council members `[itself]`, two unmeshed peers and no session
(`a_node_whose_peers_never_answer_is_never_meshed`, red on the old predicate in 38 s). Before the bootstrap the
configuration still holds the manifest's members, and the old predicate answered false there. The status fields had
the same fault until 518a1e82 (`fleet_peers_in_contact` 0 with `fleet_peers_probed` 4 on a forming five-node fleet,
diag-live-fixes-32). The supervisor named the daemon's helper as the same predicate (2026-11-07).

## Root cause

Two readings of "the fleet's members" side by side: the council's committed configuration, which shrinks to the
bootstrapping node at the bootstrap and grows by admissions, and the members the probe plane holds alive, seeded from
the manifest at boot. The mesh's predicate took the first, so it was vacuous in exactly the window a formation wait
exists for. Separately, a fleet whose peers were all held dead had no peer in contact and also counted itself meshed,
the same as a laptop.

## Impact

A fleet test that waits on `form_and_settle` or `assert_fleet_forms` right after its bootstrap could go on with no
session formed, while the other members were still being admitted. Nothing held the wait to the sessions it names.

## Exact edits

- `crates/server/src/fleet.rs`: `ProbeMesh` and `probe_mesh`, one predicate over the members held alive: the peers in
  direct contact, those with no formed probe session, and whether the membership knows any other member. `meshed` is
  true when every in-contact peer has a session and, once another member is known, at least one peer is in contact; a
  laptop, which knows none, is meshed.
- `crates/server/src/daemon.rs`: `fleet_meshed` and `fleet_unmeshed` read `probe_mesh`.
- `crates/server/src/verbs.rs`: status's `fleet_peers_in_contact` and `fleet_peers_unmeshed` read `probe_mesh`.
- `crates/server/tests/fleet.rs`: `start_mesh_unbootstrapped` starts the first nodes of a manifest without a bootstrap;
  `a_node_whose_peers_never_answer_is_never_meshed` (one node of three, its peers never started) is red on the old
  predicate; `assert_fleet_forms` asserts each node of a fleet of several held a probe session when it passed.
- `crates/server/src/fleet.rs` tests: `a_mesh_over_no_peer_is_meshed_only_on_a_laptop`.

## Siblings swept

- `Daemon::council_members` reads the configuration by design (a fleet test observing a commit); its callers' negative
  predicates (`!council_members().contains(&dead)`) run after `settle_initial_consensus`, which requires the commit.
- `verbs.rs` `owned_by_another_host`'s "a node alone" check reads the configuration's members: from the bootstrap until the others are
  admitted a fleet node answers another member's volume `NotFound`, as a laptop would, not that the fleet is not ready. Reported, not
  changed (a verb's routing, not a wait).
- The CLI flows' waits (`live_fleet`, `live_winfsp`, `live_chaos`) compare `fleet_configuration_members` and the
  sessions with the fleet's size, not with an empty set.
