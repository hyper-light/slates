# A Windows UDP socket died on an earlier send's ICMP error (2026-11-06)

## Description

Conformance's diagnostics run 42150662467 (`diag-conformance-24`, windows-latest): a three-node slates fleet never
formed in 40 s, in either run. Its nodes probed only one or two of their peers after a handshake's `recvfrom` failed
with Winsock error 10054, WSAECONNRESET.

## Root cause

On Windows, an ICMP port-unreachable answering a datagram the socket sent earlier is reported as WSAECONNRESET on the
socket's next `recvfrom`, while `SIO_UDP_CONNRESET` is on, which it is by default [B: Microsoft Learn, "Winsock
IOCTLs", `SIO_UDP_CONNRESET`, `SIO_UDP_NETRESET`]. The error belongs to no datagram being received. A fleet node's
first probes reach peers not yet bound, so the ports answer unreachable. `netsys::recv_from` returned the code as a
refusal, and the demultiplexer's receive loop (`crates/transport/src/demux.rs` `Demux::run`) ends on any refusal. So
the node's socket stopped receiving at all. Linux and macOS report no ICMP error to an unconnected socket, which is why
only Windows lost its fleet.

## Measured (CI, 2026-11-06, `diag-liveness-3`, both Windows runners)

- The failing test `a_socket_that_sent_to_a_closed_port_still_receives_from_a_live_peer` failed on main on both
  runners. arm64's answer was `the OS refused recvfrom (code Some(10054))` for the bound socket, and windows-latest's
  failed on the same assertion. With the fix it passed on both, with the rest of `crates/rt/tests/udp.rs` (12 of 12),
  `slates-rt`'s lib tests (41 of 41) and every `slates-transport` suite.
- Conformance's live fleet (`cargo xtask conformance run --suite fsx --live-ship` on its branch f2f807db), two runs
  each:
  - Without the fix, all 4 runs failed. Twice the fleet did not form within 40 s, with nodes probing 1, 0 and 1, and
    1, 1 and 2, of their 2 peers. Once a `volume create` was refused `ConsensusNotInitialized`. Once fsx missed its
    verdict over the live-shipped volume.
  - With the fix laid over the same tree, all 4 formed and ran fsx's 10,000 operations to their reviewed verdict.
- No reply is lost to the report: the test's live datagram arrives whole after it. The "a reset discards unread
  responses" fact is a TCP close's, not a UDP report's.

## Exact edits

- `crates/rt/src/netsys.rs`: `ignore_earlier_datagrams_errors` turns `SIO_UDP_CONNRESET` and `SIO_UDP_NETRESET` off
  with `WSAIoctl` on every datagram socket the Windows seam makes (`dgram_socket`) or adopts (`adopt`). A refusal of
  either call refuses the socket.
- `crates/rt/tests/udp.rs`: the failing test, over a bound socket and an adopted one.
- `unsafe-budget.toml`: `slates-rt` 95 → 96.

## Not done, and why

A residual WSAECONNRESET from `recvfrom` is not mapped to a counted "peer unreachable". With both reports off,
Windows gives no such code on a socket this seam made or adopted, so the path could not run or be tested (CLAUDE.md
§2 item 7). The demultiplexer still ends on any other receive refusal. Whether a receive loop should outlive a
refusal it does not understand is a separate question, left to its owner.
