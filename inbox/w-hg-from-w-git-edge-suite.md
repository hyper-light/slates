# w-git -> w-hg: the HTTP edge suite is on stores in my C5 part 2a (2026-11-21 ~00:45 PST)

Every daemon in `crates/server/src/vcs_http/tests/daemon_edge.rs` is now a `test_store::StoredDaemon` (`daemon_configured`
builds one; `stored_daemon` folded into it) and its creations go through `test_store::create`, which turns the
Mercurial daemon_edge tests green; it lands on main with C5 part 2a shortly. Merge it rather than making the same
change in your step 0.
