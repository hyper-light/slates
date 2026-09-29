# A symmetric partition never healed

Date: 2026-09-29. Scope: the membership loop's per-peer probe task (`probe_peer`, `crates/server/src/fleet.rs`)
and the rejoin design of A-15. Found by the KIND lane's succession measurement
(`cargo xtask kind succession`; `docs/wip/kind-lane.md`, Piece 6).

## Symptom

Three pods on KIND. The council leader's egress was cut for 15 s from an ephemeral `NET_ADMIN` container,
then healed. 180 s after the heal the old leader had still not rejoined:
- its two peers held only each other alive, and it held only itself;
- it began 175 pre-elections at term 2 while they led at term 3;
- each side's probe task for the other logged `probe of … idles: Some(BelievedDead)`.
Only a restart would have brought it back.

## Root cause

A-15 re-admits a peer the fleet believes dead when **that peer's** probe reaches a node. The node's serve
side echoes the death, the peer refutes to a higher incarnation, and its gossip re-admits it. The probe
loop of the node that believes the peer dead idles, and never dials it: "that establish would block on a
peer that will not answer".

A-15 was proven by one-sided deaths (`a_falsely_retired_peer_rejoins_by_refutation`: A retires B, B keeps
probing A). A cut is symmetric: each side's detector ages the other to death. Both probe tasks idle, no
probe ever crosses, and no refutation can happen.

## Impact

Any fleet whose halves each conclude the other dead: a partition, a link failure, or a node cut off long
enough to be retired. The halves stay apart after the path heals, until one restarts. On KIND a node cut
off alone stayed a fleet of one — its members only itself, 175 refused pre-elections — while its peers ran
without it.

## Fix

An idle probe task reaches out to its peer on a backed-off schedule (`Reconnect`, `reach_out`):
- the first attempt one suspicion window after the retirement: two beats, 200 ms;
- each unanswered attempt doubles the wait (RFC 6298 §5.5's backoff), up to an order of magnitude
  (`ELECTION_MARGIN`) past the longest dilated suspicion window: 6 s at the daemon's beat;
- an answered attempt starts the schedule over.

An attempt is a fresh probe session, one handshake budget, and one ping carrying this node's gossip. That
gossip includes its own state and its belief that the peer is dead, so a live peer refutes at once (the
buddy system). The answer's gossip is folded as any probe's is: an echo of this node's own death, which it
refutes, and the peer's refuted state, which re-admits it. Re-admission stays by refutation alone (a higher
incarnation overrides the death).

An attempt does nothing else a probe does:
- no lease is credited, since the peer may believe this node dead;
- the detector is neither aged nor credited, since the loop realigns it on the resume;
- no path is sampled and no relay asked.

It blocks only the idle task, for one handshake budget. It is counted: `fleet.reconnect.attempted` and
`fleet.reconnect.answered`.

A peer gone for good costs one handshake per 6 s. A healed partition is found within 6 s.

## Tests

- **Failing first:** `peers_that_each_believe_the_other_dead_find_each_other_again`
  (`crates/server/tests/fleet.rs`). Two daemons each have the other's death injected; the network between
  them stays whole. Before the fix they stayed apart past the rejoin deadline: it failed after 402 s of
  waiting. After it they are together in 4.3 s, stay together, and the reconnect counters show an attempt
  answered.
- **The schedule:** `reconnection_backs_off_to_its_cap_and_an_answer_starts_it_over` (unit): 200 ms, doubling
  to 6 s and held there, and an answer starting it over.
- The daemon's in-process fleet suite passes 54 of 54 (214 s). Every test that retires or kills a peer now
  runs reconnection attempts to it.

## Measured on real pods (KIND, 2026-09-29)

The succession step now waits after each heal for every pod to hold all three members with one leader
again, bounded by the lane's rejoin wait (120 s). Over 19 trials the healed leader rejoined 4.95–6.30 s
after the heal, every time. Before the fix it had not rejoined 180 s later. By the heal, 15 s into the cut,
the schedule has reached its 6 s cap, which bounds the wait.

## Siblings

- The record link task idles the same way for a retired peer (A-15). It needs no attempt of its own: it
  resumes when the probe plane re-admits the peer.
- A peer the council has retired rejoins as an ordinary re-admission, which the group reconciles (A-15's
  authority stays the configuration group).
