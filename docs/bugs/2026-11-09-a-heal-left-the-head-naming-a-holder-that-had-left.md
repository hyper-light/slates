# A heal left the head naming a holder that had left the fleet (2026-11-09)

Contracts: §4.10 (the healer: content a recorded holder lost is placed again, and the head names the holders that
hold it), §4.8 "Promotion and takeover" (a successor fetches the content from the holders the head names), banned
items 8 (every retry is bounded) and 9 (no swallowed error).

## Description

A sealed volume's shipped head names its content holders, and a successor fetches from those holders once the owner
dies. When a holder the head named left the fleet, the healer re-placed the content, but the head kept naming the
member that had left, because a holder never takes a new value at a sequence it already holds (`ConflictingPosition`)
and nothing advanced the head. Measured on the old code, on a five-node `f = 1` fleet, with one local run per test on
a Mac at load 30 to 72: after the named holder was stopped and retired, both survivors that held the head still named
`{owner, departed}` 300 coordinator periods later, and nothing was republished. Tests:
`a_heal_republishes_the_head_once_a_named_holder_has_left` (453 s) and
`a_heal_republishes_a_live_volumes_head_once_a_named_holder_has_left` (456 s).

## Root cause

Three faults, each found by a measured run.

1. **No republish.** The healer re-recorded the snapshot's placement, but the head kept naming the departed member,
   since a head is never rewritten at its sequence.
2. **The door refused the replacement put.** The first fix placed on the current cohort and republished afterwards.
   Its put carried the head's own sequence. A cohort member that holds the head at that sequence, and is not named
   there, refuses that put: `content_retention::admits` keeps a holder's copy only when its newest record names it,
   or when the put is ahead of every record it holds. The owner counted `fleet.heal.named_gone` 1 and
   `fleet.heal.republished` 0, and the heads still named the departed member.
3. **No bound on a heal.** A replace that could not place stayed open for good. The healer skips a volume while one of
   its seal jobs is open, so a single refusal turned into a volume the healer never healed again.

## Impact

Once a holder the head named had left, the owner's death left the volume's successor naming a holder that is gone,
with one copy fewer than `f + 1` named. A single further loss lost the volume.

## Exact edits

- `crates/server/src/fleet.rs`
  - `heal_one_placed_snapshot`: reads the holders the snapshot's placement records. When every one is still a member
    it re-offers as before (`Heal::Reoffer`). When one has left, or none is remote, it replaces them on the current
    cohort (`Heal::Replace`, counted `fleet.heal.named_gone`).
  - `start_seal`: a replace puts its content at the next sequence (`SealJob::put_sequence`). That put is ahead of
    every holder's records, so the door admits it and retention keeps it as the newest placement ahead.
  - `content_work`: every heal is bounded by the membership horizon from its first round, judged only between rounds.
    A heal past it ends, counted `fleet.heal.unfinished`. The job is dropped, so a later healer step heals the volume
    again. A replace whose head is already at the largest sequence is never started, and is counted
    `fleet.heal.epoch_exhausted`.
  - `record_placed_seals` then `republish_head`: once the placement is recorded, the head is advanced to exactly the put
    sequence with the same snapshot (`Op::VolumeHeadAdvanced`), counted `fleet.heal.republished`. A refused advance is
    counted `fleet.heal.republish_refused`, which had been silently dropped in the first cut of this fix. The placement then
    names the new holders while the shipped head still names the old ones, until the volume's next seal.
- `crates/server/src/head.rs`: `SealJob::republish` and `SealJob::put_sequence`.
- `docs/wip/SLATES_DESIGN.md`: §4.10's healer sentences and amendment A-276; GAPS.
- `crates/server/tests/fleet.rs`:
  - The two republish tests (plain and live-shipped), each red on the old code.
  - `a_heal_that_cannot_place_ends_at_its_bound_and_the_volume_is_healed_later`: every survivor refuses content puts
    through the merge fault, then the refusal lifts. It is red on the old code (78 s) and red with only the bound
    removed (36 s: `fleet.heal.unfinished` 0).
  - `sealed_with_a_named_remote` waits only for what a five-node test needs: the snapshot placed, the head held by a
    named remote, and one other member able to take over. Its first form reused a wait for every member to hold the
    head, which a five-node fleet never reaches; that is why diag-live-fixes-66 failed in its setup.

## Retracted

My first account (2026-11-08) said a heal offered the first-ranked candidate first and never refilled the named holder,
on a three-node fleet. That was read from the code and never measured, and it is wrong. On three nodes every member
holds the head, so the first-ranked candidate refuses the put at the door, the round hedges, and the named holder is
refilled. `a_heal_leaves_the_head_naming_holders_that_hold_the_content` passed 3 of 3 on the old code (diag-66,
Linux). So the cause of the original load flake in `a_holder_that_lost_placed_content_is_repaired_by_the_healer` is open
again.

## Siblings swept

- A named holder that loses its copy but stays a member, on a fleet past `2f + 1`. Members outside the head's cohort
  hold no head, so they admit a heal's put as ahead content. I suspected a heal could then reach quorum there and never
  refill the named holder. Measured, it does not: `a_heal_restores_a_named_holder_on_a_fleet_past_its_cohort` (two
  losses, five nodes) passed on the old code (29 s) as on the fix (64 s), on a Mac at load 57 to 90. Neither that test
  nor a restore-the-named-holders change was kept, since nothing failed for them to fix.
- The mirror: a mirror holder that loses its copy is never repaired (GAPS, open, AC-8.15). That is its own change.
- A takeover's re-commit and a neighbourhood change both re-place content under a new holder set. From reading, not
  from a run: a takeover advances the head to a new sequence as it adopts (`Op::VolumeHeadAdvanced`), so the door
  would not refuse its puts. I have not traced whether a neighbourhood change's placement carries a new sequence.
  Both are reported, not changed, and owed a test of the same shape.
