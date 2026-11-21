# main, from w-work-mount: close notes (A-774)

## C2 multi-span edits through D-O17's minimiser — CLOSED
- Commit on origin/main: 1b0f5679c (main 64bbc241f).
- A changed file through a mount is declared as the whole-file replacement it is; compose_increment refines it with
  A-776's slates_merge::refine, so one save is the edits it made.
- Test: crates/server/tests/mounted_work.rs `one_save_changing_two_lines_does_not_cover_another_editors_line_between`
  (red on 21a904395: editor B Conflict at f5:28; green after). Ten-editor flow keeps its verdicts.

## R3 unsubmitted mount saves survive a daemon restart — CLOSED
- Commit on origin/main: 9d73a77c6 (main 21a904395).
- Recovery rebuilds a mounted work's slot from its published image over its green (mounted_work::rebuild_mounted).
- Test: crates/server/tests/mounted_work.rs `a_save_through_a_mount_survives_a_daemon_restart` (save, stop, start over
  the same anchored segment, submit: red Identical(2) on 5d29b9572; green after).

## P5 lazy base read for mounts and landings — CLOSED
- Commits on origin/main: f6660f0ab (mounts, main 7f8eb5cbb), f401f798c (landings, main 5d29b9572).
- Measurements, release, interleaved 5 runs a side (BENCHMARKS): 8 mounts over a 64 MiB green +547.0-547.1 MB ->
  +0.72-0.82 MB; a mount made 9.4-12.9 ms -> 0.53-0.78 ms; presenting the green's landing +69.2-69.4 MB -> +1.5-1.8 MB.
- Tests: crates/server/tests/mounted_work_cow.rs, crates/server/tests/green_landing_cow.rs.

## Open: C1 (FUSE and WinFsp), R4 (a mount refreshes when its work's base moves) — in work now.
