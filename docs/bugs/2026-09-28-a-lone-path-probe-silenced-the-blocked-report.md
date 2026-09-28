# A lone path-MTU probe silenced the blocked report and deadlocked a session

Date: 2026-09-28. Design: §4.10a (RFC 9000 §19.12–19.14 blocked reports; RFC 8899 path MTU discovery).
Found by `concurrent_exchanges_past_the_stream_limit_survive_every_hostile_path_and_leak_nothing` (5% random
loss, seed 3) while path MTU discovery was being wired in. It never shipped.

## Evidence

A temporary per-tick trace of the client, at 120 virtual seconds:
`send_streams: 16, unacked: 0, retransmit: 0, in_flight: 0, path_probe: 1, next_timeout=None`. The client
held data it could not send (its stream credit was spent), had nothing to recover, armed no timer, and sent
nothing more. Neither did the server.

## Root cause

A credit-blocked sender with nothing in flight tells the peer it is blocked, because no probe timer runs to
recover a lost credit update. `poll_transmit` sends those reports only when `sent.in_flight_count() == 0`.
A path-MTU probe is in flight too. After the credit-carrying acknowledgement was lost, the probe was the one
packet out, so the count was 1: the report was never sent and the session deadlocked. The probe carries
nothing to recover, so it must not stand in for traffic.

## Fix

- **One definition of in flight.** `Connection::recoverable_in_flight()` counts every packet in flight but
  the lone path probe, and the blocked reports, the probe timeout and the census all use it.
- **The probe timeout ignores a lone probe.** A probe alone in flight arms no probe timeout. Its fate is
  read from the next traffic's acknowledgements (RFC 8899 §5.1.1 lets a search wait on traffic).
- **Copies skip the probe.** A timeout copy never copies a `Ping`-only packet.
- **Census.** The census reports the probe separately (`path_probe`, at most one by construction). A
  quiescent session may hold it.

## Test

`a_blocked_sender_reports_even_with_a_path_probe_in_flight` (`connection.rs`):
- **Setup:** the sender spends its connection credit, all its data is acknowledged, and a probe goes out
  and is lost.
- **The step:** the acknowledgement arrives with its credit stripped.
- **Expected:** the next packet carries the blocked report.
- **Check:** it fails with the old `in_flight_count() == 0` condition and passes with the fix. The exchange
  test passes on every path and seed.
