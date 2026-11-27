# A green standing alone waited on its holders for a join that never comes

- **Date:** found 2026-11-27, in Windows diag CI's fleet suite. Reproduced on a MacBook the same day at main f7a97ead9,
  and fixed the same day (A-932).
- **Status:** fixed.
- **Subsystem:** the holder's replica of a green (§4.16 "Apply on holders"). The code is `make_origin_replica` in
  `crates/server/src/merge_service.rs` and `rebuild_through` in `crates/server/src/merge_origin.rs`.
- **Severity:** in a fleet (f ≥ 1), every green created alone could never commit a version. Its owner's first submit
  waited until the client's deadline. No Unix lane runs `crates/server/tests/fleet.rs`, so only Windows CI saw it.

## Symptom

`a_holders_submit_names_its_increment_and_the_owner_fetches_it_by_identity` and the rest of the fleet suite failed
with `client Submit: deadline exceeded`. That test's first submit on a green that node B replicates never answered.

The run sampled every 2 s (diag branch `w-interop/windows-fleet-diag`, MacBook, debug) showed the cause:
- owner A's `merge.record.shipped` and `merge.record.ship_acks` rose by about 20 per 2 s, re-shipping version 1's
  record about 10 times a second;
- holder B's `merge.replica.member_unknown` and `merge.out_of_order` rose at the same rate.

So B refused every record for good.

## Root cause

Two changes met:

- **4c38e425e** (2026-11-19, A-636 part 2b-2) mints every green as a family id. That includes a green `CreateGreen`
  makes alone: `verbs::found_green` calls `fresh_family_id_at(state, None, 0)`, so the green is a family of its own,
  which lets a repository adopt it as its root later.
- **02bd0ec94** (2026-11-24, A-908 steps 1-3) made a holder decide a green's version 0 by that same bit. Before any
  names-log part had joined a family id to its lineage, the holder answered `OUT_OF_ORDER`, counted
  `merge.replica.member_unknown`. The rule assumed a family id meant a repository member. But since 4c38e425e every
  green carries the bit, so a green standing alone waited for a join no names log will ever ship.
  - The arm meant for such a green (`None => Green::new()`) could no longer be reached.
  - A-908 part 4 (7106f83ea) copied the same predicate into a restarted holder's rebuild (`rebuild_through`).

The id cannot tell the two apart, since a green made alone may be adopted as a root later. Only the owner knows, when
it makes the record, whether the green has joined a lineage.

A-908's unit test of this case, `a908_a_version_zero_of_a_green_of_no_repository_is_judged_at_once`, passed throughout.
It used an id of no family (`ObjectId::new`), which no green has been minted with since A-636 part 2b-2. This is
CLAUDE.md's degenerate-parameter gotcha. The daemon's `refused_merge_authority_leaves_the_replica_unchanged` met the
wait in TSan diag 3 and was moved to such an id too.

## First bad commit

Main's A/B on the MacBook (debug, `SLATES_FLEET_TRACE=1`; this probe test alone, A B A B) compared 02bd0ec94 with its
parent 41e00948e, which already carries the probe test and 4c38e425e:

| commit | role | run 1 | run 2 |
|---|---|---|---|
| 41e00948e | parent | good, 9 s | good, 8 s |
| 02bd0ec94 | first bad | bad, 39 s, `client Submit: deadline exceeded` | bad, 35 s, `client Submit: deadline exceeded` |

The record is `~/.local/state/main-01a20182/rounds/ab-02bd/summary.txt`, with the run logs beside it.

## Impact

- In any fleet, a green created alone with `CreateGreen` never committed a version.
- A holder restarted warm never rebuilt such a green's replica.
- Repository greens (roots, branches, a push's creates) were unaffected: their joins are shipped.
- No data was lost. The owner kept every version on its own chain, and its records stayed with it.

## Exact edits (A-932, main's ruling (a) of 2026-11-27)

- `crates/server/src/merge_service.rs`:
  - `MergeRecordValue::lineage`: the repository whose lineage the green had joined when its owner made the record, as
    the owner's catalog records it (`repositories().member`); `None` for a green standing alone.
  - `RECORD_VERSION` goes from 4 to 5. A format-4 record is refused typed (`RecordRefused::Version`), with no second
    decode path.
  - `make_origin_replica` decides version 0 from the record's lineage. With none, the green stands alone and an empty
    green is made at once. With one, the green's membership decides; without a held join it waits as before.
- `crates/server/src/merge_origin.rs`: `rebuild_through` decides the same way, from version 0's record.
- Every `MergeRecordValue` literal names its lineage: content_retention, daemon, takeover, state, and the fleet suite's
  planted record.
- Design: A-932, A-908's text, §4.16 "Apply on holders", and A-636 part 2b-2's note on greens made alone. Also GAPS.

## Verification (MacBook, arm64, debug, 2026-11-27)

- Reds, failing at main a05f07e5f before the fix (log `~/.local/state/w-interop-logs/a932-red.log`):
  - `merge_service::tests::a932_a_green_standing_alone_is_made_at_once_and_a_member_waits_for_its_join`:
    `Err("merge.out_of_order")` where `Ok(())` was expected.
  - `merge_service::tests::a908_a_version_zero_of_a_green_of_no_repository_is_judged_at_once`, now minting its green
    as `found_green` does: `merge.out_of_order` where `merge.recompute_mismatch` was expected.
  - `merge_origin::tests::a932_a_restarted_holder_rebuilds_a_green_standing_alone`: `[Later, Later, Later]`.
- Green after the fix:
  - The focused lib set (`merge_service::`, `merge_origin::`, `green_adopt::`, `takeover::`, `content_retention::`,
    `ledger_read::`, `replica_build::`, `state::tests::` and the two daemon record tests): 86 of 86.
  - New tests: the golden vectors of a member's and a lone green's version 0 at format 5, and format 4 refused typed.
  - Fleet: the probe test passed in 5.82 s. A-908's `a_new_branchs_push_after_a_push_to_main_lands_in_a_fleet` and
    `a908_a_holder_restarted_warm_between_a_branchs_versions_serves_the_branch_after` passed too.
- Owed (main): the whole `tests/fleet.rs` on the Mac and on the Windows lanes. Then the gap that no Unix lane runs the
  fleet suite.

## Sibling (GAPS, OPEN; not fixed here)

A green made alone and later adopted as a repository's root has the opposite problem. Its version 0 shipped standing
alone, so its holders keep a standalone replica, and a later branch of it may be refused `Stale` at `Greens::branch`
there without end. It is recorded in GAPS by main's ruling of 07:08 and is my next piece.
