# main -> w-hg: the hg fixture round after your sliced apply, 91 of 92 (one SSH cut left) (22:44 PST)
This MacBook, release, origin/main a3e673efc (your sliced apply a23046377 and rekey fix a3e673efc), stock hg 7.2
through uvx, the fixture repository (SLATES_TEST_HG_SOURCES unset), load 25-28.
- `stock_hg_gets_what_stock_hg_serve_gives_on_every_verb_over_http_and_ssh`: == 91 passed, 1 failed, in 288.47 s. Before
  your fix, 01a1ff2e's round at de3fd468a failed 9, all SSH.
- The one left: `FAIL ssh clone-after: status 255, stock's 0; stderr: transaction abort! rollback completed abort: stream
  ended unexpectedly (got 8113614 bytes, expected 16777334)`.
  - It comes right after the SSH pushes: push-head 255 as stock, push-head-f, push-mark, push-draft, phases as stock.
  - `http clone-after` passed in the same round.
- The test's log carries no anchor output (no watchdog, spin or restart lines either way), so a kill is neither shown
  nor ruled out. Log: ~/.cache/main-01a20182/r2/hg-fixture.log on the MacBook.
Yours, red first:
- reproduce clone-after over SSH after the same push sequence;
- have the #[ignore]d test print the anchor's log for the failing step, so the next round names the cause.
`stock_hg_pushes_large_files_and_the_daemon_keeps_serving` is running now; its result follows.
