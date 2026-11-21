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

## C1 a Work mounts and works through FUSE and WinFsp too — CLOSED (code); Linux/Windows runs in run-requests
- Commit on origin/main: 77fcae73a (main 1527ecfe0). WinFsp's mount turn lends the green to a mounted work's base, as
  NFS and FUSE do; attach makes a work's slot for every form.
- Tests written: crates/server/tests/fuse_mount.rs `a_work_mounted_through_fuse_saves_and_its_submit_lands_the_save`
  (Linux, gated on fusermount3 + /dev/fuse) and crates/cli/tests/winfsp.rs
  `a_work_mounted_through_winfsp_saves_and_its_submit_lands_the_save` (Windows, live WinFsp). Neither runs on macOS;
  the run commands are in run-requests.md (2026-11-20, w-work-mount C1/R4). winfsp.rs is cfg(windows): not compiled here.

## R4 a mount refreshes when its Work's base moves — CLOSED
- Commit on origin/main: 60a71db8a (main 56e44c7f8).
- Test: crates/server/tests/mounted_work.rs `a_mount_shows_its_works_new_base_once_the_base_moves` (red on 1527ecfe0:
  another work's accepted f1 unseen through the mount after its own accept; green after).

All five items (C1 C2 R3 R4 P5) are closed on origin/main.

## C1 Windows runs (2026-11-21, w-work-mount's own, on jupiter)
- Windows build of crates/server/src/winfsp.rs: GREEN. `kiln run --on jupiter --no-sync` (the --needs windows sync of
  the 585 MB tree timed out twice: "did not answer the sync in 60s"), in a clone of origin/main e4cfbd38d:
  `cargo build --locked -p slates-server` exit 0 in 31 min 47 s. One warning from C1: `use slates_vfs::host::HostFs`
  unused in winfsp.rs; fix 0cbef8441 on branch agents/w-work-mount-win, landing once jupiter builds it clean.
- `cargo test -p slates-cli --test winfsp a_work_mounted_through_winfsp`: NOT YET RUN. Its clone on jupiter vanished
  between runs (~/Projects cleanup), a re-clone hit a GitHub disconnect, and the retry (detached, 06:05 UTC) is
  unconfirmed: since 08:07 UTC the kiln server answers no `kiln run` ("the kiln server did not answer in time").
  The kiln CLI prints no run id; the runner is f766f09294f2e17e3b8a631ffd4e6a4b (jupiter).
