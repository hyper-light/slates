# The council seats its voters by id order, so a replacement takes a live voter's seat and the dead one keeps its own

Date: 2026-09-22. Contracts: §4.8 Membership and the configuration master (D-14), AC-8.1, AUD-07.
Design: Raft joint consensus (Ongaro's thesis §4); the council's death-confirmation window
(`docs/bugs/2026-09-17-council-retires-a-suspected-voter.md`).

## Symptom

The KIND lane of CI run 35615113353 (commit `78d19ea`, job 106383701981) formed the fleet, placed a
sealed volume at `f + 1`, killed the owner's pod and saw both survivors retire the dead owner 1.5 s
after the delete. Then no survivor served the volume for 120 s: `volume stat` answered `NotFound` on
both. The lane had passed on the eight runs before it (`2179cc2` and earlier), and `78d19ea` touches no
council or takeover code, so this is an intermittent failure rather than a regression in that commit.

## Reproduction

An isolated cluster, `slates-claude`, built from a `git archive` of `3b38d15` (image
`slates:claude3b38`, release profile, arm64) so nothing in the shared working tree reached the image.
Its six kind node containers were pinned to four shared CPUs (`docker update --cpuset-cpus 0-3`) to
match the CI runner's four vCPUs. Each iteration: `helm uninstall` and wait for the pods to go; `cargo
xtask kind install --cluster slates-claude --tag slates:claude3b38 --replicas 3`; `kubectl exec
slates-0 -- /slates bootstrap root`; `cargo xtask kind prove --cluster slates-claude`. Unpinned, the
proof passed. Pinned, iteration 2 of 8 failed with the CI failure's exact output (retirement in 1.4 s,
then 120 s of `NotFound`): one failure in eight.

## Root cause (from the committed council state)

After the failure, `/slates recovery-plan region --json` on each pod showed the same committed
regional configuration (version 3) with voters

```
[1810687258910326756 (slates-2), 9658073402137337477 (the replacement slates-1),
 11598703409819757640 (the dead slates-1)]
```

and slates-0 (`12168115181700612846`), the live leader at formation, was not a voter. In id order
slates-2 < replacement < dead predecessor < slates-0: the voters were exactly the three lowest ids.

`council_voters` chose the voters as the members with the lowest ids up to `2f + 1`, whatever their
state. The replacement pod authenticated under the same certificate with a fresh id and was admitted at
once, while its predecessor stayed a member until its death had held for the confirmation window. With
four members, the leader moved the voter set to the three lowest ids: the replacement took a seat, the
dead predecessor kept its seat, and the live leader lost its own seat and stepped down.

The new voters elected the replacement. A leader retires only members that its own SWIM view has held
dead for the window. The replacement never probes its predecessor, because the predecessor's
certificate is its own, so it holds no state for that id, never counts it dead and never retires it.
Its leadership held: slates-2's replies kept CheckQuorum satisfied. The dead id stayed in the committed
configuration, the placement view kept naming it as the volume's owner, no takeover was assigned, and
both survivors answered `NotFound`.

The id carries no meaning for voting. A member id is derived from a certificate and a per-boot nonce,
so a replacement's fresh id lands anywhere in the order. The rule was chosen on 2026-09-13 so that every
node could compute the voter set from the members alone, but every node already learns the voter set
from the Raft log's joint-consensus entries; only the leader needs to compute a target.

## Fix

`council_voters(sitting, members, alive, quorum)` (`crates/cluster/src/config_group.rs`):

- A sitting voter keeps its seat while it is still a member. A seat is freed only by a member leaving
  the configuration (a confirmed, stable death taken over, or a retirement), never by a suspicion.
- A free seat goes only to a member the leader holds authenticated-alive, never to a suspected or dead
  one.
- The id is only the tiebreak among equally eligible members. More sitting members than seats (a
  lowered floor) keeps the ones the leader holds alive first, then the lowest ids.

`RegionalCouncil::reconcile_voters(alive)` passes the voters in force and the leader's alive view, and
the server's council drive passes its authenticated-alive view. No Raft rule changed: every voter
change is still one joint change at a time, committed by majorities of both sets.

## Failing test first, and validation

- `config_group::tests::a_replacement_admitted_before_its_predecessor_retires_takes_no_live_voters_seat`
  uses ids in the KIND order. Before the fix, the admission alone began a voter change
  (`[true, false, false]` where `[false, false, false]` is required). After it, the leader keeps its seat
  and its leadership while the predecessor is unretired, and the replacement takes the predecessor's
  seat once the takeover commits.
- `config_group::tests::a_free_seat_waits_for_a_member_the_leader_holds_alive`: a suspected learner is
  not promoted to a freed seat; once it is alive it is.
- `cargo test -p slates-cluster`: 152 unit tests and every integration test pass; `cargo clippy -p
  slates-cluster -p slates-server --all-targets -- -D warnings` and rustfmt are clean. Both were run on
  a clean export of `9c82f19` plus this change, because the shared tree holds unrelated uncommitted
  work that does not pass clippy.
- `cargo test -p slates-server --test fleet`: 47 of 48 passed on the first run (321 s). The one failure,
  `a_learner_fetches_the_councils_committed_configuration_over_the_transport`, predicted its learners
  from the old rule; it now reads them from the committed voter set and passes 3 of 3.
  `settle_initial_consensus` likewise now checks the rule's invariants on the committed voter set
  (`min(2f + 1, members)` seats, every one a member, the bootstrapper among them) instead of predicting
  the set from ids.
- KIND, pinned: the fixed image (`slates:claudefix`, `3b38d15` plus this change) passed 6 of 6,
  including one history where the replacement's new id was the fleet's lowest and the leader kept its
  leadership. At the measured baseline of one failure in eight, 6 of 6 is weak evidence by itself
  ((7/8)^6 ≈ 0.45); the committed voter set above and the deterministic unit test are the root-cause
  evidence.

## Still open (sibling review)

1. **A replacement cannot see its own predecessor's death.** With the seat rule, a replacement becomes a
   voter only when a seat is free, so the observed history cannot recur from one failure. It can still
   happen after two overlapping faults: another voter falsely confirmed dead inside the predecessor's
   confirmation window frees a seat, the replacement takes it and wins an election, and nobody retires
   the predecessor. Closing it needs the certificate anchor recorded for each member in the
   configuration (two members of one anchor are then resolvable from committed state, the older
   superseded), or death evidence carried from followers to the leader. Owed.
2. **Root-group representatives follow id order too.** `root_representatives` picks the lowest-id alive
   host per region, so a lower-id replacement moves the root voter. That is churn, not a stuck group:
   the new representative is alive. The same incumbency rule would remove it. Owed.
3. **The KIND lane's failure report could not show this.** The takeover diagnostic asked `status <id>`,
   which answers `NotFound`, instead of the daemon's counters; neither the formation nor the takeover
   report printed the council's committed voters; and the view's `resolve_refused` matched only the
   unnamed `fleet.resolve` kind, so it read 0 whatever lookups failed. This diagnosis needed a live
   cluster. **Fixed in the follow-up change:** every failed fleet wait and the takeover wait now report
   each pod's node and IP, fleet and session counters, every refusal summed by kind, the committed
   council voters and the last 80 log lines (`Lane::fleet_diagnostics`), and `resolve_refused` sums
   every `fleet.resolve.*` kind (`kind::tests::a_views_resolve_refusals_sum_every_lookup_failure_kind`
   failed before: 1 counted of 6). Exercised on the local cluster by scaling to two pods: the formation
   wait failed at 180 s with every block present and slates-1 reporting 45 `fleet.resolve.refused`.
4. **The initial-formation failure of run 35615970514** (slates-0 and slates-2 each probing only
   slates-1 for 180 s) is a different failure and was not reproduced here: 14 local formations
   (8 unfixed, 6 fixed), all formed in 0.2 to 0.3 s.

## Edits

- `crates/cluster/src/config_group.rs`: `council_voters` takes the sitting voters and the alive view;
  `reconcile_voters(alive)`; module and function docs; the two tests above; test helpers pass the
  alive view.
- `crates/cluster/tests/config_group_live.rs`: passes the alive view.
- `crates/server/src/fleet.rs`: the council drive passes the leader's authenticated-alive view.
- `crates/server/tests/fleet.rs`: `settle_initial_consensus` and the learner test read the committed
  voter set rather than predicting it from ids.
- `docs/wip/SLATES_DESIGN.md` (§4.8 status, A-29), `docs/wip/GAPS.md` §0, `docs/wip/TBD_FIXES.md`.
