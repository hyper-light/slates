# A lone green adopted as a root left its branches unreplicated

- **Date:** found 2026-11-27 by reading the code while fixing A-932 (GAPS row 2d3a8cfe2), confirmed red the same day,
  and fixed the same day.
- **Status:** fixed.
- **Subsystem:** a holder's replicas of a repository's lineage (§4.16 "Apply on holders", A-908). The code is
  `crates/server/src/merge_service.rs` (`make_origin_replica`, `adopted_root_founded`) and
  `crates/server/src/merge_origin.rs` (`rebuild_branch`).
- **Severity:** in a fleet, a branch of a repository whose root was a green made alone never reached any holder, so
  its pushes could not commit. Its records were retried without end (CLAUDE.md §2 item 8).

## Root cause

A green made alone is a family of its own (A-636 part 2b-2), and a repository may adopt it as its root
(`repository::install_root` with an adopted green). The holders and the owner then disagree:

- **The holders:** the green's version 0 was shipped while it stood alone, naming no lineage (A-932), so each holder
  made its replica standing alone.
- **The owner:** the adoption moves the owner's engine into the new lineage and enqueues no new version 0, so no
  holder ever founds the lineage.

A branch's version 0 then reaches `Greens::branch` on each holder. That refuses it `Stale`, because its source is no
lineage's member there. The refusal is counted `merge.replica.branch_source_behind` and retried by the sender without
end.

A holder restarted warm rebuilt the root from its version 0, standing alone, and refused the branch the same way.

## Exact edits

- `merge_service::adopted_root_founded`: where a branch's source stands alone here and the names log held here joins it
  as its repository's root, the lone replica founds that lineage (`Greens::found`, its name holds moved with it), counted
  `merge.replica.adopted_root_founded`. A lineage the repository already has refuses it; that is counted
  `merge.replica.lineage_found_refused`, a bug signal, and the replica stays as it was.
- It is called where a branch is made from its source: a record's version 0 (`make_origin_replica`) and a rebuild
  (`merge_origin::rebuild_branch`).

## Verification (MacBook, debug, 2026-11-27)

- Reds before the fix:
  - `merge_service::tests::a_lone_green_adopted_as_a_root_is_branched_on_its_holder`: `Err("merge.out_of_order")`;
  - `merge_origin::tests::a_restarted_holder_rebuilds_a_branch_of_a_lone_green_adopted_as_a_root`:
    `[Later, Later, Later, Later]`.
- After the fix: both pass, and the focused set (`merge_service::`, `merge_origin::`, `green_adopt::`,
  `takeover::`, `ledger_read::`, `replica_build::`) passed 79 of 79.
- Not covered: a takeover successor's adoption of such a root before any branch of it reached it (`green_adopt`,
  w-shared's A-918 B). Main's fleet run decides whether that needs a test.
