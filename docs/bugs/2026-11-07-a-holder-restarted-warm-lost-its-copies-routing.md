# A holder restarted warm lost its copies' routing, and refused their successor's adoption

**Found:** 2026-11-07, by `a_member_down_through_a_voters_restart_is_still_taken_over` in
`crates/cli/tests/live_fleet.rs` (agents/raft-6). Amendment: A-250 (§4.8 "Promotion and takeover"). Once the restarted leader could retire the dead owner at f + 1
votes (caf06d71), the test still failed in 1 of about 3 runs: no survivor served the owner's volume within the fleet
wait.

## Description

Three nodes at f = 1. The test stops one voter and restarts it warm, keeping its member id. Then it kills the owner of
a volume. The council retires the owner, and the successor's takeover learns both of the owner's objects. It never
adopts them. In one stalled run (pid 1498286) over about a minute:

- the successor counted `fleet.takeover.adopt_unplaced` 1,058 times, every one `adopt_unplaced.unacked`;
- the restarted holder counted `fleet.record.refused.foreign_generation` 1,056 times;
- `fleet_takeovers_pending` stayed at 2 on the successor until the test gave up.

The counters that show this were added in the same change. Before them, the holder refused without a trace and the
successor's round showed only its pending count.

## Root cause

A holder's routing view (`FleetNode::routing`, read by `fleet.object_owner`) is not persisted. A record accepted
live is routed by `track_object` as it is accepted. A record restored from the held image after a warm restart
(`content_holder::restore_held_registers`) got its acceptor back and no routing.

Every install reaches a held object only through routing:

- `takeover::resolve_held_objects` moves an object of a retired owner to its successor and records
  `departed_owners`;
- `fleet::reconcile_held_authority` raises its acceptor to the installed generation;
- `takeover::list_page` lists and fences it for a successor's round.

All three skip an unrouted object. So the restarted holder kept its acceptor at the old generation. It refused the
successor's adoption at the new one `ForeignGeneration` (`Acceptor::check`) every period. It also answered the
successor's page empty and complete, so the round counted its promise while its copy was never fenced.

## Impact

A member that dies while another voter restarts is never served again: its volume stays unadopted on the successor,
with nothing lost. Correctness: a restarted holder's unlisted, unfenced copy was counted as a promise. In this test
the successor held the newest record itself. If the restarted holder had held a newer one, the successor would have
adopted the older record. The empty page is no longer reachable from a restart once the copy is routed, since an
install routes it before any page under that install is answered (a page under a version the holder has not
installed is refused `ConfigurationStale` or left for its fetch).

## Exact edits

- `crates/server/src/content_holder.rs` `restore_held_registers`: route each restored copy to the owner its acceptor
  names (`track_object_owner`), as accepting a record does.
- `crates/server/src/daemon.rs`: `a_restored_holders_copy_moves_to_its_successor_and_accepts_its_adoption`, failing
  first ("the restored copy moves to its successor"), passing with the edit. It drives the real image round trip
  (`held_image`, `split_held`, `restore_held_registers`).
- `crates/server/src/fleet.rs`: `reconcile_held_authority` is `pub(crate)` for the test; the holder counts every
  refused held record by kind (`fleet.record.refused.<kind>`).
- `crates/server/src/takeover.rs`: the round counts its silent outcomes (`page_unmatched`, `pages_unsent`,
  `page_unbound`, `round_behind`, `adopt_unplaced` and its three causes, `unpromised`).

## Siblings, reported, not changed

- A holder answers a page that leaves out a copy it holds as complete, and the successor counts that as a promise.
  Any other path that leaves a held copy unrouted would reopen the unfenced-promise hole. A page could list every
  held copy whose acceptor names the departed owner, routed or not; that is a §4.8 change for the takeover's owner.
- `peer_host` on the record and takeover streams falls back to the seed's id when `learned_members` lacks the peer,
  and `learned_members` starts empty at boot. It did not cause this stall (the pages bound), but a restarted holder
  that has not relearned a peer would refuse that peer's records `Unauthorized`.
