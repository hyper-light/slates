# One refused receive ended a fleet node's transport (2026-11-07)

## Description

The fleet's serve-socket receive loop (`crates/transport/src/demux.rs` `Demux::run`) treated every refused receive as
the socket's end. One refusal that names no such end (an earlier send's report, which Windows gives as WSAECONNRESET
and Linux as ECONNREFUSED on a socket's next receive) stopped a node's plane for the daemon's life
(`crates/server/src/fleet.rs` `run_demux`, counted `fleet.serve`). The Windows UDP reset of 2026-11-06 was one way to
reach it; the loop's rule was the sibling the supervisor ordered fixed. The endpoint's own-link receive and the DNS
query failed their exchange on any refusal too, and the membership plane read a gone socket again every turn,
without end.

## Root cause

The runtime's receive seam returns every errno but `EAGAIN` and `EINTR` as one untyped refusal, and each loop chose its
own rule. Only a few codes mean the socket cannot be read again: `EBADF` and `ENOTSOCK` (recv(2)), `WSAENOTSOCK` and
`WSAESHUTDOWN` (Microsoft Learn, "recvfrom function"). Linux reports errors to an unconnected UDP socket on receive, some
for an earlier packet sent on it (udp(7), "ERRORS").

## Exact edits

- `crates/rt/src/error.rs`: `RtError::receive_refusal` names a refused receive's kind (`ReceiveRefusal`): the socket
  gone (those codes, and the runtime's own refusals), an earlier send's report, the OS short of resources, or other.
  Per-platform code tables, beside `is_message_too_large`.
- `crates/rt/src/sim.rs`, `udp.rs`: a one-shot receive refusal on the simulated fabric (`sim_udp_refuse_next_recv`).
- `crates/transport/src/demux.rs`: `Demux::run` counts every refusal but the socket gone by its kind
  (`DemuxCounters::receive_refused_*`), yields, and reads on; it ends only on the socket gone.
- `crates/transport/src/endpoint.rs`: an own-link receive counts and reads past them within its deadline
  (`Endpoint::receive_refusals`).
- `crates/server/src/dns.rs`: a query skips them as it skips a stray datagram, within its deadline.
- `crates/server/src/member_task.rs`: the membership plane counts them by kind and ends on the socket gone.
- `crates/server/src/verbs.rs`, `fleet.rs`: status reports the serve sockets' counts
  (`fleet.serve.receive_refused.{earlier_send,resources,other}`).

## Tests, each failing before its change

- `a_refused_receive_does_not_end_the_demultiplexer` (`crates/transport/tests/receive_refusal.rs`): failed on main with
  the loop ended `Err(Io(DriverRefused { call: "recvfrom", code: None }))` and the new peer never served.
- `a_refused_receive_does_not_end_a_clients_handshake`, same file: with the endpoint's change undone, the handshake
  failed `Io(DriverRefused …)`.
- `dns::tests::a_refused_receive_does_not_end_a_lookup`: with the change undone, `Err(Io(DriverRefused …))` against
  `Ok(10.244.0.9)`.
- `a_receive_that_finds_the_socket_gone_ends_the_demultiplexer`, `a_receive_on_a_socket_that_is_gone_names_it_gone`
  (`crates/rt/tests/udp.rs`: a pipe over the socket's descriptor on Unix, a receive shutdown on Windows) and the
  classification's unit test hold the other side: what cannot recur still ends a loop.
- The membership plane's change has no test of its own: its loop needs a daemon's state. The fleet suites cover it.
