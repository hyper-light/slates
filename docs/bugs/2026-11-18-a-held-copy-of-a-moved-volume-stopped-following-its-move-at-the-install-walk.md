# A held copy of a moved volume stopped following its move at the install walk

Date: 2026-11-18. Area: `crates/server/src/takeover.rs` (`resolve_held_object`), `crates/server/src/moves.rs`
(`follow_moved_object`), `crates/server/src/fleet.rs` (the install walk, `install_on_copy`). Design: §4.8 "Promotion and
takeover", "Authority scope"; §4.10 "Ownership follows the writer"; A-291 (the move), A-398 (the install walk);
amendment A-485. Goal 85, bug 1.

## Description

`a_move_whose_target_dies_at_the_switch_is_served_by_its_successor` (`crates/server/tests/fleet.rs`) failed every run
from A-398 on: 4 of 4 across macOS (the quiet Mac Studio, main's r242 and r277, at 3022c6894 and 2a5bf7264) and Linux
(adacachy, r327). Three nodes at `f = 1`; a moves a volume to b; b dies at the switch, before it adopts; the survivors
retire b. b's successor never served the volume:
- neither survivor adopted or materialized it: c counted no `fleet.takeover.adopted`, `fleet.move.adopted` or
  `fleet.materialize.*`, and no log line names a takeover or materialization of it;
- a let its frozen copy go (`fleet.move.released` 1; log: "a volume 947c… this node moved away is released to member
  …");
- every read through a's client then met `NotFound` on Linux (13,068,430 of them over the poll) and
  `HomedElsewhere { region 0 }` on macOS (1,605,259 forwards, each refused by the survivor whose route named a).

The test had passed after A-291's handoff (7.85 s and 5.87 s on the Mac Studio).

## Root cause

A-291 made every holder follow a move: after the install had resolved every held copy, one pass over the moved objects
(`moves::follow_moved_objects`) took each copy's route to the exception's owner, and the reconcile that follows moved
the copy's authority there. A-398 cut the install into a walk that reaches the copies a slice a turn, and the per-copy
step (`takeover::resolve_held_object`) ended with that pass, after its early returns. It ran only when the copy reached
was routed to a retired host.

At a move's switch every copy is routed to the volume's live old owner, so the step returned early for each and no
holder followed the move: each copy kept a's route and a's authority. A takeover lists a copy only along its authority's
line (`slates_db::takeover::copy_falls_to`), and a's line ends at a live member, so when b died no survivor's round
learned the volume. Both confirmed their shares without it. The configuration follows a move's exception by lineage
alone (`RegionalConfiguration::follow_moved_takeover`): it handed the volume's line to b's successor, or dropped the
exception where that successor was a, the creator. a's move then read the switch as settled and let its frozen copy go
(`moves::step_for`'s `Release`). Nobody held the volume.
Each platform's last read is what one of the two successors gives (inferred from the code; which survivor each run
ranked is not in the counters read, though the member id a released to names it):
- a successor a: a's route names itself and its catalog no longer holds the volume, so a read meets `NotFound`
  locally (Linux, r327);
- a successor c: a forwards to c, and c, whose route still names a, refuses each forward `HomedElsewhere`
  (`verbs::forward_redirect`; macOS).

The unit test reproduces the cause with no fleet: a holder's copy of a volume moved to a third member, walked by the
install step, stays routed to its old owner, and the target's takeover lists nothing for its successor.

## Impact

Any fleet volume moved with `slates move` or `Instance::move_volume` whose target died before adopting it was lost:
the old owner released its copy and no survivor served it. Whether the move back to the creator (goal 85 bug 2) leans
on the same following is not shown: a holder the new owner's own move round reaches takes its authority in that round,
whatever the walk did.

The pass inside each per-copy step also went over every held copy each time it ran, a pass per copy reached, against
the walk's bound on a turn's work.

## Exact edits (A-485)

- `crates/server/src/moves.rs`: `follow_moved_object` brings one held copy to its object's move under the walk's
  configuration: routed to the exception's owner (`fleet.move.followed`), or dropped once the move is adopted by an
  owner whose placement leaves this holder out (`fleet.move.reclaimed`). It replaces the whole pass
  `follow_moved_objects`.
- `crates/server/src/takeover.rs`: `resolve_held_object` judges the copy by its route (`resolve_by_route`, the old
  body), then calls `follow_moved_object` whatever the route judged. The failing test is in its unit tests.
- `crates/server/src/fleet.rs`: `install_on_copy`, the walk's per-copy step, is `pub(crate)` so the test drives it.
- Docs: §4.8 (A-398's status), the amendment log (A-485), GAPS (the move entry).

## Tests

`a_held_copy_follows_its_move_at_the_install_walk_and_falls_to_the_dead_targets_successor` (`crates/server/src/
takeover.rs`): A-340's seven-member region, a copy of another member's volume held under that member's authority, the
volume moved to a third member and the install walked onto the copy, then the target retired and the next install
walked onto it. It expects the copy routed to the target after the move's install, and listed for the target's
successor in the target's takeover. Red without the fix on the Mac Studio (d1323898, debug); green with it. Command:
`cargo test -p slates-server --lib a_held_copy_follows_its_move`.

The fleet test that found it, and the volume-moved test (bug 2), are main's to run on macOS and Linux.

## The same cause, goal 85's bug 2 (A-487, g230)

`a_volume_moved_to_another_member_is_served_there_and_its_old_owner_forwards` failed 2 of 2 on the quiet Mac Studio
(adas-mac-studio-3) at main 3022c6894: the move there completed, and the move back to the creator answered `Moving` and
`Refused { MoveRefused { reason: Unplaced } }` in turn until the test's deadline. The counters the failure printed
(`~/.cache/slates-gate/fleet-q1.log` there) are this record's cause, seen from the other side of a move: the third node c
refused every head and catalog record the volume's new owner b shipped `Unauthorized` (`fleet.record.refused.
unauthorized` 8,006 = b's `fleet.ship.head.short` 4,000 + `fleet.ship.catalog.short` 4,006) and 2,663 of its content
puts (`fleet.content.unauthorized`); b began 223 handoffs of the head off itself and finished none
(`fleet.heal.unfinished` 222), and its move back was refused `Unplaced` 40 times; no node counted
`fleet.move.followed`. A move back must hand over a head held by `f + 1` members other than its owner, at `f = 1` both
the creator and c, and c's copy still named the creator. It is not the A-435, A-436 or A-437 commits main bisected
between: A-398 predates both points; what let it pass once at 2a5bf7264 is not read from any log.

A-485's fix closes it. Its regression test, `a_holders_copy_of_a_moved_object_follows_the_move_as_the_install_walk_
reaches_it` (`crates/server/src/fleet.rs`), installs the move through the walk as the coordinator drives it
(`install_slice` until the walk ends) and judges what the move back needs: the copy routed to the new owner, its
authority naming it, the new owner's next record accepted and the creator's refused `Unauthorized`, counted followed
once. Green with A-485's rule (Mac Studio d1323898, kiln r26, debug, 0.77 s, beside A-485's own test); red with
A-485's call to `moves::follow_moved_object` removed from `takeover::resolve_held_object` in a throwaway copy: the copy
stayed routed to the creator under its authority, the new owner's record was refused `Unauthorized`, the creator's
accepted, and nothing was counted followed, the shape the fleet run showed. (g230's branch agents/g230-a487-move-back
built the same rule independently and was dropped for A-485's.)
