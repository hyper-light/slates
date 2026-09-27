# The session plane's probes hid tail losses, and loss had no time threshold

Date: 2026-09-27. Contracts: §4.10a §8 (the session plane follows RFC 9002), the constrained-link design
(`docs/wip/research/nfs-transport-constrained-links.md` §5.3). Found while giving the connection a clock
for the congestion-control bake-off, by the connection's own oracle tests.

## Symptom

- A persistent-congestion test of the new clocked connection could not produce persistent congestion from
  a path that dropped everything for half a second.
- Reading `SentTracker::probe_oldest` against RFC 9002 §6.2.4 showed why: a probe timeout removed the
  oldest in-flight packet from tracking and requeued its frames. The packet was never acknowledged and
  never declared lost; it vanished.

## Root cause

1. **Probes removed the original packet.** RFC 9002 §6.2.4 sends a probe (new data, or a copy of
   unacknowledged data) while the original stays in flight, to be acknowledged or declared lost by the
   thresholds once the probe's acknowledgement arrives. Removing the original meant:
   - a tail loss recovered by a probe was never reported as a loss, so no congestion controller backed
     off for it;
   - the persistent-congestion test (§7.6.2) never saw the full span of lost packets;
   - the bytes left the in-flight count early.
2. **No time-threshold loss detection.** Loss was declared only by the packet threshold (three later
   packets acknowledged, §6.1.1). The time threshold (9/8 of the RTT, §6.1.2) was missing. So a loss with
   fewer than three packets after it waited for a full probe timeout, and reordering within a round trip
   could not be told from loss by time.

## Fix

- `SentTracker::copy_oldest` gives a probe a copy of the oldest packet's frames. The original stays in
  flight. `probe_oldest` is deleted.
- `SentTracker::take_lost(now, loss_delay)` declares a packet lost by either threshold, and reports the
  next time-threshold expiry. The connection arms a loss timer on it, `next_timeout`/`on_timeout`.
- A probe the timeout owes, but cannot send because no data can leave, no longer leaves the timer firing
  at the same instant. The probe checks the connection credit before choosing new data over a copy, and
  an owed probe suppresses the timer until it is sent. This case was introduced and caught within this
  change.

## Evidence

`crates/transport/src/connection.rs` tests:
- `the_time_threshold_declares_a_loss_the_packet_threshold_cannot` (the loss timer, not a probe);
- `persistent_congestion_collapses_the_window` (losses spanning three probe timeouts collapse the window
  to the minimum);
- `a_single_packet_tail_loss_is_probed` under every control law;
- the loss and reorder proptest oracles, under every control law;
- `crates/transport/src/conn.rs` `a_probe_copies_the_oldest_in_flight_packet`.
