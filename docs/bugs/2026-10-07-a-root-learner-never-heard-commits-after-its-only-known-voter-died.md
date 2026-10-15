# A root learner never heard the root's commits after its only known voter died

**Found:** 2026-10-07, building the mirror region's promotion adoption (`docs/wip/mirroring.md` M4).

## Description

Three regions: region 0 (a, b, c), region 1 (x, y, z) and region 2 (w). The root group has one voter per region,
its region's representative. With region 0 lost, the root leader (w) committed `PromoteRegion { lost: 0, mirror: 1 }`.
The root voters, w and y, re-homed region 0's volumes to region 1. The root learners in region 1, x and z, never did:
their root configuration stayed at version 2, before the promotion, for the whole run (over 2,000 promote asks,
about 500 s).

## Root cause

A trace of the learner's fetch showed it. Each learner wanted to fetch every period (its alive regions, {1, 2},
differed from its root's regions, {0, 1, 2}), but asked only the voters it knew. The only voter it knew was host
`8991…`, region 0's former representative, now dead. 4,269 fetches found no session to it and went no further. The
leader had moved the voter set to each live region's representative, but a learner learns the voter set only from
a fetch, and the only voter it would fetch from was gone.

The earlier promotion test (`an_operator_promotes_a_lost_regions_mirror_over_the_transport`) runs three single-node
regions, every node a root voter, so no learner was ever exercised.

## Impact

Any root learner whose known voters all die — every member of a lost region's mirror except its representative —
stops following the root: promotions, region admissions and moved homes. After a region loss, such a node kept
routing the lost region's volumes to the lost region.

## Fix

`fleet::root_learner_targets`: a learner asks the voters it knows. With no session to any of them, it asks each
live region's representative (`alive_representatives`), the set the root leader moves its voters to. A learner that
knows no voter yet asks every session it holds, as before.

A first cut asked the representatives even when the known-voter list was empty, during formation. The
representatives need not include the root's voter there (it isn't necessarily its region's lowest id), so formation
failed 2 of 3 runs ("the root admits the new region before its regional bootstrap"). With the change switched off,
formation passed 2 of 2. The empty list now keeps the original behaviour.

## Test

`slates-server` `tests/fleet.rs`
`a_volume_awaited_in_the_mirror_is_served_there_after_its_home_region_is_lost_and_promoted`:

- with the fix switched off, x and z never left version 2 (traced), and the test failed at its adoption check;
- with it, the test passed 4 of 4 in 5.4–6.4 s.
