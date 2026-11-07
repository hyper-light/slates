# A live shipper made before its candidates were known shipped to nobody (2026-11-07)

Contracts: A-200 (a live-shipped volume's publications ship to its other candidates; a write is durable once the owner
and `f` of them hold it), §4.10, §4.8 (the configuration names each object's candidates).

## Description

A live-shipped volume did not conform as a plain one: its log stood at `head=2 durable=none owed=true` from the start of
a suite, its later publications were refused for credit, and its stable replies were answered "try again" for the whole
run. Seen first by the conformance agent on macOS (2026-11-06: `fleet.live.refused` 137 and 52, candidates holding
nothing; pjdfstest's calls answered `ETIMEDOUT`), and on Linux in my diag run 42325923902 (native-linux-fuse fsx over a
live volume: the owner's `appended` 3, `refused` 49, `durable_late` 51, and both candidates' `fleet.live` counters
empty).

## Root cause

`live_ship::published` made the volume's `Shipper` at its first publication with the holders the owner's configuration
named then, and the shipper never changed them. The conformance harness uses a fleet the moment `bootstrap root`
returns, on node `a`; when `a` is the bootstrap node (the lowest member id, as in the Linux run: `a`'s id was the lowest
of the three), its configuration names only itself until the others are admitted, so the shipper's holders were empty
for good. Nothing was ever sent (the candidates' counters were empty), no entry could be durable at `f = 1`, and the
credit ran out after three entries.

## Impact

Every live-shipped volume made on a fleet's bootstrap node within its first moments, and any live volume whose
candidates later change (a candidate's takeover, a neighbourhood change), shipped to the wrong set or to nobody; with
nobody, every barrier waited and every stable reply answered "try again" for good.

## The first fix was unsafe, and was replaced

The first fix (`Shipper::set_holders`) replaced the holders outright. raft-chaos showed it unsafe on 2026-11-07: a new
set sharing fewer than `f` members with the old holds none of the durable writes, so a takeover counting it adopts a
head without them. The model below confirms it (the record-at-once mutation breaks NoLoss). The replacement changes the
holder set in the manner of Vertical Paxos (Lamport, Malkhi and Zhou, PODC 2009): ship to both sets, count durability
over each, and switch only once the new set holds every durable write and the record names it.

## Exact edits

- `crates/db/src/live.rs`: `Shipper::begin_transition` starts a move to a new holder set: the shipper ships to both
  (`Shipper::targets`) and counts a position durable only once `f` of each hold it. `Shipper::transferred` says the
  target holds every durable write and the head the shipper started from. `Shipper::activate` makes the target the
  holders once the record names it and it is transferred. A transferred target is kept against a later change until it
  is activated, since its record may be in flight.
- `crates/server/src/live_ship.rs`: `follow_candidates` begins a transition to the volume's other candidates as the
  shard's configuration names them now, at every publication and every record period (counted
  `fleet.live.holders_transition`), and activates it once transferred (`fleet.live.holders_activated`). Until A-240's
  record lands (raft-chaos), the record is the configuration itself. `pending_sends` ships to every target.

## Evidence

- The model (`crates/db/tests/live.rs`, `Following`): the nemesis over `4f + 1` hosts while the configuration names
  random candidate sets for the volume (any overlap with the old, none included, now and then none; half the runs start
  with none, as a volume made on a bootstrap node). 200 seeds each at `f = 1` and `f = 2`, 3,000 steps:
  - the joint transition (`a_live_logs_holders_follow_its_candidates_through_a_joint_transition`): 0 broken; 3,622
    activations, 599 of them keeping fewer than `f` of the old holders; 243 takeovers; 11,108 durable positions;
  - the record naming each new set at once (`recording_a_new_holder_set_before_it_holds_the_log_loses_durable_writes`):
    NoLoss broken;
  - the shipper keeping its first holders, the daemon before this fix
    (`a_shipper_that_keeps_its_first_holders_never_reaches_new_candidates`): 384 of 400 seeds never converge;
  - `transferred` without the start floor: NoLoss broken (`f = 2`, seed 63). A successor starts from the adopted head
    with nothing durable of its own, so a move to a disjoint set counted as transferred at once.
- Unit tests: `a_shipper_made_before_its_candidates_were_known_ships_to_them_once_named` and
  `a_new_holder_set_counts_only_once_it_holds_every_durable_entry`.
- `a_live_volume_made_on_the_bootstrap_node_at_once_ships_to_its_candidates` (`crates/cli/tests/live_fleet.rs`): three
  daemon processes, the volume made on the bootstrap node the moment it bootstraps. With the replace rule, red 3 of 3
  on Linux and 6 of 6 on macOS with it switched off. It also needs the placement fix of the same day (the head counted
  under a stale joint cohort).
