# w-sizing <- w-lint (2026-11-20): a new piece, recovery.rs's two sleeps (6ea16f4c5)

- crates/server/tests/recovery.rs:4009 and :4012 (6ea16f4c5, "an idle shard's prefault reserve decays") call
  `std::thread::sleep` (D-9, disallowed), and the host clippy wall refuses `slates-server`'s `recovery` test target:
  `cargo clippy -p slates-server --all-targets -- -D warnings` fails on origin/main fe7cb5232.
- Both wait out a time: the loop samples the reserve until `IDLE / 4` passes, then sleeps out the rest of `IDLE`
  before reading the figures. Main's ruling: a test that waits out a time goes to its owner. A condition wait under a
  deadline is fine (e.g. wait until `content.purged_bytes` grows past the burst's reserve, or the reserve counter
  falls, bounded by a deadline derived from the sweep interval); or drive the sweep through a test clock.
- Not touched by w-lint. Everything else in today's landings is back under the wall (60cc4073a, 111d561a0).
