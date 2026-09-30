# Packet numbers saturated into a repeat (AUD-29-27)

## Description

- **The repeat.** `SentTracker::next_pn` advanced with `saturating_add(1)`: at `u64::MAX` it returned
  that number again and again. The packet number is an input to the AEAD nonce, so a repeat under the
  same keys is nonce reuse. The control seal already refused its own counter's exhaustion; the session
  tracker did not.
- **Unchecked credit arithmetic.** The flow controller's credit ceilings added with plain `+`.
- **Ordering in the reassembler.** A segment's end was computed saturating, so it depended on the window
  check to refuse an end past the range.

## Root cause

- **The wrong ceiling.** The tracker used `u64::MAX` as its ceiling, instead of the packet-number space
  the format defines (RFC 9000 §17.1: `[0, 2^62)`) and the terminal rule at its end (§12.3: close, never
  reuse).

## Fix

- **The tracker.** `PACKET_NUMBER_SPACE = 2^62` (`packet_number.rs`). `next_pn` returns `Option`, and
  `exhausted()` reports the spent space.
- **The connection.**
  - `poll_transmit` and `poll_probe` check exhaustion before touching any state, so no frame leaves its
    queue.
  - `emit_confirm` returns `Option`.
  - `packet_numbers_exhausted()` reports the spent space.
- **The endpoint.** It ends the session `EndpointError::PacketNumbersExhausted`, from its flush and its
  confirmation. The swim probe judges that session `Broken`.
- **Arithmetic.** The credit ceilings saturate (a `u64` on the wire, D-15, bounded by what was actually
  received and read), and a segment whose end overflows is refused `BeyondWindow` before anything is
  recorded.

## Evidence

- **`the_packet_number_space_ends_without_a_repeat`.** A tracker seeded two numbers short of `2^62`
  yields both numbers once, then `None` for good.
- **`a_spent_packet_number_space_sends_nothing_and_loses_nothing`.** A spent connection sends no packet,
  probe or confirmation, and its queued data is still there to send under a fresh space.
