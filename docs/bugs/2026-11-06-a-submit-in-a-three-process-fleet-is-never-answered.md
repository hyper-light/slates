# A submit in a three-process fleet was never answered

Found 2026-11-06 by goal 4's Linux rungs on adacachy, using loadtest-scale's `crates/cli/tests/load_scale.rs` at
`712dd67b` (one primary and two replica server processes at `f = 1`, plus a client node, over `kernel/` of Linux
v7.4-rc1). §4.16 "Commit" (AUD-11): a submit at `f > 0` commits its effects at once, and is answered only when its
version's merge record commits at the quorum (`merge_service::resolve_accepted`). In a fleet deployed from a
manifest and bootstrapped, that commit never happened for a green made on the bootstrapper, so no submit to it was
ever answered. That held at 1, 30, 300 and 1,000 clients, and even for a CLI `submit` on the owner itself, which
had not returned after 600 s.

## Failing tests

- `a_submit_made_while_the_council_forms_is_answered_once_its_record_commits` (`crates/server/tests/fleet.rs`):
  three nodes at `f = 1`, bootstrapped without waiting for the initial membership (`start_mesh_unsettled`). On the
  bootstrapper it makes a green, a work with one edit, and a submit bounded by the fixture's formation wait. It was
  never answered in 8 of 8 runs (30 s each) before the fix, and passes 5 of 5 after (1.16 to 1.30 s).
- `a_submit_in_a_three_process_fleet_is_answered_once_its_record_commits` (`crates/cli/tests/cli.rs`, gated on
  `SLATES_TEST_CLI`): three daemon processes from one manifest, bootstrapped as the deployment flow is, then the
  same submit with a 40 s deadline. It was unanswered in 5 of 5 runs before the fix, and answered in 3 of 3 after
  (2.28 to 3.01 s).
- `a_joint_placement_with_a_cohort_smaller_than_the_quorum_needs_every_member_of_it` (`crates/db/src/register.rs`):
  the placement rule alone. On the old rule, `[[a], [a, b, c]]` at `f = 1` was not placed even with every candidate
  acknowledging.

In-process fixtures settle the initial membership before they act (`settle_initial_consensus`), so no green existed
while the configuration formed. Every in-process shape tried passed before the fix: two or three nodes, local or
forwarded, one, two or seven shards, a scratch or a based green.

## Cause, from the daemons' own lines

Temporary prints at the merge record's enqueue, at the control shard's configuration install, at each inputs put
and at each record ship, in the in-process and the CLI tests, showed:

1. The bootstrap (`consensus::bootstrap`) gives the bootstrapping node a one-voter council and installs that node's
   placement at once, with the owner as its only candidate. A green made there before the council grows enqueues
   its version 0 under that placement: `config v0 neighbourhood [owner] candidates [owner]`, 3 of 3 runs. A submit
   made shortly after enqueues version 1 the same way, or under the next configuration.
2. **Pending merge records kept that placement for good.** `PendingGreen` built each holder's owed positions from
   the candidates at enqueue and never re-derived them. Head records, by contrast, are placed by the current
   configuration each period (`fleet::owed_record`). With the owner as the only candidate at `f = 1`, version 0
   could never commit, and a holder cannot skip its prefix, so no version after it could either.
3. Once the pending records follow the current configuration (fixed here), they hit **the joint placement**. The
   council's growth moves the owner's neighbourhood from itself alone to the region, and during that change a
   record must commit at `f + 1` of each cohort (`Placement::joint`). The settled cohort, as the bootstrap left it,
   is the owner alone, and a cohort of one can never give `f + 1 = 2` acknowledgements. Version 0's ship returned
   `NotPlaced` with `acked: [owner, b, c]` and `joint: [[owner], [owner, b, c]]`, and version 1's inputs,
   acknowledged by all three, never counted as placed. The neighbourhood change can settle only once the owner's
   greens are placed on the new cohort (`greens_placed_on_current_cohort`), so this was a deadlock.

## Exact edits

- `crates/server/src/merge_service.rs`: `PendingGreen::retarget` re-derives every pending position's record and
  inputs placement from the owner's current placement of the green, keeping the acknowledgements gathered and
  rebuilding what each holder is owed. `next_merge_work` calls it each period before selecting. This is the
  re-shipping §4.8 already does for heads.
- `crates/db/src/register.rs`: `Quorum::committed_within(acked, cohort)` holds when acked ≥ min(`f + 1`, cohort), and
  at least one. `Placement::placed_with` uses it for each cohort of a joint placement. A cohort of `f + 1` or more is
  judged exactly as before. A cohort smaller than the quorum needs every one of its members, which is the most a
  takeover recovering through it could meet. `Quorum::recovery` (one promise for such a cohort) was already
  consistent with this, and its doc now says why.
- The three tests above. `start_mesh_with` is split so that `start_mesh_unsettled` stops after the bootstrap. The CLI
  test's failure prints every node's merge, content and seal counters.
- §4.8 "Neighbourhood changes" status and A-234. GAPS.

## Checks

- **Safety of the cap.** Every reader of a placement goes through `placed_with`: `region_placed` (and so
  `live_ship::head_placed`), `placed_on_current`, the merge plane and the heads. Takeover phase one counts
  `recovery(cohort)` = max(cohort − f, 1) promises; for a sub-quorum cohort every member holds a commit, so one
  promise meets it. raft-chaos reviewed this and agreed, on the condition that no reader accepts fewer than every
  member of such a cohort; none does.
- **Wider suites on adacachy, 2026-11-06, load 38 to 50.** `slates-db`, `slates-server` and `slates-cluster`, all
  targets: 61 passed, including the whole fleet target and the new tests. Two failed in the concurrent run, and
  neither is this change's:
  - `liveness::a_wedged_shard_stops_the_beat`: alone, interleaved with main `d61c3be2`, it failed 2 of 5 on main
    and 2 of 5 on this change, in the same rounds.
  - `shards::hostile_descriptor_asks_are_refused…`: alone, it passed 5 of 5 on both.
  The CLI fleet tests passed 3 of 3. fmt, workspace clippy, `xtask check` and docs pass.

## Also found on the way

loadtest-scale's harness (`load_bench.rs` `Agent::refused`) gives up on `Overloaded` and `OwnerUnreachable`, which
the protocol calls retryable with the same request id. With the commit stuck, that turned unanswered submits into
false oracle failures (787 and 720 at 1,000 clients, 766 at 300). That is loadtest-scale's to fix, and it has been
told.
