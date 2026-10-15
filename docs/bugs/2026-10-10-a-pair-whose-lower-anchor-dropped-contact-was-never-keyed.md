# A fleet pair whose lower anchor dropped contact was never keyed

**Found:** 2026-10-09 and 2026-10-10. Full fleet suite runs under suite-wide load (load 50–90 from other sessions)
failed 3 times in 5 with one node unmeshed for 4,000 periods: `a_cross_region_client_finds_the_copyset_successor_…`
once, `a_location_round_asks_a_peer_whose_session_was_out_once_it_returns` twice. Each test passes run alone.
**Status: fixed** (same day as the evidence).

## Evidence

The formation report was extended in two steps: each node's contact reason per peer (`5dd8f3dd`), then each pair's
plane facts (`10e9d589`). The run with both showed the unmeshed node d, and its missing peer a:

| from | to | contact | `keyed` | `canonical` |
|---|---|---|---|---|
| d | a | `Voter` (kept) | `None` | `false` |
| a | d | `OutsideNeighbourhood` (dropped) | `None` | `true` |

The pair was never keyed, and the side that dropped it was its canonical dialer. Every other kept pair was keyed.

## Root cause

A pair's probe plane is keyed by an epoch that only its lower anchor (the canonical dialer) announces, over the record
session that node dials (`member_task::PLANE_EPOCH_STREAM`; the acceptor refuses an announcement from a higher anchor).
A node dials only peers it keeps direct contact with (`fleet::direct_contact`): its neighbourhood, and the council and
root voters. That relation is one-sided:
- every member keeps the voters, but a voter keeps only its own neighbourhood and the other voters;
- an owner keeps its neighbourhood, but a holder keeps only its own.

During formation everyone contacts everyone, so pairs usually key then. Under load, a lower anchor's epoch announcement
could still be pending when the consensus groups formed and its rule dropped the peer. Its per-peer task then idled,
removed the record session each heartbeat, and never announced. The member's probes to it went unanswered from then on,
and it never meshed.

## Impact

- **Formation:** a node could stay unmeshed, and the tests' formation gate failed after 4,000 periods.
- **Production:** a member's probes to a voter, and an owner's to a holder, went unanswered for good when the other
  side was the pair's lower anchor:
  - the owner's lease confirmations from that holder never arrived, and the lease then rested on its other holders;
  - a member's view of a voter's liveness came only through relays.
- **Load-dependent:** mostly unseen on a quiet machine, where formation keys every pair.

## Exact edits

- `crates/db/src/register.rs` `RegionalConfiguration::holds_for(owner, host)`: whether `host` is in `owner`'s current or
  settled neighbourhood, without building the owner's configuration.
  `a_holders_view_agrees_with_each_owners_neighbourhood` holds it equal to each owner's own `neighbours` for every pair,
  in a formed configuration and one with a change in flight.
- `crates/server/src/fleet.rs` `DirectContact`:
  - `Holder`: this node holds the peer's content;
  - `AsVoter`: this node is a voter.
  Both are kept, so contact holds from both sides of every pair that needs it.

## Siblings

- `Staged` (a leader catching a member up) has its reverse already: the member keeps the leader, a voter.
- The formation report keeps its `keyed` and `canonical` fields, so a pair left unkeyed for any other reason names
  itself next time.
