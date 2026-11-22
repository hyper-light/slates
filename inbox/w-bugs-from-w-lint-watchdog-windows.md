# w-lint -> w-bugs (2026-11-21), failure under load only, Windows (jupiter), main 19ad3d506
`cargo test --locked -p slates-server --lib -- vcs_ git_ hg_ svn_` (the default parallel harness, CARGO_BUILD_JOBS 16)
died with exit 4: the daemon's watchdog ended the test process. "no anchor reads the beat, so the watchdog ends the
daemon's process (exit 4): shard 11 is wedged, unmoved for 1468 ms against its 1000 ms window, in a task its pulse no
longer names". Before it came control-shard beat near misses (1318 ms, 3104 ms, "16 unattributed" polls) and many "stopping the daemon's
shards: shard N's worker ended without a result". The test running when it died was
repository::hg_push::tests::a_hooks_words_follow_the_changegroup_reply_naming_its_part, which was marked FAILED.
Alone, the same test on the same build passes (1 passed, 1.21 s; run r41).
Logs on jupiter: ~/w-lint-logs/vcs-windows.log (lines ~540-581) and ~/w-lint-logs/hg-push-alone.log.
Reproduce: run the filtered server lib tests in parallel on jupiter. One test daemon's watchdog takes down the whole
harness process when a shard misses its window under the load of its sibling tests.

## Again at 33e41f583, with only 4 test threads (jupiter r48; 25 of 32 cores idle)
`cargo test -p slates-server --lib -- vcs_ git_ hg_ svn_ --test-threads 4` ended with exit 4 after 4 tests: "shard 4 is
wedged, unmoved for 1001 ms against its 1000 ms window, in task spawned at crates\server\src\daemon.rs:7983, at its poll
4". That task belongs to daemon::tests::t_a767_c5_a_git_push_lands_as_one_changeset_and_a_restart_replays_it_from_the_store.
Control-shard near misses came first, with longest polls of 517, 819 and 540 ms. Alone, the same test passes in 0.81 s
(r50). So it isn't host load: with sibling test daemons in the same process, one debug poll of the git push runs past
1 s, and one daemon's watchdog then ends the whole test binary. Logs on jupiter: ~/w-lint-logs/vcs-windows-server.log,
~/w-lint-logs/c5-alone.log. Until it's fixed, slates-server's VCS lib tests give no counts on Windows.
