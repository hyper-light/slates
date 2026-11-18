# A learner, the pair's lower anchor, dropped a voter that kept it (2026-11-18, A-337)

## Description

`a_learners_reports_reach_the_council_once_the_voter_it_joined_from_is_gone` (`crates/server/tests/fleet.rs`) failed
once in g251's diag run 44262759484 (job 134659842188, ubuntu, 2026-11-18 08:13): its fixture
`fleet_that_lost_its_bootstrap_voter` gave up its formation wait (`assert_fleet_forms`, 49 to 50 periods of
`mesh_formation_periods(5)`) after the initial consensus had settled in 0.83 s. Node c was unmeshed.

## What the logs show

The formation report's contact facts for the pair (c, e):
- c, a council voter, held e `AsVoter keyed=None canonical=Some(false)`: c keeps contact with every member as a voter,
  and is not the pair's canonical dialer.
- e, a learner, held c `OutsideNeighbourhood keyed=None canonical=Some(true)`: e did not know c votes, and e is the
  pair's canonical dialer (the lower anchor).

## Root cause

A pair's probe plane is keyed by an epoch only its lower anchor announces, over the record session that anchor dials
(`member_task::announce_epochs`). The role rule (`fleet::direct_contact`) is kept from both sides since 2026-10-10
(`Neighbour`/`Holder`, `Voter`/`AsVoter`), but only while both sides read the same configuration and voters. A learner's
voters can be stale: a seat change is a Raft configuration entry with empty command bytes, so the regional
configuration's version does not move and no learner fetches on it; and a learner's fetch answer carries the answerer's
known voters, so a learner answered by another learner takes that one's. e did not know c votes, judged c
`OutsideNeighbourhood`, idled its link to c and never announced the pair's epoch. c dialed e and kept that session, but
the session c dials is not the pair's canonical one, so c's probes to e were never keyed.

## Impact

A pair whose lower anchor is a learner with stale voters, and whose higher anchor is a voter it does not know, never
forms its probe mesh: the voter cannot probe the learner, the learner's reports may never reach the voter, and a test
that waits for the mesh waits out its budget. Seen once in that run; the rate rises with load (stale voters last while
no configuration commits).

## Edits

- `crates/server/src/state.rs`: `ShardState::dialed_in`, the record sessions each peer dialed to this node that this node
  serves now, by the peer's anchor.
- `crates/server/src/fleet.rs`: `DialedIn`, a guard counting a session in `dialed_in` for `serve_peer_records`' life;
  `DirectContact::Dialed`, kept, when this node is the pair's canonical dialer and serves a session the peer dialed to
  it (`dialed_by_a_higher_anchor`).
- `crates/server/src/daemon.rs`: the state's initializer, and the test
  `the_pairs_canonical_dialer_keeps_a_peer_that_dialed_it_and_only_it`.

## Siblings found, reported

- The learner's stale voters (GAPS, open): the principled fix is the voter set inside the versioned configuration a
  learner fetches, a format change for Ada's design review.
