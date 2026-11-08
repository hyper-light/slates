# An accepting end gave up on a dialer's repeats, and the dialer waited out two deadlines

**Found.** Slow first formations on the shared Mac at load 127–244: 72.3 s, 40.0 s, 40.9 s and 41.9 s, against 0.2–2.1 s
otherwise. Exclusive session lending was ruled out in the same runs (longest hold 68 ms at the 15 s mark of the 72.3 s
formation). Per-reason counters caught the fourth one (load about 170, 2026-11-08):

- Before the formation wait began, nodes c, d and e each counted `fleet.accept.stall.repeats` 1.
- By 5.2 s, dialer a counted `fleet.dial.stall.confirm_deadline` 3, and it stayed at 1 of 4 peers probed.
- At 21.3, 22.8 and 27.5 s it counted `fleet.dial.redial` 1, 2 and 3, and its peers then came up.

None of the 8 fast runs before it counted either stall.

**Root cause.** Two rules met.

1. An accepting end counted each repeat of a flight it had already answered against the handshake's retransmit bound of 32,
   and `Reassembler::push` reported a repeat per fragment, so one resent flight of F fragments cost F. A dialer's resends
   start at a millisecond and double, and a dialer slow to read the reply keeps resending. So a live dialer spent the
   bound in about a second (a test replaying a real first flight every 100 ms: given up after 1.3 s). The bound was a
   count of what load spends, not a bound on time.
2. By then the dialer had finished its side of TLS and was waiting for a confirmation from an end that had already gone. It
   waited out the absolute confirmation deadline (33 × the 666 ms initial probe timeout, 22 s), and
   `ESTABLISH_BUDGETS_BEFORE_REDIAL` = 2 made it wait out a second one on the same socket: about 44 s. That matches the
   three formations of 40–42 s. 72.3 s fits two such cycles, though that run predates the counters.

The second budget exists for a peer that has not answered (booting, or starved past one budget;
`2026-09-14-handshake-retry-forgets-its-flight.md`). That reason holds for a silent peer. It does not hold for a dialer past
its confirmation, which has heard from its peer.

**Impact.** Liveness only: a fleet formed up to a minute late under heavy load.

**Edits.**
- `crates/transport/src/flight.rs`: `Reassembly::Repeat` is the first fragment of a retransmitted flight;
  `Reassembly::RepeatRest` is the rest. One retransmit, one repeat.
- `crates/transport/src/endpoint.rs`: a turn's repeats no longer count against the retransmits. They are bounded by the
  turn's derived deadline (`handshake_deadline`, the same 33 × initial probe timeout) in time and by the datagram bound in
  work, and this end answers them at its own backoff, not once per datagram.
- `crates/server/src/fleet.rs`: `keeps_its_socket` retries on the socket only for a stall that is not a confirmation stall.

**R3.** `MAX_HANDSHAKE_RETRANSMITS` now bounds only waits paced by this end's own timer, so load cannot spend it: 32 waits of
at most one probe timeout is a time bound. It remains a shape constant, sized to span a boot skew, and is not measured.

**Tests.** Each red-checked in a scratch copy.
- `an_accepted_handshake_is_not_given_up_for_a_dialers_repeats` and
  `a_dialer_that_only_repeats_is_given_up_at_the_handshakes_deadline`: red with repeats counted as before (given up on
  repeats, after 2.6 s).
- `a_dialer_past_its_confirmation_redials_at_once`: red with confirmation stalls keeping the socket.
- `a_dialer_whose_peer_never_confirms_reports_a_confirmation_stall` and
  `a_handshake_that_gives_up_names_the_bound_it_reached`.

**Then also fixed (A-258).** The accepting end gave up silently, so a dialer waited out one 22 s deadline for an end that had
gone. It now sends a close sealed under the Handshake keys (RFC 9000 §10.2.3), and the dialer ends `PeerClosed` within a
round trip; a close that does not open under those keys is ignored.
