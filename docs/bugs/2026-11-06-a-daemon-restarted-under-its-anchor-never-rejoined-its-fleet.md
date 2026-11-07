# A daemon restarted under its anchor never rejoined its fleet (2026-11-06)

Contracts: §4.8 "Membership" (SWIM with Lifeguard; a dead member refutes with a higher incarnation), the probe plane
keyed per pair by its canonical dialer (`member_task::announce_epochs`), the anchor's warm restart (the successor
daemon keeps the member id: `daemon.rs`, the retained boot nonce).

## Description

raft-chaos's report 8: a candidate's daemon killed (SIGKILL) under its anchor is succeeded in 0.3 to 0.9 s, but the
fleet never re-forms: the successor holds three members with no peer probed, while each other node holds two members
and one peer probed. Reproduced on this Mac every time (3 of 3 at load 11) by
`a_candidates_daemon_succeeded_under_its_anchor_rejoins_the_fleet` (`crates/cli/tests/live_chaos.rs`): kill one
candidate's daemon in a settled anchored fleet, then wait for every node to hold three members with both peers probed.

## Root cause (from counters and the membership log)

The successor comes back under the **same** member id (the membership log shows one id before and after the kill), as
a warm restart is meant to. Each other node had condemned that id at its old incarnation. Two things then kept the pair
apart for good, each counted:

1. **The restarted epoch sequence was refused.** Where the successor is its pair's canonical dialer (the lower anchor),
   it announced the pair's plane epochs from one again, and the acceptor, holding the dead process's last epoch, refused
   each as not newer (`fleet.plane.epoch_refused.install` 344 on the acceptor; `fleet.plane.epoch_refused.answer` 343 on
   the successor, one diagnostic run). The pair stayed unkeyed, so the successor's probes, which carry the refutation,
   were never opened.
2. **Contact with a dead member was dropped and never restored.** Where the other node is the canonical dialer, it
   held the id dead, dropped its link (`fleet.link.invalid.believed_dead`), and never dialed it again; the successor's
   own dial announced only its identity, which keys nothing. hyper-swim's refutation needs the dead member's probe to be
   answered with the death, so nothing could revive it.

## Impact

A daemon restart that outlasted the death confirmation (a crash, a watchdog's kill) left its node out of the fleet: no
probes, no council seat, no holder role, for as long as the processes ran.

## Exact edits

- `crates/cluster/src/member_plane.rs`: `MemberPlane::restart_epochs` forgets a peer's datagram keys and installs an
  epoch as the start of a new sequence.
- `crates/server/src/member_task.rs`: `accept_announcement` takes a not-newer epoch from the pair's canonical dialer as
  a restarted sequence (each connection announces once, each epoch above the last, so only a restarted process sends
  one), counted `fleet.plane.epoch_restarted`; its keys come from the new session's exporter secret. A refusal now
  counts its reason (`fleet.plane.epoch_refused.{malformed,forged,not_canonical,install,answer}`). `note_returned` records
  a member held dead that announces itself over an authenticated session, at the incarnation it died at
  (`ShardState::returned_members`, bounded by the fleet's peer capacity; counted `fleet.plane.member_returned`).
- `crates/server/src/fleet.rs`: `direct_contact` keeps contact with such a member (`DirectContact::Returned`) while it
  is held dead at that incarnation, so the canonical side dials it, the pair is keyed, and its probes refute the death.
- `crates/cli/tests/live_chaos.rs`: the failing test, which prints every node's fleet lines when the fleet has not
  re-formed.

## Evidence

- Red on this Mac: 3 of 3 runs at load 11 (the successor held no probed peer for 40 s); with the reason counters, one
  run showed `fleet.plane.epoch_refused.install` 344 on the acceptor and `fleet.link.invalid.believed_dead` on the
  other node, both causes in one fleet.
- One build, three arms, six runs each under two spinners a logical CPU on macos-latest (diag run 42338189526, load 11
  to 20 on 3 CPUs), switched by diagnostic-only environment variables that the commit does not carry:
  - neither edit: 0 of 6 re-formed (406 `install` refusals on the acceptor in the first);
  - the restarted epoch sequence alone: 3 of 6 (the three failures show only `believed_dead`);
  - both: 5 of 6, re-formed 0.10 to 1.68 s after the successor answered.

## Open

One run of six with both edits still failed: the node holding the successor dead counted no `member_returned`, so no
announcement from the successor reached it within the 40 s wait. The counters do not say why; the next diagnostic run
keeps each failure's whole log.
