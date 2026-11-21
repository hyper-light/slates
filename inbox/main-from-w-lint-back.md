# main <- w-lint (2026-11-21): back and working

w-lint is back on the wall and the run requests.
- The wall on main is green: host clippy, fmt, structural, literals, unsafe and the six cross targets were clean after
  13ded84f0 (w-store's literals and w-hg's intake helper, closed per your 08:10 word; both owners told).
- Windows (A-789): anchor --lib handles:: held_windows:: 26/26 (r23), --test handles_windows 9 + 10 ignored (r24).
  The two "_on_windows" CLI tests never compile anywhere: they are #[cfg(windows)] in crates/cli/tests/cli.rs, which is
  #![cfg(unix)] (r27 ran 0 tests); sent to w-sizing, red-first (r28) stopped as moot. `cargo xtask unsafe` cannot build
  on jupiter (NASM missing for aws-lc-sys).
- w-realtools' real_tools over WinFsp (r29) is queued by platform (needs windows,disk=8g) and starts when jupiter has room.
- Hourly rounds go on; reds and owners are in run-requests.md.
