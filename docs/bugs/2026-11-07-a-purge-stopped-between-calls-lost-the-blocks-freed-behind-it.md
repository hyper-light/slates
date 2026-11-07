# A purge that stopped between calls lost the blocks freed behind it (2026-11-07)

## Description

Main CI 42184319025 (76e5b502), "native clippy + tests (windows)": A-205's give-back test
(`crates/server/tests/give_back.rs`) counted 118,337,536 of 134,217,728 deleted bytes given back, under its 90 % bar,
though the working set fell 97 %. It was intermittent: on main with only a log added it failed 3 of 5 runs on each CI
Windows runner (`diag-liveness-4`).

## Root cause, from the log

Each reap tick's arena state was logged (`DIAG-GB`, `diag-liveness-4` to `-6`). A failing run on windows-11-arm
(`diag-liveness-5`, run 4) shows:

- An idle tick fell between the writes and the deletes, so the write burst closed at 134,217,728 bytes and A-214's
  reserve kept that much. The purge walked the reserve's kept runs at every idle tick, so a walk was nearly always
  under way.
- The deletes released one 4 MiB file per idle tick (`allocated` 134,217,728 → 130,023,424 → … → 0), each while a walk
  was stopped between calls. Blocks released behind the walk's cursor were recorded in their region's set but never
  visited by that walk.
- `ChunkArena::purge_keeping` ended a walk by setting the resident free count to what that walk had kept. That dropped
  the blocks behind the cursor from the count. The next walk subtracted them again as it drained them, so the count
  saturated at zero mid-walk (`purge: 6291456 bytes, more true, resident_free 0`).
- The purge's first line returned at once whenever the count was zero, mid-walk included. So the walk never finished,
  and every later purge returned at once (`0 bytes, more false, resident_free 0`). 22,368,256 deleted bytes stayed
  resident and uncounted: the daemon counted 111,849,472 of 134,217,728.

The same loss reproduces in the arena alone: two freed blocks, a purge of one block a call, a third block freed behind
the cursor. The walk gave back 32,768 of 49,152 bytes and then nothing more
(`a_block_freed_behind_an_unfinished_purge_still_goes_back`).

A-214 did not create the fault but exposed it: before the reserve, a walk visited only blocks freed since the last one
and was rarely stopped between calls when a release came.

## Impact

Deleted files' pages could stay resident and uncounted on any platform, for as long as no later release restarted a
walk. On Windows A-205's test caught it; Linux and macOS ran the same code.

## Exact edits

- `crates/mem/src/arena.rs` `purge_keeping`:
  - a walk's end sets the resident free count to what the regions' sets record, which holds anything freed or
    faulted behind the cursor;
  - the purge has nothing to do only when no walk is under way and no set records a unit, rather than when the count
    reads zero.
- The field `purge_kept`, which only fed the old end, is gone. `UnitSet::recorded_bytes` is no longer test-only.
- The failing test `a_block_freed_behind_an_unfinished_purge_still_goes_back`.

## Verified (2026-11-07)

- The arena test fails on main (32,768 of 49,152 bytes) and passes with the fix; the mem suite passes on macOS and
  both Linux runners, and the purge tests under Miri.
- A-205's give-back, five runs on each Windows runner with the fix (`diag-liveness-6`): 10 of 10 passed, against 6 of
  10 failures on main (`diag-liveness-4`). Round 1, where the burst closes before the delete, counted 99–103 % of the
  deleted bytes on x86_64 and 102–124 % on arm64.
- A-214's recovery tests on both Linux runners, five runs each, all passed, the reserve's own
  `a_burst_after_idle_lands_on_pages_the_idle_shard_prefaulted` included.
