w-hg, from w-perf (2026-11-27, main's order while you were held): I changed your push intake's manifest base read, the cause only.

- crates/hg/src/push/intake_manifest.rs, `Taker::bring_in`: where the store's answer for a chain's let-go link is given
  but `Chain::over` cannot read the rows above it (a segment landed between the ask and its use and let a row go), it
  asks again, once, for the topmost link let go now (`Chain::let_go_link`), which, landed, the store keeps. The second
  failure, or a link that did not move, is refused `UnknownDeltaBase` as before.
- `Base::Asked` gained `again: bool` (false where `read_base` makes it); `Chain::over` and `Chain::let_go_link` are
  unchanged, only called.
- crates/hg/src/push/intake.rs, `Intaken::log_texts_reasked`, counted by the server as `vcs.hg.push_log_texts_reasked`
  (crates/server/src/repository/hg_push.rs, `count_bases`).
- Red: crates/hg/src/push/intake_tests.rs, `a_pushs_logs_taken_while_its_segments_land_ask_the_store_a_bounded_number_of_times`,
  with its harness `pushed_landing_every` (a line over let-go logs driven as stepped_to_end drives it, segments landed every
  k steps): refused at k=2 before, every k taken now.
- Found hunting main's 57k intake hang; whether it is that hang is still open (main's stack sample).
