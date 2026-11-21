# main, from w-work-cow: close notes (2026-11-20)

## C1 a Work costs its divergence: a first edit copies only what it wrote -- CLOSED
- Commit on origin/main: 0c5dd0cb5 (landed via af045255b), A-790. Finer than chunk grain: a changed file is pieces over
  the base; a first edit holds the bytes written plus three pieces, never a chunk of the base.
- Test: crates/server/tests/work_edit_cow.rs -- red on main before (8 one-byte edits of a 64 MiB file: +201,490,432 B
  resident, 2.38 ms each); green +18,304 B live heap (2,288 B a work), bound 8 x 64 KiB + 8 MiB.
- Measurement: BENCHMARKS "A work's first edit as pieces over its base against a whole-file copy (A-790)": live heap
  a work 1.05 MB / 16.8 MB / 268 MB (A) vs 2.6-2.9 KB (B) at 1 / 16 / 256 MiB; best mean 261/455/5150 us vs 113/132/173.

## P2 the brief's deciding measurement over the grid -- CLOSED
- Commit on origin/main: 5aef5f02f (landed via f9c113ae9).
- Measurement: BENCHMARKS "A work over a green costs its divergence: the brief's grid, before and after (w-work-cow
  P2)": green 1 MiB / 32 MiB / 1 GiB x files 1k / 10k / 100k x works 1 / 10 / 100, three rounds, release, before and
  after interleaved, 135 runs all shown. After: 1.6-5.6 KB of live heap a work at every point, flat in green size and
  works (O(N x metadata)). Before (a copy of the green per work, run where N x G <= 2 GiB): 1.5 MB to 1.25 GB a work,
  time to 129 ms. Test: crates/server/tests/work_cow.rs (bounds live heap and resident).
- Finding (GAPS, not worked): one submit of a 1 GiB or 100k-file changeset holds its shard 1,097 ms past its
  1,025 ms window; the watchdog ends the daemon. The grid seeds in parts of 16 MiB / 5,000 files.

## Codex goal (Green/Work + atomic-conflict work, Codex 01a1bdab on adacachy) -- CLOSED
Inventory: 476 branches (work/slates-*, experiment/*, codex/*) in ~/Projects/slates, none on origin; 1,387 commits not on
main cc29bfc4c, 6,394 behind. Read only (git -C reads; a --shared bare copy under my ~/.cache). 94 carry Green/Work or
conflict work; 382 are VFS/DAX/FUSE/catalog/reclaim/runtime, outside the goal, untouched. Full list with tips: GAPS
"the Codex agent's Green/Work and atomic-conflict branches against main", landed d8416294c.

| strand (branches) | verdict | evidence on main |
|---|---|---|
| conflict check + head-conditional resolution: work/slates-atomic-conflicts 6420b7ee7, conflict-cli 0d5ab5141, conflict-mutants 65472f43a (3) | **LANDED** as A-796: 6823fde6f (via 68ce3d585) server/client/protocol; c1e76d0a7 (via d2ad10da4) MCP tools + CLI verbs | `crates/server/tests/merge_check.rs` (1 and 8 shards, same answers; red: dropping the stamped-head check let a moved head's resolution through); `crates/mcp/tests/mcp.rs` assert_merge_check_resolves_over_mcp; CLI the_grammar_parses_the_merge_verbs |
| owned merge verdict / verdict-* / runtime-step-verdict / server-root-publication (7) | superseded | A-257 + A-782: `green_checkpoint.rs` (0782fdb0c), `recovery.rs` |
| Green publication / preparation / physical fill / prepared head / source issuer / capture & pin release / snapshot-view / trusted & held publication / accepted content (38) | superseded | A-201 `checkout.rs` merged_pushes_are_checked_out_and_land_on_the_servers_disk (11468ac0d); A-774 p2/p3 `green_landing_cow.rs` (1c3734ca7), `mounted_work.rs` ten_editors_on_mounted_works_submit_and_a_grant_lands_the_green (a2415c930) |
| namespace fill / directory recreation / pure rename (10) | superseded | A-612/A-631/A-643/A-646 `replaced_node_tests.rs` a_directory_made_again_after_a_works_base_is_not_removed (aa9de3fb4); `green_post_state.rs` renames (26c0eff4f) |
| Work declarations / journal admission / mounted-work authority & slots / mounted early conflict / submitted source / version binding (14) | superseded | A-247 `works_off_owner.rs` (4f62cbe52); A-790 `work_edit_cow.rs` (0c5dd0cb5); A-774/A-776 `mounted_work.rs` one_save_changing_two_lines... (a2415c930) |
| perf experiments on the Codex structures (cursors, borrowed catalog, owner row, lookups) (16) | superseded (they tune structures main replaced) | rows above |
| nfs-work-lease-holder-phased-v4 4bf73859d (1) | finding, not worked (test-only: an old Work mount writing under another holder's lease) | GAPS |
| reclaim-* / holder-reentrant-drop-green (5) | outside the goal's scope | -- |
None of what lands duplicates A-255, A-257 or A-263-A-267.

Landed for the goal: d8416294c (GAPS record of all 94 branches), 6823fde6f/68ce3d585 (A-796 check + resolution),
c1e76d0a7/d2ad10da4 (MCP slates.merge.check/read_check/resolve, CLI slates check/resolve). Pushed as merges onto main
through studio-3, never as agents/codex-* branches (nothing of the Codex trees was pushed: the port is new commits on
main). Not ported, recorded in GAPS: the Codex rebase_with refinement (a kept op conflicting only on a path the
replacement re-establishes answers the windows instead). A-255, A-257, A-263-A-267 untouched (A-267's kept verdict reused).

### Codex strand finished (main's 22:54 routing: CLI/MCP adapters, rebase_with, conflict-mutants' tests) + the wire red
- Wire red (A-796's tags unpublished, slates-ipc --test wire_order): FIXED db80da5e8 (CheckMerge 98, ReadMergeCheck 99,
  ResolveMerge 100 after A-795's RemovedCreateOverS3 97; MergeChecked, StaleMergeView; MergeView and WorkChange held).
  Test: slates-ipc --test wire_order 2/2.
- CLI/MCP adapters: c1e76d0a7 (via d2ad10da4). Test: mcp the_mcp_surface_serves_the_tools; cli the_grammar_parses_the_merge_verbs.
- rebase_with (AC-6.13): 4930bf14b (via 544020bd2), Green::rebase_establishing. Test: merge_check
  a_kept_mode_on_a_file_the_resolution_writes_anew_is_not_a_conflict (red: Conflict(f, DeleteModify)).
- conflict-mutants: its restart finding was a real hole (a stamp from before a restart named the work after one more edit,
  red Ok(Rebased(0))); fixed in 4930bf14b by seeding a work's revision from its partition's next_seq(). Test: merge_check
  a_stamp_from_before_a_restart_never_names_the_work_after_it_changed. Its racing-resolvers and stale-page cases are held
  by merge_check's moved-head refusal and verdict_pages' replaced-verdict refusal (A-267).
- Note: 544020bd2 is studio-3's merge of 4930bf14b over a main that had moved again (an auto-merged verbs.rs change I did
  not re-test after). Focused on my tree after merging e14493d62: merge_check 3/3, works_off_owner 16/16.
- Not mine: vcs_store::paths::tests::t_a664_* (submit refused Overloaded) fails at d8416294c, before A-796 (w-fed-faults' red).
