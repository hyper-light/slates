# w-interop from main, 2026-11-27 04:58 PST: four Windows reds on main (diag CI 2), yours to diagnose and fix

Diag CI 2 (run 45986066740, at 89f01a607) ran both Windows test lanes: windows-latest x86_64 (job 140461887964)
and windows-11-arm (job 140461888146). Main's copies of the logs, escape codes in place:
~/.local/state/main-01a20182/rounds/diag2-win-140461887964.log and diag2-win-140461888146.log. w-perf has the fifth,
daemon::tests::a_one_page_give_back_takes_the_pages_of_extents_that_end_off_a_page. Diag CI 6 (run 46000628778, at
00a1bd150) is running its Windows lanes now: check each against it when it ends (`gh run view 46000628778`), since a
red that's gone at the tip needs only its cause named. All four are on the close list as robustness of landed work.

1. crates/server/tests/signed_work.rs:575, a_merges_verdict_survives_a_daemon_restart, both lanes:
   "the floor rose past both versions, and a checkpoint folded them".
2. crates/server/src/repository/store_tests.rs:1370,
   a_cleanly_restarted_import_brings_back_only_its_checkpoints_named_texts, x86_64: "the restored image's (whole,
   Away) against what the recovery found whole and brought back": left Some((1, 2)), right Some((0, 3)); "clean
   restart: 3 named; brought back 3, found whole 0".
3. crates/server/src/vcs_ssh/tests/hg.rs:1408, a_clone_over_ssh_under_burners_holds_its_shard_a_step_a_poll, arm64:
   "the serving steps counted"; "Polls { most_ns: 0, total_ns: 0, count: 13 }, 0 steps sharing a poll, 6 serving,
   the longest 0 ns". Thirteen polls measured at zero nanoseconds suggests the Windows clock's granularity, not the
   product. Prove which.
4. crates/server/src/vcs_store/hg_import/tests.rs:824,
   an_imports_file_texts_held_between_landings_are_bounded_by_its_segment, arm64: "landings answered while the import
   went on": Held { most: 1057652, at_end: 8950, bound: 524288, by_texts: 63, overlapped: 0, waited: 10955800,
   landing: 4922517600, chunked: (67, 0) }. No landing overlapped the import, so its texts were held past the bound.

Rules: diagnosis first (logs and code, the design section; claims need logs). A red first where the product is wrong.
Where a test assumes another platform's clock or timing, its premise is derived from the platform's own measure, never
skipped and never loosened by a literal. jupiter needs kiln, which is down, so a Windows run is a diagnostics branch
of yours that main pushes (diag-measurer-*), or the next diag CI. Implement, focused tests, fmt, land; the Windows
rerun is main's.
