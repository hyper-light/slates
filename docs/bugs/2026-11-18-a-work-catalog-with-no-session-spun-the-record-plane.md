# A work's catalog with no session to ship over spun the record plane inside one poll (2026-11-18)

## Description

`a_submit_made_while_the_council_forms_is_answered_once_its_record_commits` (`crates/server/tests/fleet.rs`) ended its
test binary with exit 4 on CI's windows-11-arm runner: "shard 0 is spinning, 1000 ms of CPU without a step" (protected
run 44271637767), then with A-318's window "8625 ms of CPU against the 8607 ms window" (run 44298844583) and "8031 ms
against the 7960 ms window" (run 44310013691). Each daemon of the test runs one shard, so shard 0 is its control shard.
The supervisor read the first as a broken thread clock ("1000 ms at 0.08 s"); it was not: the trace's last line was at
0.08 s and the binary ran 1.6 s, and the later verdicts came about 8.7 s after the shard's last traced step, a debug
build at 7.9 to 8.6 times machine code.

## Root cause, from the logs

diag-g251-spin-3 (run 44326393195, windows-11-arm) dumped every shard's pulse at the stuck verdict, with the task the
shard was polling named by its spawn: on its first run, shard 0 was inside one poll of
`slates_server::fleet::run_record_plane::{{closure}}` (slot 11) with 8,890 ms of CPU, no full-ring wait.

The record plane runs the merge plane at once when nudged (`fleet::pace_running_merges`: `within(remaining,
merge_service::nudged())`). A merge pass ships each work's catalog register that is not yet committed (A-255,
`ship_new_works_catalogs`), and its `ShipWorkCatalog` arm nudged the plane after every ship. A ship that found no
session to a candidate still missing the catalog (`fleet::ship_head`: no holders, `fleet.ship.catalog.no_session`)
returns at once, having awaited nothing, so `nudged()` was ready again, and the next pass ran in the same poll, and the
next. On a one-shard daemon its owner shard is the control shard, so `call_within` runs the pass's question inline and
nothing in the loop ever returns `Pending`. The tasks that would bring the sessions up run on that same shard, so the
loop never ends: the watchdog judged the shard spinning a spin window later and ended the process. In the test, the work
is created while the council forms, before every record session is up; on windows-11-arm's slower debug build that
order is the common one, which is why it failed there 3 of 3.

## Impact

Any node that creates a work (or holds a submit for its route, `work_home::await_route`) while a candidate of the work's
catalog has no session: a fleet forming, a peer partitioned or restarting. On a one-shard node (the fleet tests' shape,
and any small host) the control shard is held for good and the daemon ends exit 4 (a lone daemon) or is killed by its
anchor; on a node of several shards, each pass's question to the owner shard suspends, so the plane spins at a whole
CPU through the rest of each period instead. GAPS' "a kill the rule does not touch" sighting of 2026-11-17 ("shard 0 is
spinning, 1077 ms", in the third of 13 A-255 and tombstone tests on a Mac) has this shape.

## Exact edits

- `crates/server/src/fleet.rs`: `ship_head` returns `HeadShipped { dispatch, recorded }`, where `recorded` says the
  commit's placement came back and was recorded on the head's owner shard; `ship_owed_catalog` passes it on; the period's
  head shipment takes the dispatch as before.
- `crates/server/src/merge_service.rs`: the `ShipWorkCatalog` arm nudges the plane only when `recorded`, as only an event
  that makes new work does (`nudge_record_plane`'s own rule); a ship that found no session, or that the runtime refused,
  waits for the coordinator's next period.

## Tests

- `a_work_catalog_with_no_session_to_ship_over_waits_for_the_period` (`crates/server/tests/fleet.rs`): a formed fleet at
  `f = 1`, one shard a node, the creating node's record sessions to both peers held out for five periods, a work made
  meanwhile. On a Mac Studio (d1323898, load 14 to 19): green in 1.94 s, the catalog shipped with no session within the
  bound and the submit answered version 1; red with the arm nudging whatever the ship did: "shipped 233090 times with no
  session in 25 periods (at most 105)". `a_submit_made_while_the_council_forms_is_answered_once_its_record_commits`
  passes there too.
