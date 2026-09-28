# Packets grew past the datagram floor, and credit could strand a blocked sender

Date: 2026-09-28. Contracts: RFC 9000 §14 (a packet must fit the path; 1,200 bytes is the floor every path
carries), §13.2.4 (bounding acknowledgement state), §19.12–19.13 (`DATA_BLOCKED` / `STREAM_DATA_BLOCKED`);
CLAUDE.md "unbounded growth" ban. Found by the scheduler bake-off: at 100 Mbit/s and 100 ms with **no
loss**, the strict-priority and weighted schedulers got 17 % of the link against round-robin's 91 %.

## Symptom and evidence

A trace of that run (window, send-stop reasons, the fabric's drop counters, every 100 virtual ms) showed:

- **The window collapsed with nothing dropped.** Strict priority's window was halved twice
  (381 KB → 239 KB → 119 KB) while the fabric counted zero drops and the queue peaked at 3.8 KB.
- **The losses were spurious.** A per-loss diagnostic (added, run once, removed) showed each declared
  loss preceded by a received datagram that filled the 2,048-byte receive buffer, i.e. was truncated.
- **Oversized datagrams on both sides.** They were up to 2,048 bytes on the wire (2,684 datagrams past
  1,200 in one run). The simulated path had no MTU set, so the fabric never counted them as drops. A real
  path at the floor would have dropped them.

## Root causes

1. **The packet budget counted only stream data.** `poll_transmit` filled a packet with up to the budget of
   stream bytes, then appended the acknowledgement, the connection credit and **one `MaxStreamData` per
   open receive stream**, without counting them. With many exchanges open, packets grew with the number of
   streams. The fleet's `FLEET_PACKET_OVERHEAD = 80` estimate covered one frame header and nothing else.
   The swim and WAN tests used the full 1,200 as their budget.
2. **Credit could strand a blocked sender.** Credit rode only acknowledgement-only packets, which are never
   retransmitted. If the one carrying fresh credit was lost while the sender was fully credit-blocked with
   nothing in flight, no probe timer ran and neither side sent again. Bounding credit per packet (needed
   for 1) made this likely: `a_long_transfer_under_random_loss_completes` stalled, "NewReno seed 1:
   stalled with 0 in flight".
3. **The acknowledgement state grew without bound.** Every received packet number was held individually
   until ACK-of-ACK pruned it. A receiver whose ACKs travel in acknowledgement-only packets, which the peer
   never acknowledges, is never pruned; that is always true of a pure bulk receiver. It grew one entry per
   packet, and every ACK walked them all (O(packets)). Measured: 353 tracked against a bound of 40.

## Fix

1. **Exact packing.** Every frame counts against the budget by its encoded length (`Frame::encoded_len`,
   golden-tested against the encoder). The acknowledgement leads, sized to fit; then control frames,
   retransmissions and fresh data; then stream credit, with moved credit first and the rest refreshed in
   rotation, bounded by the room left. Credit that moved and doesn't fit goes in a credit-only packet.
2. **A derived budget, guarded.** `endpoint::MAX_PACKET_PAYLOAD` (1,171) is the floor less the short
   header and AEAD tag. An endpoint refuses a shape outside `[MIN_PACKET_BUDGET, MAX_PACKET_PAYLOAD]`
   (`EndpointError::PacketBudget`), and refuses any 1-RTT datagram over the floor rather than sending it
   (`EndpointError::DatagramTooLarge`). The fleet budget is now this derived value.
3. **Blocked reports** (`DataBlocked` kind 8, `StreamDataBlocked` kind 9). A credit-blocked sender with
   nothing in flight reports it, reliably. The receiver's acknowledgement carries the credit, with the
   blocked stream's credit marked moved. A report acknowledged while the sender is still blocked at the
   same limit is re-armed, at most once per round trip. With the reports switched off, the loss test fails
   on its first seed; with them, all 20 seeds pass under every controller.
4. **Ranges.** `AckGenerator` holds merged ranges, at most what one acknowledgement can report at the
   budget. Past that, the oldest is dropped and the duplicate floor raised, which is safe because a
   retransmission always rides a fresh packet number. Tested with 100,000 in-order packets (one range) and
   a gappy tail (never past the cap), with the duplicate rules intact.
5. **Stream limit.** The stream limit is now derived from stream data per packet, not the packet budget.

## Sweep

Every shape built in the tree was checked against the new budget rule. The ones that sent oversized
packets were fixed: fleet, swim and WAN tests, and `cluster/tests/content.rs`, which was caught by the new
`PacketBudget` refusal. Tests with tiny budgets now state them as `STREAM_FRAME_HEADER_BYTES + N` data
bytes. The connection tests' wire driver asserts that every packet fits its budget, so every oracle and
proptest checks it.
