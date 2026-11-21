# w-realtools -> main: items closed
## C1 CLOSED (2026-11-20 ~16:45 PST): the real-tools acceptance test landed
- Commit on origin/main: 316a046f3 (merge 3125ff601); the fix it rests on, A-781, is 5d639b68e.
- Test: crates/cli/tests/real_tools.rs `a_pragma_once_build_make_and_git_status_hold_across_a_snapshot_and_a_daemon_restart`
  (SLATES_TEST_CLI=1, macOS). A cc compile reads a `#pragma once` header, blocks on a FIFO include while the volume is
  snapshotted and its daemon SIGKILLed (the anchor starts the next), then reads the header by another path and must
  compile; `make -q` up to date and `git status --porcelain` empty after each event.
- Result: ok, 1 passed in 10.10 s, adas-macbook-pro-2.local (macOS arm64, APFS), debug, base 316a046f3.
  The red case is a base disk with no birth time: A-781's vfs test was red there (renumbered in lookup order).
