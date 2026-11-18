# Live batches shared the work route's request kind (2026-11-17)

Contracts: §4.8 and D-14 (a fleet connection multiplexes every request by its kind, and the serve loop dispatches by
kind alone, "never a guess from the bytes"), and A-200's amendment of 2026-11-17 (the entries a holder lacks go as one
batch a window).

## Description

g230's batch piece gave a batch of live entries its own request kind, `LIVE_ENTRIES_STREAM = 25`, "the next kind after
the fleet's death reports (24)". The same day A-288 (a183c060b) landed on main with its work route's question on fleet
stream 25. Neither branch saw the other. Merged with main 4f65126ab, the burst flow
(`a_burst_of_writes_to_a_live_volume_ships_its_changes_not_the_volume`, `crates/server/tests/fleet.rs`) failed 6 of 6
runs on a Mac Studio, each in under 3 s, where it had passed before the merge.

## Root cause

`fleet::serve_peer_records` checks the work route's kind before its general dispatch, so every batch reached
`work_route::serve`, which answered with nothing. The owner read the empty answer as an answer (`ship_lane` counts any
reply with a round trip as answered), so `lane_ended` sent what the holder lacked again at once: the same batches, at
the network's speed. Measured with the owner's shipper traced (`DIAG2` lines, a scratch copy, never committed):

- 1,078 to 1,544 batches carrying 9,639 to 14,425 entries in about 2.9 s, for 17 entries appended;
- the holders took 1 to 3 entries each, by the single sends the period made, and refused 4 to 195 of those `behind`;
- the owner's shipper read every batch's answer as `Malformed` (an empty reply);
- with the holders taking nothing, a publication made meanwhile was a 630,815-byte delta against a credit of 580,424,
  turned away, and the volume shipped whole: `fleet.live.refused.no_credit` 0 → 1, `fleet.live.whole` 1 → 2.

## Impact

Had both landed, every live batch on main would have been served as a work route's question: a holder behind by two or
more entries could never catch up by batches, the owner would have resent at the network's speed, and a live volume's
writers would have waited on its period's single sends. Not on main: caught at the merge.

## Exact edits

- `crates/cluster/src/live.rs`: `LIVE_ENTRIES_STREAM` is 26.
- `crates/cluster/src/lib.rs`: `LEDGER_PROMOTE_STREAM` is public, so the check below can list it.
- `crates/server/src/fleet.rs`: `request_kind_tests::every_request_kind_the_record_session_dispatches_is_its_own`
  lists every kind a fleet connection carries (25 of them) and fails on any two equal. Red with the batch kind at 25:
  "work route" and "live entries" share 25.
- `crates/server/tests/fleet.rs`: the burst flow also requires each holder to take every entry of the burst
  (`fleet.live.held`), so a batch that reaches no holder fails it by behaviour, whatever its kind.
- `docs/wip/SLATES_DESIGN.md` (A-200's amendment of 2026-11-17) and `docs/wip/GAPS.md`: kind 26.

## Evidence

With the kind at 26, the burst flow passed 3 of 3 runs on the same Mac Studio, 1.82 to 2.21 s each: 1 or 2 batches
carrying 15 entries, no publication turned away, one whole record, one holder taking all 17 entries during the burst.

## Siblings

No other two request kinds share a value today (the check above). The kinds are still numbered by hand in each module;
two branches can still pick one number, and the check now fails the second to merge.
