# Local UDP send pressure was a socket failure (AUD-29-61, runtime seam)

**Date:** 2026-10-01. **Audit:** `docs/audit/2026-09-29_audit.md` AUD-29-61. **Design:** §4.3 (socket readiness), §4.10a.

## Description

`slates_rt::udp::UdpSocket::send_to` turned every non-blocking send error into `RtError::Io`, including the
kernel's "no room now" (`EAGAIN`/`EWOULDBLOCK` on Unix, `WSAEWOULDBLOCK` on Windows). A caller could not tell
a full local send buffer — temporary pressure that clears once the socket is writable — from a failed socket.
There was no way to await write readiness on a UDP socket, and the simulated fabric accepted every
non-oversized datagram, so no test could reproduce kernel send saturation.

## Root cause

`netsys::send_to` returned `Result<usize, RtError>` and mapped every errno to `RtError::Io`, unlike its
receive sibling, which already returned the three-way `Io` (`Ready`, `WouldBlock`, `Interrupted`). The
readiness seam registered `Writable` interest on some drivers only, and `UdpSocket` exposed `readable` alone.

## Impact

A DNS query (`crates/server/src/dns.rs`) sent into a full buffer failed the lookup instead of waiting. The
fleet endpoint (`crates/transport/src/endpoint.rs`, `flush`) inherits the same error after `poll_transmit`
has advanced its send state — the half of AUD-29-61 that lives in the endpoint.

## Exact edits

- `crates/rt/src/error.rs`: `RtError::WouldBlock { call }`, local pressure typed apart from failure.
- `crates/rt/src/netsys.rs`: `send_to` returns `Io<usize>` on Unix and Windows, like `recv_from`.
- `crates/rt/src/udp.rs`: `try_send_to` (`Ok(None)` when nothing was sent, `Interrupted` retried in place);
  `send_to` keeps its shape and refuses `WouldBlock`; `writable` awaits write readiness through the driver;
  `send_to_writable` loops try/await with no spin.
- `crates/rt/src/readiness.rs`: `Interest::Writable` on every platform's driver.
- `crates/rt/src/sim.rs`: send pressure on the fabric (`sim_udp_block_sends`, `sim_udp_release_sends`,
  `sim_udp_sends_blocked`) and a simulated `register_writable` that wakes on release.
- `crates/server/src/dns.rs`: the query awaits writability (`send_to_writable`). The reply path stays a
  best-effort non-blocking send: the client re-asks after its own timeout, and a server task never waits on
  one client's buffer.

## Proof

`crates/rt/tests/udp.rs` `a_send_under_local_pressure_waits_for_writability_and_sends_once`: with the
sender's port blocked, `send_to` is `WouldBlock`, `try_send_to` is `None`, and nothing reaches the receiver;
`send_to_writable` completes only after the release at 1 ms of virtual time, and the datagram arrives
exactly once. Before the change the seam did not exist (no typed would-block, no write readiness, no
simulated pressure). The UDP suite passes 10/10 on macOS and on Linux (Docker, `rust:1.98.0`); `slates-rt`
cross-lints clean for `x86_64-pc-windows-msvc`.

## Carried elsewhere

The endpoint half of AUD-29-61 — prepare, accepted send and a bounded pending datagram, so a packet is
recorded sent (and its RTT and loss clocks started) only when the OS accepts it — is inside the QUIC layer
that Ada's 2026-10-01 directive moves to `hyper-quic` (quinn-proto's `poll_transmit` then a socket send
that keeps the transmit until accepted). It is carried there, not patched into `crates/transport` (banned
item 7). The vendored endpoint's I/O loop must use `try_send_to` and `writable` from this seam.
