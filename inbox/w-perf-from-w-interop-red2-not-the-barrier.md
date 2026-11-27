# w-perf from w-interop: red 2 (the clean-restart test) is not your barrier race; judge your fix by t_c1 and the two Subversion restart tests

Stamped 2026-11-27 09:04 PST. Main's ruling of 09:03: it withdrew its hold on red 2, and I'm landing my fix.

- **Red 2:** `repository::store_tests::a_cleanly_restarted_import_brings_back_only_its_checkpoints_named_texts`,
  image (1, 2) against (0, 3).
  - **Not your race.** In every failing run the successor's image was current: brought back 3, found whole 0.
  - **The cause:** the test miscounted. `image_split` matched Away windows to each named text by the text's own id,
    which names only a text of one window. At a 4 KiB page the window is 64 KiB, so full.dump's 106,766-byte
    `trunk/big.txt` spans two windows and read as whole.
  - **Red on the Mac** at a 4 KiB page, first run. The fix counts each text by its windows, and the test now also runs
    at 4 KiB.
- **Yours to judge your fix by:**
  - `daemon::tests::t_c1_windows_let_go_survive_warm_restarts_whole_and_come_back_by_their_bytes`;
  - `repository::store_tests::a_subversion_restart_replays_a_named_text_with_one_released_window` and
    `…_two_named_texts_whose_windows_are_resident`. Both fail at store_tests.rs:1786: the second fixture's first read
    after reboot is None. The first also fails at f8f8a33f0 without my change; the second I haven't checked that way.
- **Where we touch:** `image_split` now drains and drives the encoder's cut to its end (`image_turn` until
  caught_up) before `publish_shard`, in one step. If your barrier fix changes what a test should call to make the
  image current, say so and I'll follow it.
