//! Packet-number encoding and decoding for the owned QUIC dialect (§4.10a; RFC 9000 §17.1 with the
//! sample algorithms of Appendix A). QUIC does not put the full 62-bit packet number on the wire: it
//! sends only the least-significant 1–4 bytes, and the receiver reconstructs the full number from the
//! largest packet it has already processed. This module is that codec, pure and total — no I/O, no
//! keys — so it is oracle- and property-testable on every host before the header format (which carries
//! these bytes, header-protected) and the reliability layer (whose ACKs reference full numbers) build
//! on it.
//!
//! Evidence: RFC 9000 (IETF standard, tier A). The encoder realises Appendix A.2's requirement in its
//! own words — *use a size able to represent more than twice the range between the largest acknowledged
//! packet and the one being sent* — so a reordered or delayed acknowledgement can never make the peer
//! reconstruct the wrong number. The decoder is Appendix A.3's algorithm verbatim, computed in `i128`
//! so the window arithmetic cannot underflow near zero. Both are anchored to the RFC's worked example
//! as a golden vector.

/// The most bytes a packet number occupies on the wire.
/// Format: RFC 9000 §17.1 — the packet number field is 1 to 4 bytes; its length is carried in the two
/// low bits of the (header-protected) first header byte. A protocol constant, not a tunable.
const MAX_PACKET_NUMBER_BYTES: u32 = 4;

/// The QUIC packet-number space ceiling: numbers live in `[0, 2^62)`.
/// Format: RFC 9000 §17.1 — "packet numbers ... an integer in the range 0 to 2^62-1". A protocol
/// constant. Used to clamp a reconstructed number to a value the space can actually hold.
const PACKET_NUMBER_SPACE_BITS: u32 = 62;

/// A packet number truncated for the wire: its `len` (1–4) least-significant bytes, big-endian (QUIC
/// network byte order), in the first `len` bytes of `bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodedPacketNumber {
  /// The truncated bytes, big-endian, in `bytes[..len]`.
  bytes: [u8; MAX_PACKET_NUMBER_BYTES as usize],
  /// How many of `bytes` are significant (1–4).
  len: usize,
}

impl EncodedPacketNumber {
  /// The significant bytes, big-endian — what goes in the packet-number field on the wire.
  pub fn as_slice(&self) -> &[u8] {
    &self.bytes[..self.len]
  }

  /// How many bytes the field occupies (1–4); the value the first header byte's low bits encode.
  pub fn len(&self) -> usize {
    self.len
  }

  /// Whether the field is empty. Never true — a packet number is always at least one byte — but
  /// clippy asks for it alongside `len`.
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }
}

/// Chooses the packet-number length and truncates `full_pn` for the wire, given the largest packet
/// number the peer has acknowledged (`None` before the first acknowledgement). RFC 9000 Appendix A.2:
/// the field must be able to represent *more than twice* the range between the largest acknowledged
/// packet and this one, so that whatever the peer's true largest-received number is (somewhere between
/// what it has acknowledged and this packet), it reconstructs the right number. That requirement is
/// `2^(8·bytes) > 2·num_unacked`, applied directly here rather than through a `log2` that rounds.
pub fn encode_packet_number(full_pn: u64, largest_acked: Option<u64>) -> EncodedPacketNumber {
  let num_unacked = match largest_acked {
    // The gap to the largest acknowledged packet (the peer's largest-received is at least this).
    Some(acked) => full_pn.saturating_sub(acked),
    // Nothing acknowledged yet: the whole space up to and including this packet is in flight.
    None => full_pn.saturating_add(1),
  };
  // The window must strictly exceed twice the gap (Appendix A.2). Grow the field until it does, up to
  // the 4-byte maximum. `u128` so `twice_gap` and the shifted window never overflow.
  let twice_gap = 2u128 * u128::from(num_unacked);
  let mut bytes: u32 = 1;
  while bytes < MAX_PACKET_NUMBER_BYTES && (1u128 << (u8::BITS * bytes)) <= twice_gap {
    bytes += 1;
  }
  // Take the least-significant `bytes` of the full number, big-endian.
  let all = full_pn.to_be_bytes();
  let len = bytes as usize;
  let start = all.len() - len;
  let mut out = [0u8; MAX_PACKET_NUMBER_BYTES as usize];
  out[..len].copy_from_slice(&all[start..]);
  EncodedPacketNumber { bytes: out, len }
}

