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

## Codex goal (Green/Work + atomic-conflict work, Codex 01a1bdab on adacachy) -- IN FLIGHT (classification landed)
Inventory: 476 branches (work/slates-*, experiment/*, codex/*) in ~/Projects/slates, none on origin; 1,387 commits not on
main cc29bfc4c, 6,394 behind. Read only (git -C reads; a --shared bare copy under my ~/.cache). 94 carry Green/Work or
conflict work; 382 are VFS/DAX/FUSE/catalog/reclaim/runtime, outside the goal, untouched. Full list with tips: GAPS
"the Codex agent's Green/Work and atomic-conflict branches against main", landed d8416294c.

| strand (branches) | verdict | evidence on main |
|---|---|---|
| conflict check + head-conditional resolution: work/slates-atomic-conflicts 6420b7ee7, conflict-cli 0d5ab5141, conflict-mutants 65472f43a (3) | **LAND**: main has no non-publishing check and no resolution bound to the observed head; port in flight, red first (`CheckMerge`, `ReadMergeCheck`, `ResolveMerge` over main's rebase, A-267's kept-verdict paging reused, not duplicated) | -- |
| owned merge verdict / verdict-* / runtime-step-verdict / server-root-publication (7) | superseded | A-257 + A-782: `green_checkpoint.rs` (0782fdb0c), `recovery.rs` |
| Green publication / preparation / physical fill / prepared head / source issuer / capture & pin release / snapshot-view / trusted & held publication / accepted content (38) | superseded | A-201 `checkout.rs` merged_pushes_are_checked_out_and_land_on_the_servers_disk (11468ac0d); A-774 p2/p3 `green_landing_cow.rs` (1c3734ca7), `mounted_work.rs` ten_editors_on_mounted_works_submit_and_a_grant_lands_the_green (a2415c930) |
| namespace fill / directory recreation / pure rename (10) | superseded | A-612/A-631/A-643/A-646 `replaced_node_tests.rs` a_directory_made_again_after_a_works_base_is_not_removed (aa9de3fb4); `green_post_state.rs` renames (26c0eff4f) |
| Work declarations / journal admission / mounted-work authority & slots / mounted early conflict / submitted source / version binding (14) | superseded | A-247 `works_off_owner.rs` (4f62cbe52); A-790 `work_edit_cow.rs` (0c5dd0cb5); A-774/A-776 `mounted_work.rs` one_save_changing_two_lines... (a2415c930) |
| perf experiments on the Codex structures (cursors, borrowed catalog, owner row, lookups) (16) | superseded (they tune structures main replaced) | rows above |
| nfs-work-lease-holder-phased-v4 4bf73859d (1) | finding, not worked (test-only: an old Work mount writing under another holder's lease) | GAPS |
| reclaim-* / holder-reentrant-drop-green (5) | outside the goal's scope | -- |
None of what lands duplicates A-255, A-257 or A-263-A-267.
