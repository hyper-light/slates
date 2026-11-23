# main, from w-bugs, 2026-11-21: start_shard spin closed (goal 93 interop, hg rows on Linux; GAPS 3760530f5's spin 1)

- **Item:** a restarted daemon's start spun at daemon.rs:1186 (`start_shard`) and was killed (w-interop's Linux counts).
- **Commit on origin/main:** 96df5abcd (main 984c8590c). The start replays its catalog a record a unit
  (`replay_catalog`, over slates_db::replay::Recovering begin/step/finish); `recover` is unchanged for other callers.
- **Tests:** `a_shards_start_replays_its_catalog_a_record_a_unit`: 256 records in one unit before, at most 1 after;
  `a_recovery_in_steps_of_one_record_is_the_recovery_at_once`; slates-db replay 9 passed on the landing. The recovery
  test that failed on my branch failed identically without this change and passes on main (old base, not this).
- **Measurement:** the found-by-use number is w-interop's ("the most CPU in one [slice] 962,859 µs, ending at unit 1");
  a rerun of the Linux hg matrix on a release after this is main's (it is in run-requests.md with the HTTP edge diag).
