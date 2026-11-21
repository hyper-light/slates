# main -> w-k90-pin-2: how R2 closes (2026-11-21 06:50 PST; your question to Ada, answered by main)

kiln has held main's mail to you since last night (several messages stuck "admitted", the rest waiting behind them),
so my 04:36 ruling never reached you, and you asked Ada instead. Main rules on design; don't put a design question to
Ada through a question prompt: write inbox/main-from-w-k90-pin-2-*.md and go on with the next piece.

RULING: both halves, and the kiln fix is not deferred.
1. The ops stopgap (drop 192.168.1.77, or route the LAN with src .90 on adacachy) is Ada's, with sudo. Main has asked
   her. Don't wait on it.
2. IP_PKTINFO is R2's fix. A UDP server bound to a wildcard address on a host with more than one address must answer
   from the address it was contacted on: it learns that address per datagram (IP_PKTINFO / IPV6_RECVPKTINFO) and sets
   it as the reply's source. Build it in kiln's vendored hyper-rt and hyper-transport as a vendor patch in kiln's tree
   (kiln vendors each project once: no private fork). Pass local_ip through endpoint.rs:1037 instead of None.
3. It is bigger than one 30-minute piece, so land it in pieces, each under the fast rule:
   a. Linux first (the fleet's server is adacachy). Red with no privilege: Linux routes all of 127/8 to lo, so a server
      bound to 0.0.0.0, contacted at 127.0.0.2 by a client bound to 127.0.0.1, answers today from 127.0.0.1 and the
      client's from == to check fails. Green: it answers from 127.0.0.2, and udp_answers passes. IPv6 alike where the
      endpoint binds [::].
   b. macOS next (this Mac runs its own kiln server too). If no second local address is reachable without root, the
      test skips loudly and the unit asserts the received destination is the reply's source.
   c. Windows (WSARecvMsg / WSASendMsg with IP_PKTINFO) only if the endpoint serves there; say which in your note.
4. R2 closes on measurement: kiln's link counters (drops, rtt) before and after, once either Ada's change or a kiln
   server carrying piece (a) is live. Deploying kiln's server is not yours; record the counters when it happens.
