# A forward was refused while the owner's session was out

Date: 2026-09-25. Contracts: §4.8 "Lookup" (a node that cannot serve a verb forwards it to the owner),
§4.9 (RIFL: a retried request id meets its completion record), CLAUDE.md "typed refusals" (an
uncategorized refusal is a bug). Found by CI run 36191789379 (`e339bd8`, Linux gates).

## Symptom

`a_cross_region_client_finds_the_copyset_successor_instead_of_an_unrelated_live_peer` failed on CI (the
successor did not adopt within the audit wait). Looping it alone in a Linux container (four CPUs,
io_uring allowed, the fleet trace on) failed it three more ways, on iterations 6, 22 and 16, all with the
same refusal:

- a foreign client's write `Refused { HomedElsewhere { region: 0 } }`, one millisecond after its read
  of the same volume was served;
- the retry of a write that had just been served, refused the same way;
- the interrupted client's final retry, after its write was served, refused the same way.

## Evidence

The trace records each daemon's routing view after the failure: its root, regional and fleet
configuration versions, the owner it holds, whether a takeover is pending, and the creator's liveness.
Every region-0 node agreed on the successor as owner, nothing was pending, and the creator was dead. The
refused writes were not misrouted: the foreign node's `fleet.owner_location.direct` count rose by one
per write, so each went straight to the right owner.

New counters named the failure. After the refused retry, the foreign node read:

```
fleet.forward.session_unestablished: 1     (a link with no session and no borrow tag)
fleet.owner_location.forward_unsent: 1     (a forward that returned nothing)
fleet.owner_location.direct: 5, fleet.owner_location.round: 2
```

The record link to the owner existed, but its session was not in it. `refresh_discovery` (a periodic
discovery page) takes the session without tagging the borrow, which is why that counter read
"unestablished". A coordinator dispatch (`take_sessions`) holds it the same way, with a tag.

## Root cause

`forward_over_leader_session` took the peer's session once and returned `None` when it was not there.
Its own doc said the caller retries, and `take_sessions` says "whichever misses the session retries".
The coordinator does retry, the next period. `forward_to_owner` did not: it turned `None` into
`HomedElsewhere` at once. So a client's forward that arrived while the coordinator or a discovery page
had the owner's session out was refused, although the owner was live and serving. Right after a
takeover that session is at its busiest. The refusal went uncounted, so nothing showed it had happened.

## Fix

`forward_over_leader_session` now waits for a session that is not there to borrow, whether it is out or
being re-established. It polls at the fleet's poll interval (`HEARTBEAT_NS / POLL_PER_PERIOD`, the pace
the location round uses for stragglers). The wait sits inside the one deadline that also bounds the
request (`LIVENESS_BUDGET_NS`); the request gets what remains. It counts the first miss by where the
session was (`fleet.forward.session_out` or `fleet.forward.no_session`), and counts a wait that outlives
the deadline (`fleet.forward.session_never_returned`). Only then does the caller refuse, and it counts
that too (`fleet.owner_location.forward_unsent`). Both callers, owner forwarding and the
`promote-region` forward to the root leader, are client-verb tasks rather than the coordinator's loop,
so a wait stalls nothing else.

## Tests

- `a_forward_waits_for_the_owners_session_while_it_is_out` (new, three regions).
  - Setup: a client on b routes a read to a. The test then holds b's session to a out for five poll
    intervals (`Daemon::hold_record_session`, a new test hook that holds it exactly as a dispatch does),
    while the client writes through b and retries.
  - Expect: the write is served, the retry answers the same snapshot, `session_out` was counted, and no
    session outlived the deadline.
  - On the old forward it fails at once with CI's refusal (`write not served: Refused { HomedElsewhere {
    region: 0 } }`). With the fix it passes, 20 of 20 on Linux.
- The copyset test, alone on Linux: 45 of 45 after the fix. The forward met a session out and waited in
  7 of those runs; each of those waits was previously a refused write.
- The copyset test keeps its opt-in trace of routing views, replies and counters.

## Found on the way (not changed here; reported)

1. **The location round skips a peer whose session is out.** `owner_location::locate` is one bounded
   round with no retries. A round that misses the owner that way answers `Unavailable` (seen once:
   `fleet.owner_location.unavailable: 1`). A client retries past it, but it is the same shape as this
   bug.
   **Fixed 2026-09-26.**
   - The round asks such a peer once its session returns, inside the round's one deadline.
   - It counts `fleet.owner_location.session_out` and `.session_never_returned`.
   - A peer with no session at all is not waited for, since the bounded neighbourhood keeps none to most
     peers.
   - `a_location_round_asks_a_peer_whose_session_was_out_once_it_returns` failed before the fix with
     `HomedElsewhere { region: 0 }`. It passes 3 of 3 after, and the fleet suite passes 50 of 50 on
     macOS.
2. **Formation left a member outside its council** in one full-suite run on Linux (267 s, 49 of 50
   passed). In region 0, three nodes agreed on a three-member council and the fourth had an empty
   council view: it was never admitted within the 30 s formation wait. The shards' learned wakes that
   run were 11–61 µs (median 19), the boot mean's range, so the idle spins did not grow. The formation
   wait (`audit_wait`) writes no trace, and the failure message omits membership and links. Owed:
   diagnostics that make the next occurrence explain itself. This matches the KIND lane's unexplained
   formation failure.
3. **A first placement that never converged** kept `poll_until` waiting past twelve minutes in one
   full-suite run. It charges its budget in the slowest daemon's periods (4,000), so a non-convergence
   takes at least about 400 s to be declared, while the test polls `AwaitPlaced` about a thousand times
   a second (the client was at request 707,595). Owed: the same diagnostics, and a look at whether the
   poll's own pressure starves the owner it is waiting on.

## Edits

- `crates/server/src/fleet.rs`: `forward_over_leader_session` waits within its deadline and counts.
- `crates/server/src/verbs.rs`: `forward_to_owner` counts an unsent forward.
- `crates/server/src/daemon.rs`: `Daemon::hold_record_session` (test support).
- `crates/server/tests/fleet.rs`: the new test; the copyset test's routing trace.
- `docs/wip/SLATES_DESIGN.md` (§4.8 status), `GAPS.md`, `TBD_FIXES.md`.
