# A sealed head shipped without its successor's key entry, so the takeover could never open the volume

**Found:** 2026-10-08, classifying a full-suite failure of
`a_cross_region_client_finds_the_copyset_successor_instead_of_an_unrelated_live_peer`.
**Status: fixed.**

## Description

A head that names sealed content carries the volume's lineage key, wrapped once for each neighbour the owner shard has
delivered a pair key to (A-92 piece 4c, `seal_keys::head_sealing`). A successor unwraps its own entry, adopts the key,
and opens the envelope archive it holds (piece 3b).

The owner shipped a sealed head whatever its sealing covered. A head written before a candidate's pair arrived had no
entry for that candidate. If that candidate became the successor, it adopted the head, could not adopt the lineage
key, and refused the envelope every period (`fleet.seal.envelope_unopened`). The volume was never served again.

The design stated this window (§A-92 piece 4c, "a neighbour that acknowledged a head before its pair arrived holds that
sequence without its entry until the next seal; as a successor it then starts a new key"). That was written while
archives were plain. Since piece 3b they are envelopes, and a new key cannot open an envelope sealed under the old one.

## Evidence

- The suite failure: the successor's own client was refused `Overloaded` for the whole wait, with 301
  `fleet.materialize` refusals and no breakdown. The test's counter filter (`takeover_counters`) dropped the
  `fleet.seal.*` counters, and by elimination the only materialize refusal whose breakdown it hid was
  `fleet.seal.envelope_unopened`.
- The reproduction below, with the fix disabled: the survivors held a head naming the sealed manifest with
  `sealing: None`, and the successor counted `fleet.seal.envelope_unopened` 580 times and `fleet.materialize` 290 times.

## Root cause

Nothing tied a sealed head's shipment to its key coverage. Pair keys are delivered each record period, and a head
could ship in the period before (formation, a neighbour newly joined, or a delivery that missed a period).

## Fix

- `fleet::sealing_covers`: a head naming sealed content (the volume has a lineage key, `seal_keys::lineage_recorded`)
  ships only once it carries an entry for every remote candidate. The owner shard learns each neighbour's anchor
  each period (`ShardState::member_anchors`, sent by `deliver_pairs`), since key entries are named by anchor and
  candidates by member id.
- Every candidate at once, not each as it is covered: a head's bytes at one sequence are one register value, and
  shipping to some holders now and the rest later with more entries would put two values at one position. The wait is
  bounded: pairs are delivered once per neighbour and kept, so only a member new to the neighbourhood, or one
  unreachable until it is retired, holds a new sealed head back.
- Only candidates still in the neighbourhood are counted. A candidate the council has retired stays in the
  placement while the owner's neighbourhood change is in flight, but it can never take the volume over. The first
  version of the fix counted it, and held the head back from the new cohort for good: the neighbourhood change could
  never settle. The full suite found that (`an_owner_settles_its_neighbourhood_only_once_its_head_is_placed_on_the_new_cohort`,
  3,999 traced refusals naming the retired node's missing anchor).
- The mirror shipment (`mirror::owed`) applies the same rule over its mirror holders still in the mirror
  neighbourhood.
- `takeover_counters` now includes `fleet.seal.*`, so a refusal of this kind is named in a failure message.

## Tests

`a_sealed_head_waits_for_every_candidates_key_entry_so_any_successor_can_open_it` (`crates/server/tests/fleet.rs`):
- the owner withholds pair delivery and forgets the pairs it delivered (`Daemon::inject_pair_withhold`, test
  support), seals a volume, and keeps withholding until its content has placed (`Daemon::fleet_sealed_manifest`) and
  for four record periods more;
- then the withhold is lifted, the survivors hold the head, the owner dies, and the successor must serve the volume.

Before the fix the survivors held the head with 0 key entries and the successor refused the envelope for good. After
it, the head waits during the withhold, then carries both survivors' entries, and the successor serves (3 of 3 runs).

## Impact

Any sealed volume whose head shipped before a candidate's pair arrived, and whose owner then died with that candidate
as successor, was lost to clients until a new seal, which a dead owner never makes. This is condition 10 (replication)
and condition 11 (recovery) failing silently: the volume's content was held, but could not be opened.

## Siblings, reported

- After a head ships, the owner shard's delivered pairs can still grow (a member joining), so a re-ship to a straggler
  at the same sequence can carry more entries than the first holders hold. Promotion picks a head by (sequence,
  epoch), so two holders can present different values at one position. The gate narrows this window but does not
  close it; fixing it means freezing a head's value at its first shipment.
- The successor answered its own client `HomedElsewhere` in the window before its adoption (127 times in one run of the
  reproduction, which then waited for adoption as the other successor tests do). `assert_successor_serves` asserts
  none, and holds only because the tests wait for adoption first.
