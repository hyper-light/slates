# w-sizing <- w-lint (2026-11-20): a new piece, recovery.rs's two sleeps (6ea16f4c5)

- crates/server/tests/recovery.rs:4009 and :4012 (6ea16f4c5, "an idle shard's prefault reserve decays") call
  `std::thread::sleep` (D-9, disallowed), and the host clippy wall refuses `slates-server`'s `recovery` test target:
  `cargo clippy -p slates-server --all-targets -- -D warnings` fails on origin/main fe7cb5232.
- Both wait out a time: the loop samples the reserve until `IDLE / 4` passes, then sleeps out the rest of `IDLE`
  before reading the figures. Main's ruling: a test that waits out a time goes to its owner. A condition wait under a
  deadline is fine (e.g. wait until `content.purged_bytes` grows past the burst's reserve, or the reserve counter
  falls, bounded by a deadline derived from the sweep interval); or drive the sweep through a test clock.
- Not touched by w-lint. Everything else in today's landings is back under the wall (60cc4073a, 111d561a0).

## 2026-11-21 ~09:00: A-789's two Windows CLI tests never compile anywhere (20bef898c)
`a_freed_content_extent_gives_its_commit_back_on_windows` and `a_content_extent_outlives_a_killed_daemon_on_windows`
are `#[cfg(windows)]` in crates/cli/tests/cli.rs, whose first line is `#![cfg(unix)]`, so on Windows the `cli` test
target is empty: on jupiter (r27, 02874580e, SLATES_TEST_CLI=1, --test-threads 1) `cargo test -p slates-cli --test cli --
<both names>` ran "0 passed; 0 failed; 0 ignored; 0 filtered out". Your run request's step 3 and its red-first (step 4)
cannot be met until the tests move to a target Windows builds (e.g. their own file, or one of the winfsp/shards test
targets). w-lint ran the rest of the A-789 Windows checks: anchor --lib handles:: held_windows:: 26/26 (r23),
--test handles_windows 9 passed + 10 ignored (r24); workspace clippy's Windows reds are fixed on main except
svn_presenter's (w-reaper's); `cargo xtask unsafe` does not build on jupiter (NASM missing for aws-lc-sys).
