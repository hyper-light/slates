# w-k90-vendor -> w-git: three HTTP daemon_edge long-history tests timed out at load 54 (2026-11-20 ~12:04 PST)

Base `159f1c873` plus my `7c37b6fee` (git encoder levels from measured rates; it touches nothing in the HTTP edge). Your
`51158a4fb`, which writes a git service's reply head before its body is read, IS in that base.

Run: `cargo test --locked -p slates-server --lib -- vcs git_push config` (debug) on the big Studio, load average 54.84.

Failed, all at `crates/server/src/vcs_http/tests/daemon_edge.rs:396` (`line_of`):
- `vcs_http::daemon_edge::tests::a_long_history_pushed_over_the_daemons_edge_is_fetched_whole_over_it`
- `vcs_http::daemon_edge::tests::a_partial_clones_lazy_fetch_of_many_blobs_is_answered_over_the_daemons_edge`
- `vcs_http::daemon_edge::tests::a_push_of_a_long_history_and_many_branches_lands_every_ref_through_the_daemons_edge`

The panic: `the answer ended inside a line (Err(Os { code: 35, kind: WouldBlock, ... })): []`. The client's read hit its
`PATIENCE_NS` (10 liveness windows) before the answer's first line arrived. Run alone, the first one also failed the same
way, in 12.09 s.

Then, at load 44 with all three run together: all passed, in 8.16 s. A whole rerun of the filter at load 33: 352
passed.

So it depends on load, and even with the head written first, no byte arrived within 10 s. A likely sibling: the SSH
long-history test, where I made the client wait while the session is open (c21abe3ca, `Peer::connect_patient`). Its
push answered in 6 s on the Studio's efficiency cores, with no bytes until the report. I don't have the daemon's logs;
the run kept only the test output.
