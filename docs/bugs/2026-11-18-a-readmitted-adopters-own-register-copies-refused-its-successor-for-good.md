# A re-admitted adopter's own register copies refused its successor for good (2026-11-18, A-382)

## Description

The chaos nemesis (`a_seeded_nemesis_of_single_faults_keeps_every_register_invariant` in
`crates/cli/tests/live_chaos.rs`), replayed on main 93c3af8d0 (CI diag 44345243646, the integrator's
diag-integrator-nemesis-3), failed 6 of 6 runs. On arm64 the two seeds 6876545201569229162 and 7236833171758868842
failed at the convergence wait (`wait_caught_up`, `live_chaos.rs:5680`) with every daemon alive: the creator had taken
the volume back from the adopter that took it over first, shipped it at fence 3, and the third node held its head; the
adopter's copy stayed at fence 2 (heads 59 and 73) for a whole fleet wait. (On x86_64 the same seeds failed at the same
wait for another reason: two daemons were ended by their watchdogs, a shard spinning 1000 ms of CPU in one poll, so their
copies read none. That is the live takeover's materialization in one turn, g251's item, not this.)

## What the logs show

The status dumps of the four failing runs (arm64 and x86_64, both seeds) agree:

- The adopter b was retired while away and admitted again (`fleet_host_epoch` 2). It still shows its own log of the
  volume at fence 2 (`live shipping ... fence=2`, never fenced: it had nothing to ship, so no holder told it) and its old
  copy as a candidate at fence 2 (`live held ... epoch=1 fence=2`).
- b counted `fleet.record.refused.foreign_generation` 660 to 796 times on its control shard, and no live refusal at all
  (one `hold_refused.not_a_member` from an earlier fault). Its holds (`fleet.live.held` 60, 73) are all from before.
- The new owner counted `fleet.ship.catalog.short` 400 and `fleet.ship.head.short` 623 (its catalog and head never
  placed on b), and its live lanes to b `unanswered`/`no_session` about once a period.

Reproduced in process first (`every_copy_catches_up_once_the_creator_takes_its_volume_back_from_the_adopter`,
`crates/server/tests/fleet.rs`, on main's code: CI diag 44354746021, both Linux arches, 50 s): the creator ships at
fence 3 (head 9, durable 9) to the third node and b; the third node holds fence 3, head 9; b ships its own log at fence
2 (head 7) and holds its old copy at fence 2 (head 5), and counts no live refusal: the creator never sent it anything.

## Root cause

- When b took the volume over, the register copies it held of the volume's objects as a candidate (`holder_records`, the
  catalog register and the head register) moved to b's own authority at the generation of its takeover
  (`fleet::reconcile_held_authority` follows the routing view, which named b).
- b's re-admission forgot b's own routes (`FleetNode::install_configuration`: `routing.forget_owned_by(self.host)`), and
  the install moves a held copy's authority only to the owner its route names. With no route, the copies kept naming b
  at the old generation, for good.
- So every catalog and head record the volume's next owner shipped to b was refused: its generation was newer than the
  copy's (`Acceptor::check`, `ForeignGeneration`), and the owner names a host the copy's authority does not.
- `live_ship::pending_sends` ships a live log only to a holder that has acknowledged the volume's catalog
  (`catalogued`), since a holder without it refuses the log (`hold_bound`). b never acknowledged it, so it was never sent
  the log, and its live copy never moved.
- Nothing else would ever fix the copies: the configuration no longer names the successor once the takeover completes
  (a re-admission requires that: `RegionalConfiguration::admits`), and a takeover round's page, which installs a
  successor's authority on a holder's copy, never asked b, which was retired.

## Impact

Convergence, the nemesis's own invariant: after a takeover undone in turn (the adopter retired, the volume taken back by
its creator or taken by the third node), the re-admitted adopter never held the volume again, though it was one of the
volume's candidates. At `f = 1` the writes stayed durable on the third node, so nothing answered was lost while only the
one fault lasted; but the node counted among the volume's copies held none of its later writes, so a later takeover had
one copy fewer to count than the configuration promised. And the new owner re-shipped its catalog and head to that node
every period, each refused.

## Edits

- `crates/server/src/readmission.rs`: at the install that re-admits the host, the control shard lets go every held
  register copy whose authority names the host (`let_go_own_registers`, counted `fleet.readmission.registers_let_go`);
  held content stays. The successor's next record makes the copy afresh, as a new candidate's (`fleet::check_held_record`).
- `crates/server/src/fleet.rs`: `unplaced_heads` ships no catalog or head record of a volume this node lost to a
  successor (`verbs::lost_to_a_successor`: yielded or fenced), so a re-admitted creator's stale catalog, stamped at its
  new host epoch, cannot take such a copy first.
- `crates/server/tests/fleet.rs`: the two convergence tests (the creator's case and the third node's), green 2 of 2
  each on both Linux arches and red without the let-go (CI diag 44358084443); and
  `a_superseded_owner_restarted_warm_ships_no_record_of_the_volume_it_lost`, the record half's own test: the convergence
  scene never ships a stale record (red without the record half's guard, 3 of 3 runs passed), but an owner superseded,
  fenced and restarted warm has forgotten which records its holders acknowledged and ships them again.
- `crates/server/src/readmission.rs`' module doc, §4.8 "Authority scope"'s status, A-382 and GAPS.

## Sibling found, not fixed here

In 2 of 12 runs of the third node's case the re-admitted adopter's own log of the volume (yielded, never fenced)
built 119 and 1,714 installs for the creator, which refused each `not_the_acting_owner`, and the adopter never learned
its fence from those answers. It does not stop convergence; it is a storm of the kind A-349 found starving a takeover
round. Recorded open in GAPS.