/// Reconstructs the full packet number from the `truncated` wire bytes (big-endian, 1–4 of them) and
/// `largest_pn`, the largest packet number already processed in this space. RFC 9000 Appendix A.3,
/// computed in `i128`: the result is the value congruent to `truncated` modulo the window that lies
/// nearest to `largest_pn + 1`, resolving a wrap in either direction. An empty slice cannot occur from
/// a well-formed header (the field is 1–4 bytes) and yields the expected next number; more than four
/// bytes are ignored past the fourth.
pub fn decode_packet_number(largest_pn: u64, truncated: &[u8]) -> u64 {
  let len = truncated.len().min(MAX_PACKET_NUMBER_BYTES as usize);
  if len == 0 {
    return largest_pn.saturating_add(1);
  }
  let mut truncated_pn: u64 = 0;
  for &byte in &truncated[..len] {
    truncated_pn = (truncated_pn << 8) | u64::from(byte);
  }
  let pn_bits = len * (u8::BITS as usize);
  // Appendix A.3, in i128 so `expected - pn_hwin` cannot underflow near zero.
  let expected = i128::from(largest_pn) + 1;
  let pn_win = 1i128 << pn_bits;
  let pn_hwin = pn_win / 2;
  let pn_mask = pn_win - 1;
  let candidate = (expected & !pn_mask) | i128::from(truncated_pn);
  let space = 1i128 << PACKET_NUMBER_SPACE_BITS;
  let reconstructed = if candidate <= expected - pn_hwin && candidate < space - pn_win {
    candidate + pn_win
  } else if candidate > expected + pn_hwin && candidate >= pn_win {
    candidate - pn_win
  } else {
    candidate
  };
  // The result is a valid number in `[0, 2^62)`; clamp defensively (the branches above already keep it
  // in range for well-formed input), so the `i128 → u64` conversion is always in range.
  u64::try_from(reconstructed.clamp(0, space - 1)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
  use super::*;
  use proptest::prelude::*;

  /// The QUIC packet-number space ceiling as a `u64`, for generators.
  const SPACE: u64 = 1u64 << PACKET_NUMBER_SPACE_BITS;

  /// AC (§4.10a): the RFC 9000 Appendix A.3 worked example decodes exactly as the RFC states —
  /// largest processed `0xa82f30ea`, truncated `0x9b32` (16 bits), reconstructs to `0xa82f9b32`.
  #[test]
  fn the_rfc_appendix_a3_example_decodes() {
    let decoded = decode_packet_number(0xa82f_30ea, &[0x9b, 0x32]);
    assert_eq!(decoded, 0xa82f_9b32, "RFC 9000 A.3 worked example");
  }

  /// AC (§4.10a): the same numbers encode back to the RFC's two wire bytes — a packet `0xa82f9b32`
  /// with `0xa82f30ea` acknowledged needs 16 bits, i.e. the bytes `0x9b 0x32`.
  #[test]
  fn the_rfc_appendix_a3_example_encodes() {
    let encoded = encode_packet_number(0xa82f_9b32, Some(0xa82f_30ea));
    assert_eq!(
      encoded.as_slice(),
      &[0x9b, 0x32],
      "RFC 9000 A.3 example, encoded"
    );
    assert_eq!(encoded.len(), 2);
  }

  /// AC (§4.10a): the field grows exactly at the "more than twice the gap" boundary of Appendix A.2.
  /// With one byte (window 256) the gap must stay under 128; a gap of 128 forces a second byte.
  #[test]
  fn the_field_grows_at_the_twice_the_gap_boundary() {
    // Gap 127: 2·127 = 254 < 256, one byte suffices.
    assert_eq!(encode_packet_number(127, Some(0)).len(), 1);
    // Gap 128: 2·128 = 256, not strictly less than the 256 window, so two bytes are required —
    // the boundary a naive floor(log2)+1 sizing gets wrong.
    assert_eq!(encode_packet_number(128, Some(0)).len(), 2);
  }

  /// A first packet (nothing acknowledged) with a small number takes a single byte.
  #[test]
  fn a_first_small_packet_is_one_byte() {
    let encoded = encode_packet_number(0, None);
    assert_eq!(encoded.as_slice(), &[0]);
  }

  /// A hostile decode: an over-long slice is read as its first four bytes, never panicking or reading
  /// out of bounds.
  #[test]
  fn an_overlong_truncated_field_is_bounded_to_four_bytes() {
    let five = [0x01, 0x02, 0x03, 0x04, 0x05];
    // Decoding with a huge largest_pn keeps the low four bytes 0x01020304 near the expected value.
    let decoded = decode_packet_number(0x01_0203_0300, &five);
    // The fifth byte is ignored; the result is congruent to 0x01020304 mod 2^32.
    assert_eq!(decoded & 0xffff_ffff, 0x0102_0304);
  }

  proptest! {
    /// The behavioural oracle (R5): whatever number a healthy in-order receiver expects, the sender's
    /// chosen field length lets it reconstruct the exact number sent. For any `full_pn` and any
    /// `largest_acked ≤ full_pn`, decoding the encoded field against **any** largest-received value in
    /// the guaranteed range `[largest_acked, full_pn]` — the sender knows the receiver is at least as
    /// far along as what it acknowledged — recovers `full_pn`. Tested at both range endpoints (the
    /// tightest window stress and the in-order common case) and a random point between.
    #[test]
    fn encode_then_decode_recovers_the_number(
      full_pn in 0u64..SPACE,
      gap in 0u64..1_000_000,
      probe in 0u64..=1_000_000,
    ) {
      let largest_acked = full_pn.saturating_sub(gap);
      let encoded = encode_packet_number(full_pn, Some(largest_acked));
      // Any receiver largest-received in [largest_acked, full_pn]; test the two endpoints and a probe.
      let span = full_pn - largest_acked;
      let mids = [largest_acked, full_pn, largest_acked + probe.min(span)];
      for largest_pn in mids {
        let decoded = decode_packet_number(largest_pn, encoded.as_slice());
        prop_assert_eq!(decoded, full_pn, "largest_pn={}", largest_pn);
      }
    }

    /// The first-packet path (nothing acknowledged) round-trips the same way.
    #[test]
    fn a_first_packet_round_trips(full_pn in 0u64..1_000_000) {
      let encoded = encode_packet_number(full_pn, None);
      // The receiver of a first packet expects 0, i.e. largest_pn is "one before 0"; the in-order
      // case has largest_pn = full_pn - 1 (or, for full_pn 0, decoding against 0's predecessor).
      let largest_pn = full_pn.saturating_sub(1);
      let decoded = decode_packet_number(largest_pn, encoded.as_slice());
      prop_assert_eq!(decoded, full_pn);
    }

    /// Decoding never panics and always yields a number in the valid space, for any largest-received
    /// value and any 1–4 byte field (hostile robustness).
    #[test]
    fn decode_is_total_and_in_range(
      largest_pn in 0u64..SPACE,
      field in prop::collection::vec(any::<u8>(), 1..=4),
    ) {
      let decoded = decode_packet_number(largest_pn, &field);
      prop_assert!(decoded < SPACE);
    }
  }
}
