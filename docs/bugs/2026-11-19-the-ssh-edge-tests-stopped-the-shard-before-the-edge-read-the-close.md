# The SSH edge's tests stopped the shard before the edge read the client's close (A-651)

## Description

`vcs_ssh::tests::a_held_key_runs_its_command_and_its_connection_teaches_the_edge` failed at its assertion on the
phases' samples (`crates/server/src/vcs_ssh/tests/serve.rs:530` on main; line 583 with A-651 part 3's tests in the
file): left `(1, 1, 1, 0, 0)`, right `(1, 1, 1, 1, 0)`. The closing phase's sample (`samples[4]`) was missing. Main
saw it once under load (kiln r85, Mac Studio, 06daf9301, load average 9 to 12,
`cargo test -p slates-server --lib vcs_ssh -- --test-threads=4`: 18 passed, 1 failed). A-651 part 2's six runs passed
it. With A-651 part 3's tests in the same run it failed in 3 of 3 runs on the Mac Studio: 9af6531f, and the gate copies
45ecc528 and 0d8ad1b4, each `cargo test --locked -p slates-server --lib -- vcs_ssh credentials daemon_edge
--test-threads=4`, 37 of 38.

## Root cause

The test client (`serve.rs`'s `transcript`) read the session until the server closed the channel, sent its own close,
and returned at once. Returning dropped its socket and then its `StopsShard`, which sends the edge's shard
`Control::Shutdown`.

The edge takes the closing phase's sample when it reads the client's close (`SessionEvent::ChannelClosed`, then
`tracker.end`). It counts the connection and records its lifetime in `ended`, which the edge calls "before its socket
closes, so its client cannot see the end before the count". The client never waited to see that end.

The shard's `step` (`crates/rt/src/shard.rs`) drains its controls before it polls the ready tasks, and
`Control::Shutdown` cancels every task (`cancel_all`). If the shard thread woke for the client's close after the
client thread had queued the shutdown, its next step took the shutdown first. That cancelled the edge's connection
task before it read the close, so there was no closing sample and no lifetime. The window between the client's write
and the shard's wake grows when other tests load the CPUs. A-651 part 3 adds a two-shard daemon's test and a second
whole connection beside this test. The last failing run's log interleaves that daemon's start line into this test's
result line (`... teaches_the_edge ... slates-server: the start beat 13FAILED`).

## Impact

Tests only. The edge's rule is right: it counts before its socket closes, and a real client cannot stop the edge's
shard. While the race stood, the edge's focused run could fail this test under load, and it failed in every run with
part 3's tests, so part 3 could not land on a green run.

## Exact edits

- `crates/server/src/vcs_ssh/tests/serve.rs`: after sending the client's close, `transcript` waits for the server to
  end the connection (`Peer::ended`, bounded by the client's patience). A new field, `Transcript::ended`, records
  whether it did. `pushed` and the refused `exec` test's expected transcript say `ended: true`, so both tests also hold
  the edge to ending the connection after the channel's close. Every test that reads a transcript (`session`'s and the
  refused `exec`'s) now reads what the edge holds only after the edge has ended the connection.

## Evidence

Mac Studio d1323898, macOS arm64, debug, 2026-11-19:
- Red: the three runs above. Their logs are on the Studio in `~/.cache/g294-a651/` (`p3-green-server.log`,
  `p3-gate-server.log`, `p3-gate2-server.log`).
- Green, kiln r122, at a0c665d3 (main 2410011d with this change, the same code as the landing):
  `cargo test --locked -p slates-server --lib -- vcs_ssh --test-threads=4` passed 25 of 25 with no warning, including
  both tests that read a transcript. `cargo fmt -p slates-server -- --check` was clean.
- Part 3's own run, with its tests beside these, comes with A-651 part 3's landing.
