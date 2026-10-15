# The relay-retirement test modeled a one-way loss and called it both paths lost

**Found:** open since 2026-10-07 as an intermittent under load
(`an_indirect_probe_through_a_relay_keeps_a_peer_the_direct_path_lost_and_losing_both_paths_retires_it`). It failed in
2 of 4 full fleet runs on 2026-10-10, and in 4 of 8 concurrent copies at load 110. **Status: fixed** (same day).

## Evidence

Each failing run held B a member of both survivors after 4,000 periods. The failure message was extended in two
steps:
- council leadership: A led in every failure, so a deaf leader holding off an election is ruled out;
- each node's view of B: in two failing runs, A and C held B `Alive` at incarnation 930 and 967.

Every increment of B's incarnation is a refutation, and B asserts each one itself.

## Root cause

The test's second phase made B deaf to A and C (`inject_probe_deafness(&[A, C])`): B leaves their probes unanswered.
B still probed A and C itself, and they answered. hyper-swim piggybacks the prober's suspected or dead belief on the
answer to its probe, the buddy system (`detector::ack_gossip_into`). So B heard that it was suspected, refuted with a
higher incarnation, and its next probe carried the refutation back (`ping_gossip_into`). That is the detector's design:
a member that can still probe and hear answers is alive.

Retirement therefore raced B's refutations. It won when a suspicion ran its course before B's next probe got its
answer, and lost more often under load. The test asserted an outcome the protocol does not promise under the fault it
injected. "Both paths lost" was a one-way loss.

## Exact edits

- `crates/server/tests/fleet.rs`: the second phase makes the loss two-way:
  - B answers neither A nor C, as before;
  - A and C answer B no more (`inject_probe_deafness(&[B])` on each).
  B then never hears it is suspected, and its probes are refused before their gossip is applied. The doc comment says
  why.
- The failure message keeps the council leadership and each node's view of B, so any other way of keeping B names
  itself.

## Siblings

- The protocol behaviour is unchanged and intended. A member whose answers are lost but whose own probes get through
  stays a member, as Lifeguard's refutation means it to. Its owner leases still renew through its own probes; other
  owners' lease confirmations from it fail and fall to their other holders.
- The whole-RAM replacement test's intermittent is separate and owed (GAPS).
