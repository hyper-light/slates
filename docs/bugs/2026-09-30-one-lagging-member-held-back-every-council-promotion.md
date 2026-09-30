# One lagging member held back every council promotion (2026-09-30)

Contracts: §4.8 (the council's voter set follows the committed membership, D-14), Raft §6 and thesis
§4.2.1 (a new server is caught up before it votes). Found from three macOS CI failures in one day, each
with one signature: a node's council held one voter while its configuration held three members.
- Job 109761848083: `an_unlisted_node_enrolls_through_one_seed_and_joins_the_existing_quorum`.
- Job 109765227530: `three_daemon_processes_deploy_a_fleet…`, whose mount was refused
  `LeaseUnconfirmed { version: 3 }` with `fleet_council_voters: 1`, `fleet_configuration_members: 3`.
- Run 36663502686: the formation test. Its leader kept voters = [itself] while its record links reached
  one follower.

## Description

A council formed by its first node, with its other members then admitted, never widened its voters when
the leader could not bring *one* of the new members up to date. The members it had caught up were not
promoted either. The council stayed at one voter, so the fleet ran on one copy of its configuration log and
leases that needed the widened configuration stayed unconfirmed.

## Root cause

`RegionalCouncil::reconcile_voters` staged the whole target voter set and began the joint change only when
`RaftNode::catch_up(&target)` reported every added member caught up (`CatchUp::Ready`).

When one member stalled, its staging was aborted after `STALLED_WINDOWS` and the next call staged every
member afresh. Caught-up members therefore waited on a member that never arrived: in these runs, one the
leader had no session to.

## Impact

- The council's durability and fault tolerance stayed at one node while a membership of three was
  committed.
- Operations needing the widened configuration (a lease confirmation at the new version) were refused,
  which in the deploy flow reached the user as a failed mount.

## Exact edits (`crates/cluster/src/config_group.rs`)

- **`reconcile_voters`, on a stalled staging (`CatchUp::Aborted`).** The joint change now begins to the
  sitting voters plus every staged member that did catch up (`promote_the_caught_up`), in the target's
  order. That is a valid change: Raft §6 asks only that changes go one at a time.
- **The stalled member is not abandoned.** It is staged afresh by the next change, and promoted once it
  can catch up.
- **The common case is unchanged.** When members catch up together, the voters still move once.

## Evidence

- **Red.** `config_group::tests::a_member_that_cannot_catch_up_does_not_hold_back_one_that_has`: a council
  whose voters are `[OWNER]` and whose members are `[OWNER, A, B]`, where the leader reaches A but never B,
  driven period by period as the fleet leader drives it (reconcile voters, CheckQuorum, replicate). Before
  the edit the voters stayed `[OWNER]` through twelve periods.
- **Green.** The voters become `[OWNER, A]`. Once B is reachable, the next change promotes it too:
  `[OWNER, A, B]`.
- **Suites.**
  - `slates-cluster` passes on macOS and Linux (lib 224).
  - The server fleet suite passes: macOS 59/59, Linux (io_uring) 60/60.
  - The CLI flows pass, including the three-process deploy (13/13).
- **Safety.** The change goes through the same joint path (`begin_membership_change`, then
  `complete_membership_change` once the joint entry commits), whose safety does not depend on which
  voters the new set names.

## Sibling

- **The root group had the same gate.** `RootGroup::reconcile_voters` also answered a stalled staging with
  no change, so one unreachable regional representative held back every other representative's promotion
  to the root voters.
- **One implementation now.** The promotion lives in the Raft core as `RaftNode::promote_caught_up`, and both
  the council and the root group call it on `CatchUp::Aborted`.
- **Red, then green.** `root_group::tests::a_representative_that_cannot_catch_up_does_not_hold_back_one_that_has`
  failed before the edit (the voters stayed `[P0]`) and passes after it (`[P0, P1]`, then `[P0, P1, P2]`
  once P2 is reachable).
- **Suites after the move.** `slates-cluster` passes on macOS and Linux (lib 225, one ignored), and the server fleet suite passes
  59/59 on macOS.

## Open

- Why the leader could not catch the third member up in those runs is still unobserved. The first
  occurrence showed its record links reaching one follower only.
- The fix removes the stall that lag caused; the lag's own cause needs the next occurrence's evidence.
  The CLI flow now prints every node's status and whether the council widens (`df026e4`).
