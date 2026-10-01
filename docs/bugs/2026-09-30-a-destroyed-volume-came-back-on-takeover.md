# A destroyed volume came back on takeover, and its holders never let go of it

**Date:** 2026-09-30. **Area:** `slates-db` (partition, register), `slates-cluster` (content hold),
`slates-server` (fleet record plane, takeover, verbs). **Audit:** AUD-29-43 (the retirement half), found
while building it. **Design:** §4.4 destroy ("release quota, tombstone the id"), §4.8 mechanism 1, §4.10
("a late copy is redundant and reclaimed").

## Description

A destroy ended on the owner. The volume's candidate holders kept its head record, its catalog record
and its content for good. When the owner later died, the successor's takeover adopted the last live head
it found and materialized the destroyed volume again.

Red test, `crates/server/tests/fleet.rs`:
`a_destroyed_volume_is_retired_on_its_holders_and_no_takeover_brings_it_back`.

- Three daemons form an `f = 1` fleet.
- The owner seals three volumes. With two survivors, two of the three share a takeover successor
  (pigeonhole). One of that pair is destroyed; the other is kept to prove the successor's takeover ran.
- Before the fix (456 s, most of it the release deadline):
  `held_before=true destroyed=true released=false kept_served=true passed=true gone_served=[true, false]`.
  - The survivors never released the destroyed volume's content.
  - After the owner's death its successor served the destroyed volume again.

## Root cause

- **Nothing carried the destroy off the owner.** `Op::VolumeDestroyed` removed the record, although its own
  doc said "its record becomes a tombstone". The record plane ships heads only for volumes in the shard's
  tables, so a destroyed volume simply stopped shipping.
- **The holders had no rule for letting go.** `ContentHold::forget_manifest` was reachable only from a test
  fault. The takeover's stale-copy `reclaim` dropped a copy's records but not its content.
- **Each acceptor kept every position it ever accepted.** A volume's holders kept one head position per
  seal forever. Phase one reports only the highest, so every older head position was dead weight.

## Fix

- **A durable tombstone (`slates-db`).**
  - `Op::VolumeDestroyed` now leaves a `Tombstone { volume, sequence }` in the partition. The sequence is
    one past every sequence the volume's registers used: its head epoch and, for a green, its newest
    version (`Partition::tombstone_sequence`).
  - A tombstone keeps its volume's slot of the derived volume capacity until it is retired, so the table
    is bounded by the same cap as volumes.
  - `Op::TombstoneAdopted` (a successor) and `Op::TombstoneRetired` are appended. The snapshot carries
    the table.
- **Two register stages (`crates/server/src/tombstone.rs`, class byte 4).**
  1. The tombstone at its sequence. A holder that accepts it releases the volume's content.
  2. Once every candidate holds the tombstone, the retirement at the next sequence. A holder that accepts
     it drops the volume's and its catalog's records.

  The retirement must wait for every candidate. A holder that dropped its records while another still
  held only the live head could let a later takeover meet that live head alone.

  When every candidate holds the retirement, the owner records `TombstoneRetired` and its acceptor forgets
  the volume. A volume that is still `Destroying` already ships its tombstone at the same sequence, and
  places no new seals and heals nothing.
- **Takeover.** A successor that adopts either stage records `TombstoneAdopted` on the shard the id routes
  to. It never materializes the volume or keeps its catalog. Its record plane then finishes the stages.
- **Laptop (R8).** A laptop has no remote candidate, so both stages are complete at once and the tombstone
  retires with its destroy. It is the same rule, with nothing to ship (`verbs::retire_local_tombstones`).
- **Compaction.** A single-value register (a head, a catalog, a tombstone) keeps only positions at or
  above the one it just accepted, on holders and on the owner's acceptor (`Acceptor::compact_below`). A
  green's merge chain is a ledger and is kept whole, since a successor replays it.
- **Reclaim.** The takeover's stale-copy `reclaim` now releases the copy's content too
  (`ContentHold::forget_object`).

## Tests

- The red test above passes in 9.6 s.
- `a_laptop_destroy_retires_its_tombstone_with_the_destroy` (verbs): no tombstone is left after a laptop
  destroy, and the name is created again under a fresh id.
- `a_compacted_register_keeps_its_newest_position_and_a_forgotten_one_keeps_nothing` (register).
- `tombstone::tests`: both stages round-trip. Truncated, padded, non-canonical and foreign-class values
  are refused.

## Same change: what a holder keeps

The sweep for sibling instances found the general case of the same defect: a holder released nothing,
ever. It kept every seal's content, and a late hedged copy the head never lists, for good.

- **The rule.** A manifest is kept only while the object's newest accepted record names it for this
  holder, or while it is the object's newest placement ahead of every accepted record. It is enforced at
  the door, after every accepted record, and after every held put (`crates/server/src/content_retention.rs`).
- **Measured and rejected: a time window.** The first cut released ahead content after one coordinator
  period plus the round budget's longest deadline (1.2 s). The full suite then failed
  `a_submit_is_answered_only_once_its_record_commits_at_the_quorum` with `client Submit: deadline exceeded`.
  Run alone with a release log, it showed retention releasing a green's inputs (one manifest, window
  1,200,000,000 ns) before the in-order merge record that needed them arrived. Nothing in the protocol
  bounds that delay, so the window was replaced by events: a record at or past the content's sequence not
  naming it, a newer placement, the tombstone, or the stale-copy reclaim.
- **Closure.** A put shipping a chunk its manifest does not reference is refused `Unreferenced` before any
  chunk is decoded. The ownership oracle's census now counts that refusal, and its generator gained the
  protocol's offered put: after an offer, it ships exactly the chunks the object lacks.
- **Test.** `a_holder_releases_content_a_newer_head_supersedes` passes in 2.5 s. It was written with the
  fix: before it, `forget_manifest` was reachable only from a test fault, so nothing could release the
  first seal's content.

## Still open in AUD-29-43

- Holding replicated content in the shard's arena under an all-cost, typed byte admission.
- The audit's churn acceptance test.

## Sibling reported

The owner keeps each seal's whole archive on the heap (`ContentWork.archive`) until its content places.
Nothing charges it (§4.2 lists "archive construction" among the costs admission must reserve).
