# w-forming -> main, goal 93 piece (concurrency, Mercurial and Subversion), 2026-11-21
- LANDED 7f1e5a13e: `crates/cli/tests/vcs_concurrency.rs`, two ignored interop tests (`concurrent_stock_hg_pushes_hold`,
  `concurrent_stock_svn_commits_hold`). Each scenario runs against stock `hg serve` / `svnserve` first as the control,
  then slates: hg same-head (exactly one accepted, the rest refused in stock's words), N branches and N bookmarks (all
  accepted, each where its client put it), `hg verify` + heads/bookmarks equal what was accepted; svn same-file (one
  accepted, the rest out of date), N paths (all accepted), revisions without gaps, history replayed into FSFS passes
  `svnadmin verify`. N from SLATES_CONC_N; skips loudly without stock hg 7.2 / svn 1.15.
- Proved so far: compiles with no warning (cargo check -p slates-cli --tests), rustfmt clean. NOT yet run: its focused
  case (N=2, hg same-head + svn same-file). Studio-4 has no stock hg or svn (no installs, CLAUDE.md §2 item 14), and three
  kiln placements of that one case failed on runner d1323898 ("did not answer the sync in 60s").
- Please run the focused case first, then the rounds (commands, platform and what red looks like in run-requests.md):
  `SLATES_CONC_N=2 SLATES_CONC_SCENARIOS=same-head cargo test -p slates-cli --test vcs_concurrency -- --ignored concurrent_stock_hg --nocapture`
  `SLATES_CONC_N=2 SLATES_CONC_SCENARIOS=same-file cargo test -p slates-cli --test vcs_concurrency -- --ignored concurrent_stock_svn --nocapture`
  A failure naming the control (`hg serve (the control)`, `svnserve (the control)`) is the test's expectation being wrong
  for stock: send it back to me and I fix the test.
- Stopped under your correction: my round (kiln r4) and A-827's release A/B (r3), neither started; their commands and
  C3's replay are in run-requests.md.
