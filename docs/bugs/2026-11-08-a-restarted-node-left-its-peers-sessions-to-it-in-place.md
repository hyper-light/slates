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
the slot for the next one. Only the link's discovery deadline, or the lane-side check loadtest-scale is adding,
eventually dropped it.

**Impact.** Liveness only. Every forwarded request through a peer of a restarted node waited up to 135 s. Nothing was
lost or wrongly answered: the forwards were refused and retried.

**Edits (A-261).**
- The accepting end issues its dialer a token inside the encrypted session.
  - The token is HMAC of the session's id under a key that only the node's identity can derive.
  - It is sent in a `ResetToken` frame, only to a peer that says it reads one.
- A restarted node's demultiplexer answers a packet for an unknown session with a stateless reset ending in that token.
  - It never sends one to a source that has a session there.
  - It sends a source at most one per initial probe timeout.
  - Each reset is shorter than the packet that drew it.
- The dialer ends the session `PeerReset` on a datagram whose tail is its token. A tail that matches nothing is
  discarded, as before.
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
- The process test is loadtest-scale's case: `a_restarted_owner_resets_its_old_session_and_b_reaches_the_new_process`.
  Its CI figures are added when the run reports.

**R3.** The token's 16 bytes, the 21-byte minimum and the 43-byte send length are RFC 9000 §10.3 constants. The
interval between resets to one source is the initial probe timeout (RFC 9002 §6.2.2). The source table is bounded by the
demultiplexer's session pool.
