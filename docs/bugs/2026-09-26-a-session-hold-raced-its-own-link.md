# A test's session hold raced the link task's discovery page

Date: 2026-09-26. Contracts: §4.8 "Lookup", the test
`a_location_round_asks_a_peer_whose_session_was_out_once_it_returns`
(docs/bugs/2026-09-25-a-forward-refused-while-the-owners-session-was-out.md). Found by CI run
36275755772 (`4cea8a4`, macOS gates): `the foreign node's session to the successor was held out`,
left `Ok(false)`, right `Ok(true)`.

## Symptom

`Daemon::hold_record_session` reported that the foreign node's link held no session to take. The test
failed at its setup, before the lookup it exists to check.

## Root cause (from the code; the CI log names no state)

- The hold took the session with a single look (`fleet::take_sessions`), so it found none whenever the
  session was out of its link at that instant.
- Every heartbeat, the link task borrows its own session for a discovery page (`refresh_discovery`, in
  `crates/server/src/fleet.rs`). The session is out of the link for that page's round trip.
- The product already treats this window as normal: a forward waits for the session to return
  (`forward_over_leader_session`). The fixture did not, so it assumed a system condition, "the session
  is never out at this instant", which a loaded 3-vCPU runner breaks.
- Locally the window is microseconds. 18 of 18 runs under six-copy load found the session at the first
  look (waited 42–83 ns), and 12 of 12 at HEAD passed.

## Fix

- The hold waits for the session the way a forward does: paced at the fleet's poll interval, for at
  most `SESSION_WAIT_NS` (one liveness budget).
- It reports a typed outcome, `SessionHold`:
  - `Took { waited_ns }`;
  - `NeverInItsLink`, when the link exists but its session never came back;
  - `NoLink`, when this node kept no link to the peer.
- If the hold misses again, the failure names which case it was instead of a bare `false`.
- The test's assertion is unchanged in substance: the session must be held out before the lookup runs.

## Edits

`crates/server/src/daemon.rs`, `crates/server/src/lib.rs`, `crates/server/tests/fleet.rs`, this record.
