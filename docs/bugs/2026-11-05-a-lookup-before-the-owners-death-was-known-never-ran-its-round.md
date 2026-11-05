# A lookup before the owner's death was known never ran its round

Main CI [41990139505](https://github.com/hyper-light/slates/actions/runs/41990139505) on `21da6e57` failed the macOS
job (job 127120151580) in `a_location_round_asks_a_peer_whose_session_was_out_once_it_returns`
(`crates/server/tests/fleet.rs:5099`; §4.8 Lookup). The foreign client's `Status` was answered after 4.027 s with
`Refused { refusal: OwnerUnreachable { region: 0 } }`. The foreign node's counters were `fleet.forward.no_session` 4,
`fleet.forward.retried` 3, `fleet.forward.session_never_returned` 4, `fleet.owner_location.direct` 1,
`fleet.owner_location.forward_unsent` 4 and `fleet.owner_location.route_dropped.refused` 1; the successor counted
`fleet.owner_location.owned_unmaterialized` 1. The same failure, with the same counters, was recorded open on
2026-11-04 (1 of 50 runs on one busy CPU, before `21da6e57`;
`docs/bugs/2026-11-04-a-settled-fleet-refused-its-first-create.md`, "Another failure of the same history").

## Not the quorum collector's change

The only code between main's last green run (`4a221524`) and this one is `21da6e57` (`crates/cluster`: a dispatch's
reply wakes its collector). The hypothesis was that the location round's retries had been paced by the collector's
10 ms poll. An interleaved A/B on adacachy (AMD Ryzen 9 9950X3D2, Linux 7.2.9-2-cachyos), 2026-11-05, in capped user
units with busy loops inside them (`abfleet.sh 50 <loops> fleet-A-4a221524 fleet-B-21da6e57 …`):

| load | `4a221524` | `21da6e57` |
|---|---|---|
| one CPU, two busy loops | 1 of 50 failed (round 42, the same counters) | 0 of 50 |
| two CPUs, four busy loops | 0 of 50 | 0 of 50 |

The failure predates the change, and it never ran the location round: `owner_location.direct` says its owner was
known without one, and no `owner_location.session_out` was counted.

## Cause

`resolve_and_forward` (`crates/server/src/verbs.rs`) takes the volume's owner without a location round when one is
known: a cached route, else the volume's creator while it is an eligible member of the region, that is, not held
dead by this node (`Query::known_owner`, `crates/server/src/owner_location.rs`). The foreign node is in another
region and learns of the owner's death through its own detector or the fleet's gossip. The test killed the owner,
waited for the successor to adopt and to serve its own client, and then looked the volume up from the foreign node
at once. Under load the foreign node did not yet hold the owner dead. So:

1. The creator was its known owner, and the lookup forwarded to it directly (`owner_location.direct` 1), over a
   link it no longer had (`forward.no_session`).
2. It sent again while the owner was not confirmed dead, within the takeover bound (`forward.retried`).
3. It was refused `OwnerUnreachable`, as §4.8 specifies for a takeover's window ("A dead owner is declared within
   the membership horizon, which ends the resends, and the client is refused as before").

The product behaved as designed. The test's subject, a location round that meets the held-out session and asks
the successor once it returns, never ran. Its precondition, that the foreign node holds the owner dead, was not
established. A faster takeover (each commit no longer waits a 10 ms poll since `21da6e57`) brings the lookup
closer to the death, which may explain the timing of the CI failure; the A/B above does not show a higher rate.

## Exact edits

- `crates/server/tests/fleet.rs`, `a_location_round_asks_a_peer_whose_session_was_out_once_it_returns`: before the
  hold and the lookup, wait (the fixture's audit wait) until the foreign node holds the dead owner dead, and fail
  with what it holds if it never does. The lookup, the hold, the reply and every counter assertion are unchanged.
- Its failure message now also prints the foreign node's view of the dead owner, the hold, its record links
  before the hold and after the lookup, and its link, dial, discovery and accept counters.

The counters alone establish the precondition's absence in every failing run: the foreign client was connected for
the lookup, so it carried no cached route (`owner_route` is the client's own slot), and with none
`owner_location.direct` is counted only when the creator is an eligible owner, that is, not held dead there.

## Verification

Interleaved A/B on adacachy, 2026-11-05: `21da6e57` (before) against the fix (after), three capped units each
pinned to one CPU with two busy loops inside it, 50 rounds each (`abfleet.sh 50 2 fleet-B-21da6e57 fleet-C-fixed
fix<cpu> a_location_round_asks_a_peer_whose_session_was_out_once_it_returns`; logs
`~/.cache/slates-agents-scratch/fixtures/ab-fix-{8,10,11}.log`):

| unit | before | after |
|---|---|---|
| CPU 8 | 0 of 50 | 0 of 50 |
| CPU 10 | 1 of 50 (round 19) | 0 of 50 |
| CPU 11 | 1 of 50 (round 46) | 0 of 50 |
| all | 2 of 150 | 0 of 150 |

Both failures had this record's counters (`owner_location.direct` 1, `forward.no_session` 1). With the A/B above,
unfixed code failed 3 of 250 runs at this load; the fix 0 of 150. The failure is rare, so 0 of 150 carries limited
weight alone: the fix holds by construction, since the lookup now starts only once the condition that took the
direct path is gone.
