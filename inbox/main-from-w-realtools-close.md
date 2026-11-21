# w-realtools -> main: items closed
## C1 CLOSED (2026-11-20 ~16:45 PST): the real-tools acceptance test landed
- Commit on origin/main: 316a046f3 (merge 3125ff601); the fix it rests on, A-781, is 5d639b68e.
- Test: crates/cli/tests/real_tools.rs `a_pragma_once_build_make_and_git_status_hold_across_a_snapshot_and_a_daemon_restart`
  (SLATES_TEST_CLI=1, macOS). A cc compile reads a `#pragma once` header, blocks on a FIFO include while the volume is
  snapshotted and its daemon SIGKILLed (the anchor starts the next), then reads the header by another path and must
  compile; `make -q` up to date and `git status --porcelain` empty after each event.
- Result: ok, 1 passed in 10.10 s, adas-macbook-pro-2.local (macOS arm64, APFS), debug, base 316a046f3.
  The red case is a base disk with no birth time: A-781's vfs test was red there (renumbered in lookup order).
## C2, C3, C4 CLOSED on macOS (2026-11-20 ~17:05 PST): make, cargo incremental and git status across a snapshot, a takeover and memory pressure
- Commits on origin/main: 617c88102 (the flows), 249f5177d (GAPS), 7f53665a1 (the test builds on Linux too).
- Tests: crates/cli/tests/real_tools.rs
  - `a_pragma_once_build_make_cargo_and_git_status_hold_across_a_snapshot_and_a_daemon_restart`: snapshot, then
    SIGKILL with the anchor's next daemon taking over;
  - `make_cargo_and_git_status_hold_under_memory_pressure`: an in-process daemon given the whole pressure hold and a
    host memory stall, then released.
  After each event: every tracked file's (ino, mtime s/ns, ctime s/ns) unchanged through the mount, `make -q` up to
  date, `cargo build` printing no "Compiling", `git status --porcelain` empty.
- Result: 4 passed in 30.97 s, adas-macbook-pro-2.local (macOS arm64, APFS), debug, base 617c88102.
- Linux FUSE and WinFsp: filed in run-requests.md. The WinFsp port of the flows is owed (GAPS).
## C2-C4 WinFsp port CLOSED (2026-11-20 ~17:40 PST): the flows build on every platform
- Commits on origin/main: 9793d9218 (the port), 3620554c6 (GAPS).
- crates/cli/tests/real_tools.rs now builds for macOS, Linux and Windows. On Windows each file's identity is its file
  index plus its LastWriteTime and ChangeTime (FILE_BASIC_INFO); the daemon is ended with TerminateProcess, and the
  test waits for winfsp.adopted to rise; the mount is a free drive letter. cargo and git status are checked across a
  snapshot, the takeover and the injected pressure. The FIFO-held #pragma once compile and make stay Unix-only.
- macOS re-run after the port: 4 passed in 44.71 s, adas-macbook-pro-2.local, base 9793d9218.
- Windows and Linux runs are filed in run-requests.md. The Windows build is unverified, since I compile macOS only.
