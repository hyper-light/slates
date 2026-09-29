# A formation cohort lost a record its survivor held

Date: 2026-09-29. Scope: the takeover's phase-one quorum (`ready_objects`, `crates/server/src/takeover.rs`), the
owner lease's confirmation count (`confirmations_needed`, `crates/server/src/lease.rs`), and the readiness an
owner reports a neighbourhood change on (`Configuration::placed_on_current`, `crates/db/src/register.rs`).
§4.8 "Promotion and takeover", "Neighbourhood changes", "Leases and reads". Found by a Linux io_uring loop of
the three-process CLI test after `265c1eb`.

## Symptom

The loop ran `three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death` 150 times
in `rust:1.98.0` with `--security-opt seccomp=unconfined` (io_uring, as GitHub runs it), with the status
diagnostics then uncommitted (`fleet_retirement`, `fleet_settled_generation`, the typed join refusals). Run 118
failed: after the owner A was killed, neither survivor ever served the volume. Both answered `volume stat` with
`NotFound` until the 60 s wait ran out.

## Evidence

Status of both survivors at the deadline (`scratchpad/loop2/fail-118.log`):

```
B: fleet_members: 5856216932550062358 17906511811766453129   fleet_held_records: 1
   fleet_takeovers_pending: 0   fleet_configuration_version: 7   fleet_settled_generation: 4
C: (the same members, held records, version and generations)   fleet_council_leads: true
   shard 0 refused fleet.takeover.lost: 2
```

Neither printed a `fleet_retirement` line: A's retirement was no longer kept. C was the node the test
bootstrapped (the lowest member id); A, the owner, was the other node the test does not bootstrap.

## Root cause

**How the owner came to be settled beside one host.** `bootstrap` forms a region with the bootstrapping node
alone, and the leader admits the others one at a time (`crate::consensus::bootstrap`). The first node admitted
is placed beside the bootstrap only, and that two-host neighbourhood is its settled one until it reports its
later neighbourhood placed. A owned nothing when B was admitted, so it would have settled within a period. It
created the volume first, while still unsettled. Its joint write placed at both hosts of `{A, C}` and at two
of `{A, B, C}`, and A was killed before its `Settle` reached the council. Configuration versions: 0 formed,
1 and 2 the admissions, 3 one settlement, 4 A's retirement, 5 and 6 the survivors' settlements, 7 C's
confirmation.

**The loss.** A's retirement recorded `{A, C}` as its settled neighbourhood, with C its only survivor. C's
round learned the head from its own copy, and `ready_objects` then asked for `f + 1 = 2` live members of the
cohort `{A, C}`. It found one, so it counted the object lost. C confirmed its share, the retirement was
pruned, and nothing served the volume again. B and C both still held its record.

`f + 1` is the phase-one quorum of a `2f + 1` cohort, not of every cohort. A commit is `f + 1`
acknowledgements of its cohort. Phase one must meet every such set, so `n − f` promises suffice in a cohort
of `n` hosts: `q1 + q2 > n` (Howard, Malkhi, Spiegelman, "Flexible Paxos", OPODIS 2016). In `{A, C}` at
`f = 1` a commit needs both hosts, so C's one promise meets it.

**The lease had to move with it.** The owner lease needs enough fresh confirmations among an object's other
candidates to meet every promotion quorum. With `f + 1` promises that was `others − f`: none for an owner
beside one host. With the corrected quorum, that host's one promise promotes. An owner holding a lease on
zero confirmations could then keep serving while its only co-holder promoted a successor. The bound is
`min(f, others)`. The exhaustive oracle written for this fix failed on exactly that case before the lease
changed: "f = 1, 1 others: 0 confirmations meet every promotion of 1".

**A sibling found while reading: readiness judged on the joint shape.** An owner reports its change placed
once every object it owns is held by `f + 1` of the new cohort. The code asked for `f + 1` of each cohort of the
joint shape, the old one included. Suppose an owner's old cohort loses a host while its change is in
flight, for example the bootstrap dying while the second member is still settled beside it. That cohort can
never again gather `f + 1`. The owner would then stay unsettled for good, and every write it made would stay
unplaced. The design's text asks only the new candidates. The old cohort holds every joint commit anyway.

## Impact

- Every region `bootstrap` forms has, until each owner reports its first full neighbourhood placed, owners
  whose settled cohort has fewer than `2f + 1` hosts.
- When such an owner died before settling, a takeover declared its objects lost while a survivor held them:
  1 run in 118 of this loop.
- The readiness deadlock needed a second failure inside the same window. No test or run observed it.
- No stale read was served. The loss refused what it should have recovered.

## Fix

- `Quorum::recovery(cohort)` = `max(cohort − f, 1)`. It is `f + 1` at the floor. A cohort of `f` or fewer
  hosts committed nothing, but a round adopts only what a promise reports, so it needs one.
  `ready_objects` asks each recovery cohort for it. An object is lost only when a cohort has fewer live
  members than that quorum, that is, when more of the cohort failed than it tolerates.
- `confirmations_needed(others, quorum)` = `min(f, others)`: `f` at the floor as before, and every other
  candidate when there are fewer. A lone owner still needs none.
- `Configuration::placed_on_current(object, acked)` judges `f + 1` of the object's cohort in the current
  neighbourhood. Both readiness checks use it: volume heads in `fleet.rs` and greens in `merge_service.rs`.

## Tests

- `a_takeover_recovers_an_owner_settled_beside_one_host_from_that_host` (server lib). The region forms the way
  `bootstrap` forms it, and the second member retires while settled beside the bootstrap. Before the fix:
  `fleet.takeover.lost` 1, nothing ready. After: the survivor's own promise readies the head.
- `a_recovery_quorum_meets_every_commit_its_cohort_could_make_and_no_smaller_one_does` (db). Exhaustive over
  `f ≤ 3` and every cohort size up to `2f + 1`. Its minimality arm fails the old `f + 1` rule, since in a
  two-host cohort one promise already meets every commit.
- `every_promotion_quorum_meets_the_lease_confirmations_and_no_fewer_suffice` (server lease). Exhaustive
  intersection of the lease's confirmers with every promotion quorum, with minimality. Red under
  `others − f`.
- `below_the_candidate_floor_the_owner_needs_every_other_candidate_up_to_f` replaces
  `below_the_candidate_floor_the_intersection_needs_fewer_confirmations`, whose premise was the old promotion
  rule ("one other candidate cannot give the f + 1 = 2 promises a promotion needs").
- `an_owner_whose_old_cohort_lost_a_host_is_ready_once_its_current_cohort_holds_its_head` (db). Red under the
  joint readiness.

## Siblings (reported, not changed here)

- `slates_db::ledger::Ledger::take_over` refuses below `f + 1` reachable candidates. `Promotion::promoted`
  counts `f + 1`. Both belong to the modelled register and its oracle and are exercised only there.
- `slates_cluster::promote_ledger_record` waits for `f + 1` promises. The daemon no longer calls it: greens are
  taken over through the per-host round. It survives only with its own test, a second path to remove.
- Separate, not fixed here: CI run 36603261909's two failures are both owner-lease refusals.
  - The NFS takeover test's `LOOKUP` answered `NFS3ERR_JUKEBOX`. The mount's only source of that status is
    the lease gate.
  - The CLI mount was refused `LeaseUnconfirmed { version: 3 }`.
  - The lease and every record are keyed to the regional configuration's single version, which every
    `Settle` and `Confirm` now advances. That churn is the hypothesis for both refusals. It is unproven until
    the refusal's reason (superseded, or short of confirmations) is counted.
