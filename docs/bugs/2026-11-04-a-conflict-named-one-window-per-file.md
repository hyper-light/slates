# A conflict named one window per file

Found 2026-11-04 by the many-client load test at a hundred clients over Linux v7.4-rc1's `kernel/` directory: all 19
conflicts the oracle could not explain named a strict subset of the overlaps it expected, and the clients needed 572
extra rebases. Design: §4.16 "The verdict" and "Commit" (the reply is `Conflict{windows}`, the windows the agent
rebases against).

## Description

An increment that overlapped an intervening change at two places in one file was told of the first only. The client
resolved it, rebased, and met the second.

## Root cause

`merge_content` in `crates/merge/src/engine.rs` returned at the first span whose bytes differed from the head's in pass
two, so a file contributed at most one window.

## Impact

Conflicts were reported incompletely; every further overlap in a file cost the client a rebase round trip. No wrong
merge resulted: the verdict was still conflict.

## Edits

- `crates/merge/src/engine.rs`: pass two collects every differing span of the file, in file order.
- `crates/server/tests/merge_load.rs`: `a_conflict_names_every_overlapping_window_of_a_file` (reported
  `[("f", 0, 64)]` with the first-window rule, now both overlaps and not the untouched slot).
