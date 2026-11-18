# A replacement successor waited for a key only the dead owner could deliver

**Status:** fixed 2026-11-17 (g240, A-295) for a live log at `f ≥ 2` and a sealed head at any `f`. Two failing tests
showed it before the fix (`crates/server/tests/fleet.rs`):
`a_replacement_that_never_received_the_owners_pair_serves_the_live_volume_it_succeeds_to` and
`a_replacement_that_never_received_the_owners_pair_opens_the_sealed_head_it_succeeds_to`. At `f = 1` the window is a
second failure for a live log (below), open.

**Severity:** a takeover gap since pair keys (A-92 piece 4b) met A-271's rule that a successor never makes a key. A
live volume whose successor was a node replaced at its own anchor, in the window between the replacement's
configuration and the owner's next record period that delivers it the owner's pair, was never served after the owner
died. Nothing acknowledged was lost: the cover gate holds writes in that window, and the holders keep their copies
(A-272). No flow reached it: the flows kill the owner only once the fleet has recovered, by which time the pair has
arrived (48 of 48 runs of diag-live-durability-80 read every key-wait counter 0).

## What happened

A node replaced at its anchor gets a new member id and none of its predecessor's keys. The owner shard of each volume
delivers its pair key to each neighbour once a record period (`fleet::deliver_pairs`), and no other node holds that
pair. The live log's whole entry wraps the volume's lineage key under the pair for each candidate's anchor, so the
replacement can open its entry only once the owner's pair reaches it. The successor is the member rendezvous ranks
first among the retired owner's cohort, from the configuration alone (`RegionalConfiguration::successor`), so the
replacement can be it. When the owner died first, the replacement adopted the log, fetched it, held no key for its
own entry, and was refused `no_key`; A-271's bounded wait then waited for a pair only the dead owner could deliver,
and ended `takeover_no_key_exhausted` at that head for good.

## Evidence

Read from the code first (the second owner, GAPS 2026-11-17), then held by two process tests on this Mac
(2026-11-17, `cargo test -p slates-server --test fleet -- --exact <test> --nocapture`, load 22 to 159), each run
red with the hand-off removed (`fleet::ask_handed_lineage` returning nothing) and green with it:

- `a_replacement_that_never_received_the_owners_pair_serves_the_live_volume_it_succeeds_to` (five nodes at `f = 2`,
  39 live volumes, the owner's pair delivery to the replaced anchor failed by `Daemon::inject_pair_withhold_to`). Red:
  12 and 7 of the 39 had the replacement as successor, and none was served after 434 and 455 s. On the replacement,
  `fleet.live.takeover_refused.no_key` 24 and 14, `takeover_waits_key` 1,127 and 609, `takeover_no_key_exhausted` 48
  and 70, `fleet.materialize.given_up` 12 and 7, keys handed 0. Green: 4 of 4; in the three runs that printed their
  figures, 8, 6 and 7 volumes were served 2.35, 2.11 and 2.03 s after the owner stopped, each through a handed key
  (`fleet.live.takeover_key_handed` 8, 6, 7; the fellows' `fleet.seal.lineage_handed` summing to the same).
- `a_replacement_that_never_received_the_owners_pair_opens_the_sealed_head_it_succeeds_to` (three nodes at `f = 1`,
  16 volumes sealed by snapshots). Red: 8 of 16 unserved after 451 s, `fleet.seal.envelope_unopened` 1,600. Green: 3
  of 3, 7, 8 and 6 served 2.58, 2.58 and 2.07 s after the owner stopped, every envelope key handed.

Two findings on the way, neither landed:

- The first green run of the live test failed the same way as red: 2,238 asks, each answered empty, and every fellow's
  lineage counters 0. The hand-off's stream had been given kind 25, which main's work routes (A-288) took first, and
  `fleet::serve_peer_records` routes kind 25 to that arm by a guard before its `match`, so no lineage handler ever ran
  and rustc said nothing. The stream is kind 27 now.
- Two red runs of the sealed-head test failed before the key: every fetch found no recorded holder
  (`fleet.fetch.no_holder_session` 500 and 800). At `f = 1` a sealed volume's content is on the owner and one other
  node, the same node for every volume of a run, and when that node is the one replaced the owner's death is a second
  failure for the content. The test now replaces the other node that holds the least of it.

## Root cause

The design gives a successor one way to the key: its own entry, unwrapped under the owner's pair. Only the owner's
shard delivers a pair, so a successor whose pair had not arrived when the owner died had no way left. hyper-seal's
construction names the other (`docs/seal.md` §6, "a hand-off to a successor"); slates had not built it.

## Impact

A live volume unserved after a takeover at `f ≥ 2`, for as long as the replacement stayed its successor. At `f = 1`
the window leaves the live log's recorded holders short of `f + 1` survivors, so no successor adopts it: the owner
records the replacement as a holder only once its whole entry covers it (`live_ship::target_recordable`) and never
moves to a set of `f` (`Shipper::begin_transition`). That is a second failure for the volume, not this defect, and is
owed with A-272 (ii).

## Exact edits

- `crates/server/src/seal_keys.rs`: `own_lineage_secret` (factored from `successor_lineage`), `hand_lineage` (a
  candidate's own entry wrapped to the successor's recipient key), `accept_handed_lineage` (opened with this node's
  recipient and kept only when the volume's naming record opens under it), `SealKeyError::Unchecked`.
- `crates/server/src/fleet.rs`: `LINEAGE_STREAM` served on the record session (`serve_lineage`: `acts_for`,
  `held_sealings_of`), asked by `ask_handed_lineage`; the request carries the asker's recipient key; `materialize` asks
  for a sealed head's key (`fleet.seal.envelope_key_handed`); counters; unit tests
  `a_lineage_request_of_any_other_shape_is_refused`, `only_the_volumes_acting_owner_is_handed_its_key`; test support
  `injected_pair_withhold_to` in `deliver_pairs`.
- `crates/server/src/live_ship.rs`: `fetch` asks for a handed key once the log is fetched and at each attempt of the
  bounded wait (`handed_lineage`), kept with the refusal (`Refused::handed`); `held_sealing`, `held_position`;
  `fleet.live.takeover_key_handed`.
- `crates/server/src/daemon.rs`, `state.rs`: `Daemon::inject_pair_withhold_to`, `Daemon::fleet_live_held`.
- `crates/server/tests/fleet.rs`: the five-node live test, the three-node sealed-head test; `fleet_config_with_quorum`.
- `crates/server/tests/seal.rs`: `a_lineage_key_is_handed_from_a_candidates_own_entry_and_kept_only_when_it_checks`.

## Siblings

- A volume served from a sealed head's envelope (not live-shipped): `materialize` adopted the key only from the
  successor's own entry in the head's sealing, so a replacement successor in the same window left the envelope
  unopened. Fixed in the same change: `materialize` asks the head's other candidates, and a candidate hands from the
  heads it holds (`fleet.seal.envelope_key_handed`). A head's takeover counts the members that exist now, so this one
  is fixed at `f = 1` too.
- A live log at `f = 1`: above; open.
