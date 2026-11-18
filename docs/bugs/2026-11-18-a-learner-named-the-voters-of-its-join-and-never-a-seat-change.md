# A learner named the voters of its join and never a seat change (2026-11-18, A-380)

## Description

`a_learner_learns_who_votes_once_the_voter_it_joined_from_is_gone` (`crates/server/tests/fleet.rs`) failed 3 of 24 runs
at one CPU beside two busy loops (diag-g225-forming-fleet-reaches-2, GitHub ubuntu x86_64 and arm64, 2026-11-18). In each,
a learner named only the stopped bootstrap voter as the council's voters (`council_known_voters` `[bootstrapper]`) while
the leader's voter set was another, and the learner held that dead voter `BelievedDead` as its only voter.

## Root cause

A learner takes no part in the council's Raft. It knew who votes from its join state's voters (the bootstrapper's, when
it joined from the bootstrapper's state) and from the voters a fetch reply named (`Fetched::voters`, the answerer's known
voters), adopted only with a newer configuration view (`consensus::adopt_fetch`). A seat change is a Raft configuration
entry with no command (`RaftNode::begin_membership_change`, `complete_membership_change`), so the regional configuration's
version did not move: no learner fetched on it (a learner fetches on a divergence of its view, a newer version announced
on a probe, or a `ConfigurationStale` refusal), and a fetch at the same version adopted nothing. The council seats its
voters after the learners join, so a learner kept its join's voters until some later change moved the version, and then
took whichever answer first brought it, another learner's stale voters included.

## Impact

A learner's contact rule (`fleet::direct_contact`'s `Voter`) and its view of who votes were wrong for as long as no
configuration change followed a seat change: a learner could drop a voter it should keep (A-337 keeps the pair keyed from
the voter's side since), and an operator or a test reading `council_known_voters` saw a voter long gone.

## Edits

- `crates/db/src/register.rs`: `RegionalConfiguration::voters`, `seats`, `seat` (the set in id order, the version moved
  only on a change).
- `crates/cluster/src/raft_wire.rs`: the set encoded last; `REGIONAL_LAYOUT` 3.
- `crates/cluster/src/config_group.rs`: `ConfigCommand::Seat` (tag 9), `Reconfiguration::Seat`, `record_voters` (the
  leader's proposal once no change is in flight), `known_voters`/`known_voter` read the record; `learned_voters` and
  `learn_voters` removed.
- `crates/server/src/consensus.rs`: the fetch reply carries no voters; `adopt_fetch` adopts the configuration alone.
- `crates/server/src/fleet.rs`: the leader period calls `record_voters`.
- Tests: `a_learner_names_the_voters_the_configuration_records_through_a_seat_change`,
  `a_seat_records_the_voters_in_id_order_and_advances_the_version_only_when_they_change`, the configuration round trip,
  the three end-relative encoding tests and the layout golden (`RCFG` version 3).

## Evidence

- Red-checks on the Mac Studio (kiln r3624, 2026-11-18, a copy of 7e76e684f): with the learner reading its Raft voters
  as before (`known_voters` ignoring the record), the learner unit test fails at "the learner names the changed voters"
  (it names `[1, 2, 3]`, the voters of its join); with the leader's record withheld (`record_voters` proposing
  nothing), it fails at "the leader recorded the voters in force". Both pass with the change.
- Focused runs, same machine: `slates-db` `register::tests` 76 of 76, `slates-cluster` `raft_wire` and `config_group`
  48 of 48, the server's consensus, member-task and dialer tests 7 of 7, and ten fleet tests (the four learner tests,
  the learner reports, the council's election and retirement, two takeovers) each passed.
- Owed (the supervisor's diag lane): the learner tests' rate at one CPU beside two busy loops, as
  diag-g225-forming-fleet-reaches-2 measured the 3 of 24 above.

## Siblings found, reported

- The root group's learners have the same gap: `drive_root_learner_fetch` asks the root voters of their join, and a root
  seat change moves no root version (GAPS, the open sibling of 2026-11-08). The Windows two-region formation failure of
  run 44298844583 shows three root learners naming the root bootstrapper after the root's voters moved.
