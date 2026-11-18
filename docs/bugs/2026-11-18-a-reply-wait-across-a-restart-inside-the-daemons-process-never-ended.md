# A reply wait across a restart inside the daemon's process never ended (2026-11-18)

## Description

A-456 (main `ba179ff61`, 2026-11-18) made a synchronous call's reply wait end at the reply or at its daemon's departure
(`slates_ipc::endpoint::ClientEnd::wait_while_daemon_lives`). On macOS and Windows the departure it waited for was the
daemon process's exit alone. A daemon stopped and started again inside one process never exits. So a call sent across
such a restart waited forever on macOS and Windows.

The in-process daemons of the client and server tests restart that way. Examples are `crates/client/tests/client.rs`
`a_session_outlives_a_daemon_restart_and_its_retry_meets_the_completion_record` (the canonical restart oracle),
`a_restarted_daemons_first_answer_is_awaited_for_the_reconnect_budget` and
`a_resumed_session_waits_out_a_shard_start_its_supervisor_allows`. An embedding that restarts its daemon inside the
host's process would hang the same way. g251b found it reading those tests for the next piece (A-468), before any lane
ran them on A-456; main held the macOS and Windows client restart runs until the fix.

## Root cause

A client tells a gone daemon from a slow one by `Liveness::daemon_gone`. On macOS and Windows that check reads two
facts. One is the exit watch on the process that answered the claim (A-143). The other is the start stamp in the
instance's bootstrap object, which every daemon start rewrites (AUD-29-20). The stamp is the only one of the two a
restart inside one process changes; the test in `client.rs` says so in its own words ("the client learns the daemon is
gone only once the successor republishes the stamp").

A-456 added `Liveness::departed`, the exit watch alone, for its doubling looks on macOS. It also gave Windows a wait on
the reply's Event and the process handle with no timeout, which nothing but a reply or the process's exit ends. Its
entry said the stamp was not needed, since "a live daemon answers its ring whoever holds the instance's name". That is
wrong. The old daemon's ring is not served after a restart, whether or not its process lives on.

Linux was not affected: there the departure is the control socket's close, and an in-process daemon's stop closes its
clients' sockets.

## Impact

On macOS and Windows, a synchronous call made on a client connected before an in-process daemon restart waited
forever. In the test suites that is a test that never ends and a CI job run to its timeout. In production, only a
daemon restarted inside its own process: the anchor restarts a daemon as a new process, whose exit the watch sees. No
data was at risk; the call never returned.

## The same blind spot in the Windows claim (A-447)

A-447 made an unanchored Windows claim wait on its daemon's process alone. A daemon stopped inside its own process
takes back its start stamp (`Listener`'s drop, A-176), but its process lives on, so such a claim waited forever: the
blocking connect, and the polls of `crates/ipc/tests/rendezvous.rs`
`a_claim_is_polled_without_waiting_and_refused_once_its_daemon_is_gone`, whose `poll_until_refused` never ended on
Windows. The claim now reads, at each poll, the start stamp it began under, and ends refused once the daemon has taken
it back; its blocking wait on the answer's Event and the daemon's process wakes again as long after as it has waited
(A-443's doubling), so the poll reads the stamp. macOS and Linux claims were not affected: the owner's bootstrap name
and the listening socket go with a stopped daemon.

## Exact edits

- `crates/ipc/src/endpoint.rs`: the departure look (`departure_due`) serves macOS and Windows and asks
  `Liveness::daemon_gone` (the exit watch and the stamp). Windows' departure park waits on the Event and the process
  handle with the look's timeout. `Looks` and the look schedule are compiled for Windows too. The module doc and the
  `Waited::Departed` doc say what a departure is per platform. New test
  `endpoint::departure::a_daemon_restarted_inside_its_own_process_ends_the_wait` (macOS, Windows); the departure tests
  stand up a real rendezvous so the stamp is read live.
- `crates/ipc/src/wake.rs`: `Event::wait_or_exit` takes a timeout and answers `EventOrExit` (the Event, the exit, or
  the timeout); the claim's wait passes none.
- `crates/ipc/src/region.rs`: `ClientRegion::wake_wait_or_exit` passes the timeout on.
- `crates/ipc/src/rendezvous.rs`: `Liveness::departed` removed; the test constructor `Liveness::of_daemon` reads the
  instance's published stamp; the Windows claim keeps its start stamp (`Claim::stamp`) and ends at its taking back
  (`daemon_went`), its blocking wait timing out at the time waited. Also, from main's checks the same day:
  `CLAIM_WAIT_NS` derived from the liveness window W with its users named, and Format lines within the literal check's
  reach on `MAGIC` and `HEADER_BYTES`, `size_of` in `DaemonMark::bytes`.
- `crates/ipc/src/region.rs`: `PARKED_ASYNC`'s Format line within reach.
- `crates/ipc/tests/rendezvous.rs`: the claim test's first loop moved to `poll_through_a_claim_wait`, and a refusal
  with the daemon alive fails on every platform (it was allowed on Windows, a constant assertion).
- `docs/wip/SLATES_DESIGN.md`: A-456's entry (a fix bullet), §4.7's status and "Wake strategy"; `docs/wip/GAPS.md`:
  the A-456 line.

## Evidence

See the A-456 entry's fix bullet: the unit test red with the exit-only look put back, green on the fix, and the restart
oracle on main and on the fix (Mac Studio, 2026-11-18).
