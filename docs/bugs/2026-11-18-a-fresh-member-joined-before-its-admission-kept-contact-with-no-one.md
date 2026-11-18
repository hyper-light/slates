# A fresh member that joined before its admission kept contact with no one (2026-11-18)

Contracts: §4.8 and D-14 (direct contact: a node keeps a record session and a probe of its own only with its holders and
the voters, amended 2026-10-10; a non-voting member learns the committed configuration by fetching it), AC-8.1 (a fresh
member joins the existing quorum). Amendment A-331.

## Description

live_chaos's fresh-member flow (`a_member_that_never_held_the_log_never_stands_in_for_its_holder`) kills the bootstrap
voter of five nodes at f = 1 and starts a fresh process in its place. In some runs the second formation wait never
ended: one node, the fresh member, stayed at `fleet_peers_in_contact` 0 and `fleet_peers_unmeshed` 0 while the other four
read (4, 0).

- 2026-11-09, one CPU of load (diag-60's harness): 2 of 48 runs on main 84d1f59a (diag-live-fixes-70) and 1 of 48 with
  learner-3 (diag-raft-chaos-ab-66), at the formation wait (`live_chaos.rs:410`).
- 2026-11-09, diag-raft-chaos-ab-68b (a status line per peer with its contact reason), sample 2, pair 1: the same stall at
  `live_chaos.rs:411`, with the fresh member's state printed every 5 s for 48 s.
- 2026-11-18 on main, both of the integrator's first gated runs: macOS at bfc34cace (`:418`, "one of five members (in
  contact, unmeshed) Some((0, 0)), the rest (4, 0)") and Linux, CI run 44269183565 on ubuntu-latest (`:418`, the same
  shape).

## Root cause

What the fresh member showed for the whole wait (diag-raft-chaos-ab-68b-2; the 2026-11-18 Linux run shows the same
counts):

- `fleet_members` named five members held alive: itself and the four others by their real ids. The four manifest seeds
  were folded dead, as they are once the real ids are learned, and the contact lines over them read `BelievedDead`.
- `fleet_peers_probed` 4, `fleet_peers_in_contact` 0, `fleet_peers_unmeshed` 0.
- `fleet_configuration_version` 0: its placement had installed nothing.
- `fleet_council_voter_ids` 735110715460604032: its council's voters were the member it had replaced, alone.
- `fleet_learner: council_version=8 fetched_version=8 asked=1 last_asked_ago_ms=47997`, and
  `fleet.learner.no_session` 113 in the 48 s. On Linux, `fleet.link.out_of_contact` 4: all four record links idled.
- The other four ended at version 12, with the fresh member admitted.

Read step by step against the code:

1. The fresh member's fetches ended at the start of the wait: the last one asked a single peer and left it at version 8.
   It joined with the bootstrapper alone as its Raft voters. A voter's join state names the three voters, and a
   learner's names the voters of its own join, which is the bootstrapper alone (a learner's Raft state never moves after
   its join), so a learner answered its join. Version 8 was committed in the first formation, before the fresh member
   existed, so it does not name it.
2. The fresh member joined both groups from that answer, so its council and root were initialized, and `Forming` (which
   keeps contact with everyone) no longer applied. Its placement installed nothing, since `configuration_for` of a host
   the configuration does not name is `None`.
3. `direct_contact` then judged each live peer by the member rule: in the council configuration (yes), a neighbour (no:
   no placement), a holder of this node (no: this node holds nothing under version 8), a voter it knows (no: it knows
   only the dead bootstrapper), staged (no), and this node a voter (no). That is `OutsideNeighbourhood` for all four,
   the only reason that drops a live peer.
4. Every record link idled and dropped its session. The learner fetched every period, since its membership diverged
   from its configuration, but found no session to fetch over, so it never learned version 12, which admits it. Nothing
   broke the cycle.

The member rule assumes this node is a member of the configuration it judges by. A node the configuration does not name
has no neighbourhood and holds nothing, so under that rule it keeps only the voters it knows, and a join can leave those
naming only the member it replaced.

## Impact

A fresh member, and a member retired while it was away, could stay out of the fleet for as long as it ran: no record
session, so no fetch, no report and no part in placement, while the others had admitted it and placed content on it.
The formation wait of every fleet test that replaces a node could time out on it.

## Exact edits

- `crates/server/src/fleet.rs`: `DirectContact::Unadmitted`. `direct_contact` asks `joining_contact` first, which keeps
  contact with every live peer while this node is not yet a member under its groups: `Forming` before both are
  initialized (as before), `Unadmitted` while the council configuration it holds does not name it. Once it holds one
  that names it, the member rule applies.
- `crates/server/src/consensus.rs`: the test
  `a_fresh_member_joined_before_its_admission_keeps_contact_with_every_live_member`, which joins a shard through the real
  fetch path (`serve_fetch` as a learner donor, `adopt_fetch` as the fresh member) from an answer whose configuration
  does not name it and whose voters are the dead member it replaced, then folds a later answer that admits it.
- `docs/wip/SLATES_DESIGN.md`: §4.8's contact status (A-331) and its learner paragraph, and the amendment log.
- `docs/wip/GAPS.md`: the entry for this stall, which had been recorded on 2026-11-09 on a branch that never reached
  main.

## Evidence

`cargo test --locked -p slates-server --lib consensus::tests` on the Mac Studio (kiln r1379, 2026-11-18, load 34 to
35 on 32 cores), on a clone of this change:

| Tree | Result |
|---|---|
| `fleet.rs` as on main before (69cd38b22) | failed: all four live peers `OutsideNeighbourhood`, the mesh counting 0 (the CI signature), with the join's preconditions holding (both groups joined at version 8, this node not named, one known voter) |
| this change | passed, 0.76 s |
| this change with only the `Unadmitted` arm taken out | failed at the same assertion |

The six contact tests already in the crate (`fleet`, `daemon`, `member_task`, `discovery`) pass with it. The
end-to-end count is the supervisor's to measure: live_chaos's fresh-member flow at one CPU, N of 48 runs that form,
before against after (before: 46 of 48 on 84d1f59a, 47 of 48 with learner-3).

## Siblings, reported and not changed

- A learner's known voters can go back. `serve_fetch` answers with the answerer's known voters, and on a learner those
  are whatever its own last answer named. An asker that adopts a newer view from such a learner takes its voters, even
  when it knew a fresher set. A-331 makes an unadmitted member independent of them; a member keeps its neighbourhood to
  fetch from. Not seen to stall anything since.
- `drive_root_learner_fetch` filters on the root group's voters (GAPS, 2026-11-08 learner entry), unchanged. With A-331
  an unadmitted node holds sessions to every live member, so its root learner falls back to the region's alive
  representatives once its root voters are unreachable (`root_learner_voters`).
- live_chaos's `diagnosis` leaves out the `fleet.learner.*` counts and the `fleet_learner` line, so this stall's own
  counters never printed on main. Told to g229, which owns that harness.
