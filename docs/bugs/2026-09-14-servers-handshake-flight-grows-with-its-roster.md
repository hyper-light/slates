# A server's handshake flight grew with the roster it admits, and overflowed the receiver's datagram

Date: 2026-09-14
Area: `crates/transport/src/handshake.rs` (`server_config`, the new `RosterVerifier`),
`crates/transport/src/endpoint.rs` (`DATAGRAM_BYTES`, `EndpointError::FlightTooLarge`,
`establish_turns`), `crates/transport/src/demux.rs`
Severity: a fleet node admitting more than about 40 peers could never be dialed — every handshake to
it faulted at the dialer, forever (`Tls(InvalidMessage(HandshakePayloadTooLarge))`), silently on the
serve side; the KIND lane's five-replica node with a 38-entry roster in one in-process test, and any
real fleet past that size on every node.

## Symptom

The KIND lane's task-budget test (`a_fleet_node_under_a_containers_memory_bound_still_admits_a_client`,
merged from `agent/kind-fleet`) gives node A one dialing peer plus 37 silent ones — a 38-certificate
roster. On main, with the fleet's task share landed, B's probe session to A never formed: B ran 4,000
coordinator periods (407 s) re-dialing, and the daemon log carried one line per attempt:

```
slates-server: fleet: the handshake to 127.0.0.1:57209 faulted: Tls(InvalidMessage(HandshakePayloadTooLarge))
```

Measured first (`handshake::tests::a_servers_handshake_flight_does_not_grow_with_the_roster_it_admits`):
the server's first flight is **694 bytes** admitting one peer and **3,024 bytes** admitting 64 — against
a receive buffer of **2,048 bytes** at every endpoint and the demultiplexer.

## Root cause

Two halves. rustls's web-PKI client verifier lists every trust anchor's subject in the
CertificateRequest's `certificate_authorities` extension (`root_hint_subjects`, RFC 8446 §4.2.4) — about
36 bytes per admitted certificate — so the server's flight grew linearly with the roster. And a
handshake flight is sent whole in one datagram and received into a fixed 2 KiB buffer, so a flight past
it was **truncated on receipt**; rustls then read a handshake header whose length field pointed past the
bytes it had and refused the stream as too large. Nothing on the serve side counted or logged: the
server's flight left intact; the fault was the dialer's.

## Fix

- **The roster is not carried in the handshake.** `server_config` wraps the web-PKI verifier in
  `RosterVerifier`, which delegates every decision (which certificates are admitted, the signature
  checks, the schemes) and answers `root_hint_subjects` with nothing. A fleet peer always presents its one
  enrolled certificate whatever the server hints, so the hints carried nothing; the flight is now the
  same size at any roster (694 bytes at one and at 64 peers).
- **An oversize flight is refused typed at the sender**, never truncated at the receiver:
  `establish_turns` returns `EndpointError::FlightTooLarge { bytes, cap }` before a byte leaves when a
  drained flight exceeds `DATAGRAM_BYTES` — one shared, documented constant now, for the endpoint's four
  receive buffers and the demultiplexer's (which each carried their own 2048).
- Owed, the general form: fragmenting a handshake flight into minimum-size datagrams as CRYPTO frames with
  offsets (RFC 9000 §19.6), which also ends the reliance on IP fragmentation for flights above the path
  MTU; recorded under the transport's MTU item.

## Tests

- `a_servers_handshake_flight_does_not_grow_with_the_roster_it_admits` (failing first: 3,024 ≠ 694; the
  identities are Ed25519 so the signature lengths, and so the flight, are deterministic).
- `crates/transport/tests/session.rs::a_server_admitting_a_large_roster_still_completes_a_dialers_handshake`:
  a server admitting 64 peers, one of them dialing over the simulated fabric, served (before: the dial
  faulted as above).
- The KIND lane's budget test, un-ignored: ok in 1.57 s on the fixed tree (its 38-peer node is dialed
  and admits its client).

## Verification

Validated on a wiped `target/` (2026-09-14 16:57–17:04, load average 6–9): 20/20 fast suites, fmt,
clippy `-D warnings`, `cargo xtask check` (unsafe, literals, structure, version) clean, and the fleet
suite **38/38 in 208.14 s** under the 300 s stall detector — the two tests the KIND branch brought (the
loopback timing split and the budget test) included.

## Sibling sweep

- The client side pins one server certificate, so a dialer's ClientHello never grew; only the accept side
  carried the roster.
- `WebPkiServerVerifier` on the client side has no hint list.
- The demultiplexer's receive loop and the endpoint's 1-RTT reads share the same bound; a 1-RTT packet is
  capped at `MIN_DATAGRAM_BYTES` by the frame cap and was never at risk.
