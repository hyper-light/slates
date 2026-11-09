# A restarted node left its peers' sessions to it in place, and they were lent dead for up to 135 s

**Found.** loadtest-scale's f = 1 primary-kill rung (2026-11-08). The primary restarted under the same member id. The
client node's record session to the old process stayed in its slot, and its forward lane borrowed it turn after turn.
Every request through the client node waited 25 to 135 s, until the forwarders had given up often enough for the link
to drop the session. No dial counter moved, so this was not the handshake stall of
`2026-11-08-an-accepting-end-gave-up-on-repeats-and-its-dialer-waited-out-two-deadlines.md`.

**Root cause.** The transport had no stateless reset (RFC 9000 §10.3). The restarted process bound the same port, and
its demultiplexer received the client node's packets for the old session. It held no session under that connection id,
so it dropped each packet, counting it in `unknown_id` and telling no one. On the client node the session looked alive:
nothing failed, packets went unanswered, and each borrower waited out its own budget before the session went back to
the slot for the next one. Only the link's discovery deadline, or the borrower check loadtest-scale is adding,
eventually dropped it.

**Impact.** Liveness only. Every forwarded request through a peer of a restarted node waited up to 135 s. Nothing was
lost or wrongly answered: the forwards were refused and retried.

**Edits (A-261).**
- Each end issues its peer a token inside the encrypted session.
  - The token is HMAC of the session's id under a key that only the node's identity can derive.
  - It is sent in a `ResetToken` frame, only to a peer that says it reads one.
- A restarted node's demultiplexer answers a packet for an unknown session with a stateless reset ending in that token.
  - It never sends one to a source that has a session there.
  - It sends a source at most one per initial probe timeout.
  - Each reset is shorter than the packet that drew it.
- An endpoint on its own socket answers a packet naming another session with a reset too (RFC 9000 §10.3). This covers
  a process restarted onto its old session's dialer port.
- An end receiving a datagram whose tail is its peer's token ends the session `PeerReset`. A tail that matches nothing
  is discarded, as before.
- The fleet drops a reset session's link (`fleet.link.peer_reset`) instead of putting the session back, and the link task
  dials the new process.

**Tests.**
- `a_session_with_a_restarted_node_ends_within_a_round_trip` (simulated, through a relay with a 10 ms round trip). The
  session ended `PeerReset` 10.0 ms after its first packet to the restarted node, which sent one reset and withheld two
  (later packets inside the interval). Red with no reset sent and with no token issued.
- `a_reset_from_another_identity_is_ignored`: a node with another identity sent resets, and the dialer kept waiting to
  the 4 s bound. Red with a dialer that took any undecryptable datagram for a reset.
- `a_node_resets_only_a_stranger_at_most_once_per_interval_and_shorter_than_it_sent`: 43-byte resets to 64-byte
  triggers, none to a 21-byte trigger, none inside the interval, and none to a source mid-handshake. Red without each of
  those limits.
- `the_reset_key_is_the_private_keys_not_the_certificates`: red with the key derived from the certificate.
- `a_dialer_restarted_onto_its_port_resets_the_accepting_ends_session`: the accepting end's wait ended `PeerReset`
  after one reset from the new process. Red with no stray answered, with the demultiplexer dropping the stray, with an
  accepting end refusing its dialer's token, and with a dialer issuing none.
- `a_stray_to_a_session_in_its_handshake_leaves_it_unaffected`: eight reset-shaped strays from the dialer's address
  before the handshake completed. Both ends established, the accepting end discarded none, and no reset was sent. Red
  with the demultiplexer handing strays to a session in its handshake.
- `a_reset_from_another_process_on_the_dialers_port_is_ignored`: 6 resets in the 4 s bound, one per initial probe
  timeout, and the accepting end still waiting at the bound.
- The process test is loadtest-scale's case: `a_restarted_owner_resets_its_old_session_and_b_reaches_the_new_process`.
  - CI's interleaved A/B (diag-61; 3 runners, 15 runs a side, debug as the gate builds the fleet tests): before is
    this branch with the node's reset never sent; after is as committed.
    - After: every run counted exactly one `fleet.link.peer_reset`. The first read through b reached the restarted a
      in 120 / 175 / 220 ms (min / median / max), within the test's one-liveness-budget bound.
    - Before: no reset; the first read reached a in 166 / 206 / 284 ms, and every run failed only on the reset count.
  - **What this test does not show.** Without loadtest-scale's burst, b reached the restarted a on its first read in
    both arms. The stall of 25 to 135 s needs the burst that keeps b's lane holding the session. So this test proves the
    reset fires and the link moves; loadtest-scale's
    `a_forward_lane_leaves_a_dead_session_for_the_restarted_owners_new_one` is the test of the stall itself.

**R3.** The token's 16 bytes, the 21-byte minimum and the 43-byte send length are RFC 9000 §10.3 constants. The
interval between resets to one source is the initial probe timeout, 666 ms: RFC 9002 §6.2.2's 333 ms `kInitialRtt`,
doubled (`rtt::INITIAL_PTO_NS`). The source table is bounded by the
demultiplexer's session pool.
