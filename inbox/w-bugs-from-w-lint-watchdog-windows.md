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
