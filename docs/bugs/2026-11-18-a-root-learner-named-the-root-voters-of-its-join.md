# A root learner named the root voters of its join (2026-11-18, A-406)

## Description

On CI's Windows runner (run 44298844583, `a_placed_snapshot_is_held_whole_by_f_plus_one_hosts_of_the_mirror_region`,
six nodes in two regions), after the root group moved from its bootstrapper to the two regions' representatives, three
of the four root learners still named the bootstrapper as the only root voter (`root_voters` `[bootstrapper]`).

## Root cause

A root learner takes no part in the root group's Raft. It knew who votes only from its join state's voters: a root seat
change is a Raft configuration entry that moves no root version, and a root learner fetched only when its alive view of
the regions differed from the configuration's (`fleet::root_diverges`), which a seat change never makes true. The same
gap A-380 closed for the regional council.

## Impact

A root learner asked its join's root voters for fetches and kept direct contact with them as voters
(`fleet::direct_contact`), whoever the root's voters were now. A representative that died left its region in place, so
the learner never fetched, and asked a voter long gone until a region changed; its fetch target fell back to the alive
representatives only once it held no session to any voter it knew.

## Edits

- `crates/db/src/register.rs`: `RootConfiguration::voters`, `seats`, `seat`.
- `crates/cluster/src/raft_wire.rs`: the set encoded last; `ROOT_LAYOUT` 2.
- `crates/cluster/src/root_group.rs`: `RootCommand::Seat` (tag 4; `RootCommand` is no longer `Copy`),
  `record_voters`, `known_voters`, `known_voter`.
- `crates/server/src/fleet.rs`: the root leader's period records its voters; a root learner fetches from the voters it
  knows and also when one is no longer alive; direct contact reads the voters a node knows.
- Tests: `a_root_learner_names_the_voters_the_configuration_records_through_a_seat_change`,
  `a_root_seat_records_the_voters_in_id_order_and_advances_the_version_only_when_they_change`, the root command and
  configuration round trips, the layout golden.
