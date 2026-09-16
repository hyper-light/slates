# Retirement closed a same-id restart's freshly-authenticated serve session, so the pod never rejoined

Date: 2026-09-14
Area: `crates/server/src/fleet.rs` (`probe_peer`'s retirement cleanup), `crates/transport/src/demux.rs`
(`Demux::close_peer`, removed)
Severity: correctness of §4.8 "Recovery" on Kubernetes — a whole-pod restart never rejoined the mesh (the
KIND lane's one open gap of 2026-09-14). Takeover was unaffected and the lane stayed green, so this was a
silent loss of the returning node, not a crash.

## Symptom

On the KIND lane, when an owner's pod was deleted the StatefulSet recreated it at a new IP; the replacement
never rejoined within the 120 s window (`peers_probed 0`, `fleet_members` = itself only) while the two
survivors held each other (`peers_probed 1`). Reproduced in process, at the **same** address, by
`crates/server/tests/fleet.rs::a_same_id_restart_survives_the_survivors_retirement_of_the_dead_incarnation`
(written first, and failing before the fix): a two-node fleet forms, B is stopped and a fresh B restarts
under the same certificate and generation-0 member id at the same addresses, and A must keep B a member
through its retirement of the dead incarnation. Before the fix, A retired B and never re-admitted it.

A time-sampled diagnostic on the unfixed tree showed the exact deadlock (this box, serialised, at rest):

| time from restart | A knows B | B′ knows A | A probe-plane `unknown_id` |
|---|---|---|---|
| 0.0 s | yes | yes | 0 (`replaced` 1 — B′'s bind evicted old-B's slot) |
| 6.0 s | **no** (A retired B) | yes | climbing 3 → 19 |
| 12.1 s | no | **no** (B′ retired A) | — |

The climbing `unknown_id` is the mechanism made visible: after A retired B, B′'s 1-RTT probe packets reached
A on a connection id A no longer mapped — because A had **closed the session B′ was using**. B′ then aged A
out in turn, and the two idled in a circular wait, each reporting `fleet_meshed` **vacuously** (a retired
peer is excluded from the mesh check, so a node with no un-retired peers is "meshed" with an empty set).

## Root cause

A pod restart currently returns at generation 0 with the same manifest seed member id its predecessor held:
the implementation loses its generation counter with the RAM anchor segment. This describes the current
implementation, not a requirement of R1. The replacement is the **same member
id** under the **same operator certificate**, arriving while the survivor still holds its now-stale probe
session to the dead predecessor.

`probe_peer`'s retirement cleanup closed the retired peer's **incoming** sessions **by certificate**
(`Demux::close_peer`, keyed on `by_peer`). That cannot distinguish the dead predecessor's session from its
replacement's: both authenticate with the same certificate. In the race the replacement had already
re-dialed and **bound** its fresh serve session under that certificate (`Endpoint::connection_id` →
`Demux::bind`, which also evicts the predecessor's slot — the `replaced` counter), so the session
`close_peer` tore down was the **replacement's** — the very session carrying the survivor's death belief back
to the returning node.

Rejoin needs that session: the survivor marks the peer `Dead@N` and echoes it on the serve reply
(`serve_peer_probes`); the returning node applies the death to itself, self-refutes to `alive@N+1`
(`Membership::refute` bumps past the death incarnation it hears, so even a restart at incarnation 0 jumps
past `N`), and gossips that, which the survivor adopts and re-admits. Closing the session at the moment of
retirement severed exactly that path.

## Fix

Retirement no longer force-closes the peer's incoming sessions. `probe_peer` still drops this node's
**outgoing** probe session to the peer (the top of the loop idles until the peer rejoins); the peer's
**incoming** sessions are left owned by their serve tasks and reclaimed by the demultiplexer's
authenticated-replacement mechanism — when the peer re-dials (a restart, or a refutation after a false
death), its fresh handshake replaces its own prior session under its certificate (`Demux::bind`), ending the
stale one. `Demux::close_peer` had no other caller and was removed, so the cert-keyed close cannot be
reintroduced by accident. A peer that never returns holds one idle serve slot per plane, bounded by the
roster (banned item 8 holds).

Edits:
- `crates/server/src/fleet.rs`: the retirement branch of `probe_peer` drops the two `close_peer` calls (and
  its now-unused `demuxes` parameter and the argument at the spawn site); the comment states why the
  incoming sessions are deliberately left to the serve tasks and the bind-replacement.
- `crates/transport/src/demux.rs`: `Demux::close_peer` removed (dead after the above).

## Verification

`a_same_id_restart_survives_the_survivors_retirement_of_the_dead_incarnation`: before the fix it failed (A
retired B and never re-admitted it); after, it passes in 11.9 s (A retires B transiently at ~6 s and
re-admits it ~0.3 s later through the death-echo/self-refutation path, `unknown_id` staying 0, and the mesh
holds stable for the settle window). The fixed-tree diagnostic showed the same: retire at 6.0 s, re-admit at
6.3 s, stable through 30 s. `cargo clippy -p slates-server --all-targets -- -D warnings` clean.

This is the same-id contrast to `a_restarted_peer_is_learned_on_contact_under_its_new_generation` (a higher
generation admitted as a new member and the old id taken over), which passes unchanged: that path follows the
peer's new id and never runs `probe_peer`'s cert-keyed close for the retiring old id.

## Why the existing restart test did not catch it

`a_stopped_daemons_serve_ports_are_freed_so_its_restart_binds_the_same_addresses` asserts only that the
restart rebinds the ports (no `fleet.bind` refusal) and that `fleet_meshed` is true — which is satisfied
**vacuously** once A retires B (an empty peer set is trivially meshed). It never asserts that A still **knows**
B, so the deadlock passed it. The new test asserts mutual membership across the retirement cycle.

## Sibling sweep and related items

- **Voter safety remains open (AUD-07).** Restoring probe sessions does not restore a Raft term, vote,
  accepted records or log after whole-pod RAM loss. The same-id regression proves membership liveness
  only; it does not authorize a state-losing process to resume voting under its predecessor's identity.
  The design's fresh-member admission and state-transfer requirement remains separate from this fix.

- The design's `serve_peer_probes` "How a return heals" note is correct and unchanged; the bug was that the
  retirement path tore down the session that note relies on.
- Ada's fix design named three further hardenings that are **not required** for this deadlock and were left
  for separate, tested changes so this fix stays minimal. All three landed on 2026-09-16:
  (2) a terminal-transport-failure `ProbeOutcome::Broken` distinct from a timeout (`crates/cluster/src/swim.rs`,
  `outcome_of_request_error`: the demultiplexer's `Closed` and a socket refusal `Io` release the session, every
  other error is one bad packet on a session kept for the next probe; unit-tested by itself), which `probe_and_apply`
  counts as `fleet.probe.broken`, treats as a miss and answers with a fresh dial next period rather than re-probing a
  dead session until the suspicion window retires the peer;
  (3) a dial still in its handshake is dropped at its peer's retirement (`probe_peer`, counted as
  `fleet.dial.stale_dropped`), so the resume dials afresh at the address discovery holds by then — proven by
  `a_retired_peers_pending_dial_is_dropped_and_its_return_at_new_addresses_is_meshed` (A retires B mid-dial, B
  returns at other addresses under its certificate and a fresh id, A meshes to it);
  (4) under the ephemeral member id there is no same-id return to fence — a restart is a fresh member holding
  nothing (R1) — and `a_restarted_peer_is_learned_on_contact_under_its_fresh_identity` now also asserts that the
  returned node does **not** serve its predecessor's volume while the successor does, so a rejoin never disturbs
  the completed takeover.
- A live KIND re-run to confirm the fix on real pods, and making the lane's rejoin a required gate
  (`xtask/src/kind.rs`), are owed — the reproduction here is the same-address in-process analog, since the
  fleet test harness has no DNS resolver to model a new-IP restart.

## AUD-07 follow-up — 2026-09-14

The voter-loss correction now gives every daemon start a fresh member id. The live retirement
regression retains this report's same-certificate/session boundary but asserts discovery of the
fresh replacement (`a_fresh_restart_keeps_its_serve_session_while_the_predecessor_retires`). The
historical same-id reproduction above remains evidence for the certificate-keyed close defect.
Safe voting admission is covered separately by the [three-voter RAM-loss history](2026-09-14-raft-voter-state-loss.md).
