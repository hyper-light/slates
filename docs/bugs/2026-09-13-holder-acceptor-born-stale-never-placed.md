# A holder's acceptor born by a refused first record is pinned at a stale generation, and a head provisioned right after a retirement never places

Date: 2026-09-13
Area: `crates/server/src/fleet.rs` (`accept_held_record`, `reconcile_held_authority`).
Severity: fleet liveness — a volume provisioned in the window between the owner's install of a newly
committed configuration and a holder's (one heartbeat period, ~190 ms measured) never region-places:
the holder refuses every re-ship of its head `ForeignGeneration` for good. Found while landing the Raft
voter-set change (`docs/bugs/2026-09-13-raft-voter-set-never-shrinks.md`), whose extra council rounds
shifted `three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node` into that window; the same
test failed the same way, on the same day, under an unrelated probe-cadence change, which was then
measured-and-rejected for it — that rejection attributed this bug to the wrong cause.

## Description

`three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node` retires C, then provisions a volume on A
and waits for its head to place over B. With the voter-set change in the tree the test ran its whole
4000-period budget (495.23 s, twice) at the placement wait; without the change, 10.19 s. Tracing the ship
round and the holder's accept:

```
TRACE-INSTALL ms=924862 node=A fleet_version=0 -> council_version=1
TRACE-ACCEPT  ms=924862 node=B from=A object=O record_gen=1 installed=0 council=0 result=ForeignGeneration{current:0}
TRACE-INSTALL ms=925050 node=B fleet_version=0 -> council_version=1
TRACE-ACCEPT  ms=955378 node=B from=A object=O record_gen=1 installed=1 council=1 result=ForeignGeneration{current:0}
… identical every period for 30 s; A's ship: NotPlaced { acked: [A] } every period
```

## Root cause

`accept_held_record` created the object's acceptor on its **first** record under
`Authority { generation: <this holder's installed version>, owner: peer }` — via `entry().or_insert_with` —
and only then ran `accept`, which refused the record because it named a newer generation (the owner had
installed the committed configuration a heartbeat before the holder). The acceptor stayed, pinned at
generation 0. A refused record never calls `track_object`, so the routing view never learned the object,
and `reconcile_held_authority` — which raises the authority of every **tracked** held object at install
time — skipped it when the holder installed generation 1 a moment later. Every later re-ship (generation 1)
met an acceptor at generation 0: `ForeignGeneration` forever. The owner's own hold had been fixed to follow
the version earlier (the test's comment records that); the holder's had this second hole.

## Fix

`crates/server/src/fleet.rs` `accept_held_record`: an acceptor is created only by a record it **accepts**.
A refused first record leaves nothing behind, so the owner's next re-ship after the holder's install creates
the acceptor at the current generation and is accepted, tracked, and placed. Minimal: one function; the
existing acceptor path and every refusal type are unchanged.

## Validation (2026-09-13, worktree `agent/raft-membership`)

- Before: `three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node` FAILED at the placement
  assertion after the 4000-period budget, twice (495.23 s, 495 s), with the trace above.
- After: passes in 10.88 s (`cargo test -p slates-server --test fleet <name> -- --exact`).
- Further single runs on the holder path after the fix: `a_provisioned_head_replicates_across_the_fleet`
  3.89 s, `five_daemons_take_over_a_dead_owners_head_over_a_multi_holder_quorum` passed (load 7.6 during
  five concurrent agent builds).

## Siblings

- `reconcile_held_authority` raises only tracked objects' acceptors. With acceptors now created only on
  acceptance, every acceptor is tracked, so the two are consistent; a future path that creates an acceptor
  without tracking would reopen this hole — the invariant is "an acceptor exists only for a tracked object".
- The probe-cadence dilation recorded as measured-and-rejected on 2026-09-13 (`docs/wip/GAPS.md` §4.8 row)
  was rejected for exactly this failure signature; with this fix in place that rejection should be
  re-measured before it stands.
