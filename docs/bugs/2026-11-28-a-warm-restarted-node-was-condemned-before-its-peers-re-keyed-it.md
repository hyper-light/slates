# A warm-restarted node was condemned before its peers re-keyed it

- **Date:** found 2026-11-27 (diag CI 13, GitHub run 46114400491 at 9b44036d0, windows-11-arm); measured 2026-11-28
  (winfleet-5, run 46122204138, 5 runs in 16; winfleet-6, run 46125840249, 7 runs in 16; windows-latest 16 of 16 in
  both); fixed 2026-11-28 (main 01a20182's ruling of 00:42).
- **Subsystem:** the membership plane's detector across a warm restart (§4.8, A-921 part 5):
  `vendor/hyper-raft/hyper-swim/src/detector.rs`, `crates/cluster/src/member_plane.rs`,
  `crates/server/src/member_task.rs`.

## Symptom

`a921_a_warm_restarted_node_answers_its_peers_while_a_silent_one_holds_its_announcements`: "no other node condemned a
within 4 detections of its settle": one or two peers held a `Dead` at incarnation 0, 143 to 362 ms after a's settle,
against a detection of 1 s. a refuted every time (each peer folded it `Dead(0) -> Alive(1)` once it re-keyed a), but
the test reads the peers' beliefs before the refutation reaches them, and a death in that gap retires a beside the dead
peer, the mechanism A-921 part 5's tests guard.

## Evidence

- winfleet-5's logs: in every red, two or three peers fold `Dead(0) -> Alive(1)` at `member_task.rs` `admit_beliefs`
  (the detector's beliefs) after the settle; ten of the eleven greens fold none (run 8, green, one). The fold to dead
  was logged nowhere: `FleetNode::observe` logged only folds to alive. 832ecc2b9 logs a fold to dead with its call
  site, and the test's reading carries the condemning node's `council_death_view` of a.
- winfleet-6, with it: every red's dead folds are at `admit_beliefs`, on 2 or 3 peers. The condemning node's own
  detector, in four reds: `condemnations: 1`, `condemned_after` 327, 576, 607 and 669 ms from a's last answer, judge
  `Pool`, unconfigured, intervals 27 to 87 ms, loss 5 to 9 %; the other three by gossip from such a node.
- Run 3's timeline (a = `HostId(13162590173136283905)`): a's last answer at 502.896 s on the condemning peer's clock;
  its old edge closed at 502.898; its new process listening at 502.914; the condemnation pending at 503.140 (its told
  probe unanswered, `suspicions: 0`: the suspicion came by gossip) and condemned about 503.223, at the next answer from
  another member; `condemned_within` 11.09 s.

## Root cause

Two rules that did not fit. A-921 part 5 grants a peer one detection to re-key a restarted node. Until a peer does, its
probes reach the node under its old process's keys and go unanswered, and the node cannot refute a suspicion it cannot
hear. hyper-swim's detector condemns a told suspect whose probe went unanswered at the next answer from another member:
two or three periods after the last answer, with no floor (`detection_bound` bounds a detection above; slates' "least
bound", `member_plane.rs`, is the least such upper bound). At 65 to 87 ms periods that is 327 to 669 ms, before the
slower host's re-key. On faster hosts the re-key comes first.

## Fix

- A local change to vendored hyper-swim (its README's "Local changes" item 4, each site marked `slates:`):
  `Detector::hold_judging(peer, held)`. While a peer is held, an unanswered probe of it is counted
  (`PeerReport::unjudged`, `Detector::unjudged`), never accounted or missed, so it neither suspects the peer nor makes
  its condemnation pending, and a pending condemnation waits for the release. A peer not held, and an answered probe,
  are judged as before. `MemberPlane::hold_judging` passes it through.
- The re-key window (`member_task`): a peer that learns a member's process restarted (`note_reconnected`, a changed
  process nonce) opens the member's window and holds its probes; the member loop's turn closes it at the re-key's
  install (the installed epoch's process the one last announced) or one detection after it opened, whichever is
  first, and its probes are judged again. No timer of its own; at most the fleet's peer capacity of windows. Counted:
  `fleet.plane.rekey_opened`, `rekey_closed.keyed`, `rekey_closed.expired`, and `rekey_unjudged` (the held probes, the
  non-vacuity).
- **Bound:** a process that dies just after its restart is condemned within one detection plus `detection_bound`. That
  is the window's price, paid only while it is open.

## Tests

- `crates/cluster/tests/member_plane.rs`
  `a_restarted_member_held_unjudged_through_its_rekey_is_condemned_only_once_released`: three members judged; member
  3 silent both ways and held by both others until each holds six of its probes unjudged: condemned by neither while
  held, by both once released. Red with the hook a no-op: member 1 condemned it while held.
- `member_task` tests `a_restarted_members_rekey_window_opens_at_its_new_process_and_closes_at_its_rekey` and
  `a_rekey_window_never_keyed_closes_one_detection_after_it_opened`. Red with the window's opening a no-op.
- hyper-swim's own suite on the patched crate, 96 of 96, with the hook's unit test
  (`slates_a_held_peer_s_unanswered_probes_are_unjudged_until_it_is_released`).
- The fleet test itself is judged on windows-11-arm by main's winfleet rerun.

## Upstream

Owed: proposed to hyper-raft's owner as a feature gate, so hyper-swim's sources return to upstream's bytes. That
proposal goes through Ada.
