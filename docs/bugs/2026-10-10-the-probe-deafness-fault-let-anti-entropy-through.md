# The probe-deafness fault let acknowledgements and anti-entropy through

**Found:** 2026-10-10, behind the council-leader test's intermittent
(`a_council_leader_the_failure_detector_holds_dead_is_replaced_and_retired`). It failed 2 of 8 runs at HEAD
(`bc11feba`, unpatched detector) and 2 of 8 with the detector patches. **Status: fixed** the same day.

## Symptom

The test isolates the council's leader on the probe plane, both ways, and waits for the survivors to retire it. In
the failing runs, neither survivor led, and the leader stayed in the committed council. The log showed the leader
condemned and refuted over and over: dead at incarnation 1, alive at 2, dead at 3, … up to 604 and 643 within the
test's window, at HEAD.

## Root cause

`Daemon::inject_probe_deafness` was built for the relay test's asymmetric loss: a node left the **direct** probes of
the named peers unanswered (`StateFleet::answer` refused them).
- Everything else from those peers was still admitted: acknowledgements, relayed answers, and anti-entropy chunks
  (`Fleet::admit`, which did not consult the fault).
- An isolated leader still received the survivors' anti-entropy, so it learned of its condemnation and refuted it.
- Its own anti-entropy reached the survivors, so they adopted each refutation. Each refutation made it a live member
  again.
- No election ever began, and only a leader proposes a retirement, so it was never retired.

Three of the fault's four uses meant total isolation:
- the council-leader test ("it answers no probe and hears none");
- the owner-isolation helper of the lease tests ("both directions");
- the relay test's retire phase (both directions since `bc11feba`).

The fault gave none of them that. It was not a physical model either: no packet loss lets a node ignore a peer's
pings while still hearing that peer's acknowledgements.

## Fix

- **The hook.** `Fleet::hears(from)` (`crates/cluster/src/member_plane.rs`). Every datagram from a peer the node is
  deaf to is dropped once opened, before any message in it is handled, as a lost datagram would be. It is counted in
  `refused_by_fleet`.
- **The implementation.** slates' `StateFleet::hears` consults `probe_deaf_to`. `answer` no longer does.
- **The model.** A node deaf to a peer models the one-way loss from that peer. Two nodes deaf to each other are
  isolated on the probe plane, while the record plane stays whole.
- **The relay test's phase 1.** B deaf to A is now the one-way loss from A to B. A reaches B only through C, and B
  reaches A only through C too, since A's answers to B are lost.

## Tests

The council-leader test passed 8 of 8 alone and under a mixed load; counts are in the ledger. The cluster crate's 25
test binaries pass. The relay test passes; see the ledger.

## Siblings

The relay test's retire phase never failed in the 156 runs between `bc11feba` and this fix, though B's anti-entropy
still reached A and C. It passed because condemnation and retirement outran anti-entropy, which is timing, not a
guarantee. It now rests on the model.
