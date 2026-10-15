# A landing's durability order was untested, an abandoned run kept its temporaries open, and a paused holder placed after its term

**Found:** 2026-10-10, while measuring why a landing's cost per file was so high (the 2026-10-01 GAPS finding that it
grew faster than its file count) and restructuring its durability into batches (A-130). **Status: fixed** the same day,
in the A-130 change.

## 1. Nothing tested that a landed file is durable before it is visible

- **Symptom:** none observed. §4.15 step 8 orders a file's data sync before its link or exchange, and the engine did
  sync first, but no test would have failed had it not.
- **Cause:** the simulated host recorded each synced file (`synced_files`, drained by `take_synced_files`), and no test
  ever drained it. So the oracle certified every landing without reading the one order a power loss depends on
  (Pillai et al., OSDI 2014: a rename that reaches the disk before its file's data is the classic lost-update).
- **Proof the hole was real:** a scratch mutation that dropped the sync from the engine passed every landing test. With
  the new check, the same mutation fails with 8 names recorded visible before durable.
- **Fix:**
  - The simulated host tracks each landing temporary's durability: dirty from creation and after any write, mode or
    mtime change; written out; durable only once a barrier names it after its write-out.
  - Every placement (`place`, `exchange`, `rename_noreplace`) records a real name that comes to hold an undurable
    temporary: `SimHost::visible_before_durable`.
  - `a_landed_file_is_durable_before_it_is_visible` lands the crash scenario (creates, replacements, removals, renames, a
    file past three windows) on four simulated filesystems, in one call and in slices, and requires none.
  - `a_temporary_placed_before_its_barrier_is_recorded` proves the check can fail in each way a placement can come too
    early: no write-out, a write-out without a barrier, a barrier without a write-out, a write after the write-out.
  - The unread `take_synced_files` is removed.

## 2. An abandoned landing kept its temporaries open

- **Symptom:** a run abandoned while it copied a file past one window (its client went, or the shard stopped) left the
  temporary's descriptor open in the host. The server gives the host back to the volume's slot, so the descriptor
  lived as long as the volume. A-130's staged batches would have left up to a batch of them.
- **Cause:** `LandingRun::abandon` closed the run's directories only. The copy in progress lived in the write phase's
  progress, which the abandon dropped without closing its file.
- **Fix:** `abandon` closes the write phase's open temporaries, the copy and every staged file, never placing them
  (`Landing::release_write`). A named temporary stays at its hidden name for the resume's sweep: the state a crash
  leaves.
- **Test:** `an_abandoned_landing_leaves_no_handle_open` steps a landing until it holds temporaries (a staged batch; a
  large file mid-copy), on filesystems with and without unnamed temporaries, abandons it, and requires every handle
  closed and a later landing to sweep and land. With the release removed, it fails: 4 handles open against 2.

## 3. A paused holder placed an entry after its lease's term

- **Symptom:** `a_holder_paused_past_its_term_starts_no_entry_after_it` expected the entry a paused holder had started
  to land after its term. The fence was checked once, before a small file's whole write. The write ran after the pause,
  its exchange included.
- **Cause:** the fence was per entry start. A large file whose copy outlived the term was already refused at its
  placement (`write_one`'s "a long copy can outlive them"), so the rule differed by file size.
- **Fix:** the fence is checked at the batch's barrier and at each placement. A file staged under the term but placed
  after it is removed unplaced and `Skipped(LeaseEnded)`. No visible write follows the term, whatever the file's size.
  §4.15 step 4 and its failure row say so (A-130). The test is restated as
  `a_holder_paused_past_its_term_places_nothing_after_it`: both entries skipped and old, no temporary left, the resume
  lands both.

## The measurement that led here

- A landing made every file durable alone before placing it: Linux `fdatasync`, macOS `F_BARRIERFSYNC`.
- On this machine's engine bench (`land_bench flat`, 1,000 to 16,000 new files):
  - over the simulated host, 3.7–4.4 µs an entry at every size;
  - onto APFS, 697–904 µs with the barrier and 326–384 µs with it stubbed in a scratch copy;
  - onto ext4 in Docker, 573–913 µs with `fdatasync` and 15–21 µs without.

  Per-entry cost is flat in the file count. The 2026-10-01 "15× for 2× files" was the per-file sync's variance on a
  shared disk, not growth.
- A-130 batches the durability:
  - macOS writes each file out with `fsync` and takes one `F_BARRIERFSYNC` per batch;
  - Linux keeps `fdatasync` per file, because `syncfs`, its only batch-wide call, flushes every other writer's data
    (measured in `BENCHMARKS.md`).

## Sibling sweep

- **Other seam calls recorded and never read: one, fixed in the same change.** `synced_dirs`, drained by
  `take_synced_dirs`, was read by no test either.
  - The durability tests fail directory syncs on purpose and check that the entries are held, so a failed sync was
    covered. An engine that never called `sync_dir` for some directory holding a placed entry would still have
    passed.
  - Now the simulated host tracks each directory changed since its last sync (`SimHost::unsynced_dir`).
    `every_landed_entry_has_its_directory_synced_before_it_advances` requires, after a `Done` landing of the crash
    scenario, every entry's directory still standing, and a rename's source directory, to be synced (§4.15 step 9).
  - A scratch mutation that skipped the root's sync fails it: 13 entries in `/` flagged.
  - `take_synced_dirs` is removed.
- **Other places a run holds a host resource across slices:** only the write phase's temporaries. Directories are
  closed by `close_dirs`, already called by `abandon`.
- **Other fences checked once for multi-unit work:** the sweep and validate phases write nothing. The directory syncs
  after the write phase are not fenced; they make durable what was placed under the term.
