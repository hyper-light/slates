# Sparse mesh drops third-member failure reports

Date: 2026-09-17. Design: §4.8 Membership, D-14, T-8.5, AC-8.14.

## Evidence and cause

The five-daemon copyset-routing history failed in 35.69 s on Linux. At 32.006 s,
two survivors had retired the stopped owner, while two home-region survivors still
reported it alive. The council still contained the owner and takeover had not begun.
This is a membership convergence failure before the location query is exercised.
Trace: `lookup-outage-final-1.log` in the supervised session scratch directory.

The minimized wire history
`a_neighbors_gossip_retires_a_known_third_member_without_enrolling_strangers`
failed in 0.81 s (3.89 s including compilation). Command:
`cargo test -p slates-server --lib a_neighbors_gossip_retires_a_known_third_member_without_enrolling_strangers -- --nocapture`.
It decodes a real SWIM message reporting a known third member dead, then supplies
stale alive gossip. The shared membership incorrectly keeps the member alive.

Each live session had a private Detector. Incoming gossip reached that detector,
but only its directly probed peer reached FleetNode. Deaths found by one detector
were also only disseminated toward its own peer, which was dead. A council voter
outside the dead member's direct mesh could therefore wait indefinitely.
Private detectors additionally admitted third members into their probe rotation,
although the server always sent the ping to the session's fixed peer.

## Exact repair

- Keep one bounded dissemination queue with the node's shared membership. Each
  adopted change occupies one entry per known member and is sent for the existing
  derived transmission budget over other live probe sessions.
- Fold authenticated reports about known members into that view. Incarnation
  ordering rejects stale alive reports. Gossip cannot enroll a stranger or revive
  an identity superseded by an authenticated restart.
- Keep each session detector scoped to its actual peer. Publish local refutations
  from the shared view and include the buddy suspicion in the bounded payload.
- Resolve forwarded owner hints on the control shard, which owns membership and
  sessions, rather than from a possibly stale non-control shard copy.
- Exercise dissemination, stale reports, identity boundaries, refutation and fixed
  probe targets, then rerun the five-daemon takeover and routing history.

This repairs live wiring of the existing SWIM design. It does not change consensus
safety, increase timeouts, expand the direct mesh, or claim the outstanding formal
model revalidation and configuration state-transfer obligations are closed.

## Validation

Implementation and green results pending. Both red invocations were foreground,
bounded runs on 2026-09-17; the live run used four Linux CPUs and 2 GiB under Docker,
and the wire test ran on the M5 Max macOS host with Rust 1.98.
