# A holder behind its own records answered every later version out of order (2026-11-18, A-412)

## Description

After a green's owner died, the takeover's successor committed the versions it adopted again through the register path
(`takeover::commit_adopted`). A holder whose replica had not reached those versions (the old owner's shipment of the
newest version never reached it) took their records but not their inputs, since the register path carries none. Its
replica stayed where it was, and the record plane never ships a placed version again. So the holder answered every
later version `merge.out_of_order`, for good. At `f = 1` in a three-node region, with the old owner gone, that holder is
the successor's only other member, so the successor's next version never reached its quorum and every submit to the
green waited forever.

## Root cause

`merge_service::record_standing` took a record ahead of the replica's next version as out of order and nothing more. It
never asked whether the versions between were already in the holder's own ledger, and nothing else brought the
replica up to them: the A-313 rebuild runs only for a holder with no replica, and A-333's advance only on a successor.

## Impact

A green taken over at `f = 1` with one holder a version behind accepted no new version on the successor; at larger `f`,
each such holder was lost to the green for good. Seen in the fleet test below: the holder answered version 4 out of
order 4,865 times over 405 s, and version 4 never placed.

## Exact edits

- `crates/server/src/merge_service.rs`: `record_standing` takes the record's sender; a record ahead of the replica goes
  to `catch_up`. With the versions between all in the holder's ledger and their inputs held, the replica is advanced
  through the version before (`merge_origin::advance_replica`) and the record served as its next. With an input not
  held, it is fetched. A version between whose record the holder lacks is out of order, as before.
  `MergeShardState::catching_up` keeps one fetch a green at a time.
- `crates/server/src/fleet.rs`: `fetch_ledger_inputs` fetches each unheld input by identity from the record's sender, the
  green's owner, which holds every version's inputs while its records name them (A-244). It runs in a detached task of
  the shard, one live exchange's budget a manifest, and stages each input as placed for its version. Counted
  `merge.catch_up.begun`, `merge.catch_up.fetched`, `merge.catch_up.unspawned`.
- Test: `a_holder_behind_its_own_records_after_a_takeover_fetches_their_inputs_and_takes_the_next_version`
  (`crates/server/tests/fleet.rs`). Green in 4.4 s on the Mac Studio (kiln r355, 2026-11-18, debug build). Red without
  the catch-up: the holder answered version 4 out of order 4,865 times and the test waited 405 s for version 4.

## Siblings

A holder restarted warm with no replica, holding a record whose input it lacks: the A-313 rebuild meets the missing
input, installs nothing and is tried again at the next record, and nothing fetches the input. Not fixed here; recorded in
GAPS beside this entry.
