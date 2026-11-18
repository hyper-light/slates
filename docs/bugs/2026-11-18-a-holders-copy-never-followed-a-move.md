# A holder's copy never followed a move, so a volume moved back to its creator never moved (2026-11-18)

Contracts: §4.10 "Ownership follows the writer" and A-291 (the switch: "every holder of its copies follows and takes the
new owner's authority at its next reconcile, so a record from the old owner is refused"), §4.8 "Authority scope", and
A-398 (an install reaches the held register copies by a walk a copy a turn, each copy brought to the version as the
install once did all of them).

## Description

`a_volume_moved_to_another_member_is_served_there_and_its_old_owner_forwards` (`crates/server/tests/fleet.rs`) failed 2
of 2 on the quiet Mac Studio (adas-mac-studio-3) at main 3022c6894, and passed once at 2a5bf7264. The move from the
creator a to b completed; the move back from b to a answered `Moving` and `Refused { MoveRefused { reason: Unplaced } }`
in turn until the test's deadline. The counters the failure printed (`~/.cache/slates-gate/fleet-q1.log` there):

- c (the third node): `fleet.record.refused.unauthorized` 8,006, `fleet.content.unauthorized` 2,663,
  `fleet.move.prepare_unmatched` 1;
- b (the owner by the move): `fleet.ship.head.short` 4,000 and `fleet.ship.catalog.short` 4,006 (their sum is c's
  refusals), `fleet.move.handoff_begun` 223, `fleet.heal.unfinished` 222, `fleet.move.begun` 41, `fleet.move.refused`
  40;
- no node counted `fleet.move.followed`.

## Root cause

A move back must hand over a head held by `f + 1` members other than its owner (A-291's handoff), at `f = 1` both a and
c. c refused b's content puts and records `Unauthorized`: its copy of the volume's registers still named the creator a
as the owner, at every configuration it installed.

A-291 made a holder follow a move at each install: the install's whole pass over the held copies
(`takeover::resolve_held_objects`) ended with `moves::follow_moved_objects`, which routed each copy of a moved object to
its exception's owner, and the authority reconcile that came after it gave the copy that owner's authority. A-398 cut
that pass into a walk a copy a turn (`fleet::install_on_copy`: fence, `takeover::resolve_held_object`,
`reconcile_held_copy`). Its run commit (12d8ff6e4) had no follow in the per-copy step; its merge with main (9c5fca527)
put A-291's call at the end of `resolve_held_object`, after that step's early return for a copy whose owner is a live
member and not retired. A moved object's owner is always such a member, so no copy ever followed a move: the reconcile
gave each copy its old owner's authority again, by its old route.

The call that remained ran only for a copy of a retired owner, and then as a whole pass over every held copy, once for
each such copy.

The three commits main suspected (A-435, A-436, A-437) are not the cause: A-398 predates both points of the bisect.
What let the test pass once at 2a5bf7264 is not read from any log.

## Impact

Every move on main since A-398's merge: no holder of a moved volume's copies took its new owner's authority. The new
owner's records were placed only through members that made a fresh copy of the volume (the old owner, which held none),
so at `f = 1` with three members the move there completed and a move back, which needs both other members, never did.
A takeover of the new owner would have found the holders' copies under the old owner's authority. At a retired owner's
install the follow was a whole pass per copy, quadratic in the held copies.

## Exact edits

- `crates/server/src/moves.rs`: `follow_moved_object(state, regional, local, object)`, the follow for one copy (routed to
  the exception's owner, or dropped once the move is adopted outside that owner's placement), counted per copy;
  `follow_moved_objects` is its whole pass, now `#[cfg(test)]`, the unit tests' oracle.
- `crates/server/src/fleet.rs`: `install_on_copy` calls it between `resolve_held_object` and `reconcile_held_copy`;
  the new unit test `a_holders_copy_of_a_moved_object_follows_the_move_as_the_install_walk_reaches_it` and its helper
  `walk_the_install`.
- `crates/server/src/takeover.rs`: the call removed from `resolve_held_object`; the test-only whole pass
  `resolve_held_objects` ends with the whole follow, A-291's order.
- `crates/server/src/owner_location.rs`: a doc line names the per-copy follow.
- Design: A-291's and A-398's statuses, the A-487 amendment; GAPS: the move's entry.

Sibling sweep: the walk's per-copy step was compared with A-291's whole pass line by line (fence, lineage and
let-go, follow, authority); the follow was the only step the cut lost.
