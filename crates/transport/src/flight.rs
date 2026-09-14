//! Handshake-flight fragmentation (§4.10a; RFC 9000 §19.6 CRYPTO frames, in slates' owned dialect).
//!
//! A handshake flight — the bytes `rustls::quic::Connection::write_hs` produces for one direction of the
//! TLS 1.3 handshake — can be larger than a single datagram: a mutual-TLS server flight carries the
//! server's certificate chain, and an operator's chain of a leaf plus an intermediate or two exceeds the
//! [`MIN_DATAGRAM_BYTES`](crate::endpoint::MIN_DATAGRAM_BYTES) path floor. Before this the whole flight
//! rode one datagram, so a flight above the receiver's buffer was refused typed
//! (`EndpointError::FlightTooLarge`) and a flight above the path MTU relied on IP fragmentation (which a
//! router may drop). This splits a flight into fragments that each fit the path floor and reassembles
//! them at the peer, so any chain a real deployment provisions handshakes.
//!
//! **Framing.** A fragment is `[tag][offset: u16 LE][total: u16 LE][payload…]`. The tag is
//! [`FRAGMENT_TAG`], chosen with the QUIC fixed bit (`0x40`) **clear** so the demultiplexer and the
//! endpoint still route it as a handshake datagram, not a 1-RTT packet
//! ([`crate::endpoint::is_short_header`] stays the discriminator), and distinct from any TLS 1.3
//! handshake-message first byte (a message type, all `< 0x40`) so a fragment is never mistaken for a bare
//! flight. `offset` is the fragment's start in the flight and `total` the flight's whole length, so a
//! receiver reassembles without a separate length negotiation and detects completion. Both are `u16`:
//! a handshake flight is bounded by the certificate chain, far under 64 KiB, and the endpoint caps a
//! flight at [`MAX_FLIGHT_BYTES`] before fragmenting so a hostile or corrupt `total` cannot allocate
//! without bound.
//!
//! **Reliability.** Fragmentation is stateless and deterministic: the same flight fragments to the same
//! bytes every time, so a retransmit of a flight (the handshake's own loss recovery) resends identical
//! fragments, and the [`Reassembler`] is idempotent under duplicates and reorder — it holds the bytes it
//! has by offset and yields the flight once every byte is present. One handshake runs at a time per
//! endpoint, so one reassembler per endpoint suffices; a flight delivered whole is remembered so an
//! immediate re-delivery (the peer retransmitting a flight this end already consumed) is reported as a
//! repeat rather than fed to the TLS state a second time.

use crate::endpoint::{MIN_DATAGRAM_BYTES, is_short_header};

/// Format: the first byte of a fragment — the QUIC fixed bit (0x40) clear so it routes as a handshake
/// datagram, the high bit set so it is distinct from every TLS 1.3 handshake-message type (all `< 0x40`)
/// and from a 1-RTT short header (fixed bit set). RFC 9000 §17.2/§17.3: long-header form has the fixed
/// bit's neighbour set; here the one tag suffices because the dialect carries no other long-header packet.
pub const FRAGMENT_TAG: u8 = 0x80;

/// Format: the fragment header — the tag, the offset (`u16` LE), the flight's total length (`u16` LE).
pub const FRAGMENT_HEADER: usize = 1 + 2 + 2;

/// Derived: the most flight bytes one fragment carries — the path-floor datagram less the header, so a
/// fragment never needs IP fragmentation. Anchored to [`MIN_DATAGRAM_BYTES`].
pub const FRAGMENT_PAYLOAD: usize = MIN_DATAGRAM_BYTES - FRAGMENT_HEADER;

/// Shape: the largest handshake flight the endpoint fragments or reassembles — the reassembler will not
/// buffer past it, so a corrupt or hostile `total` cannot allocate without bound. A mutual-TLS flight
/// with a certificate chain of several certificates is a few kilobytes; this is the power of two above a
/// generous chain (sixteen path-floor fragments).
pub const MAX_FLIGHT_BYTES: usize = 16 * MIN_DATAGRAM_BYTES;

/// Splits `flight` into fragments, each at most one path-floor datagram. A flight that already fits one
/// fragment produces one fragment (never zero: an empty flight is never sent, but an empty input yields a
/// single empty-payload fragment so the peer still learns `total = 0`). `None` if the flight is longer
/// than [`MAX_FLIGHT_BYTES`] — the caller refuses it typed rather than sending an unreassemblable stream.
pub fn fragment(flight: &[u8]) -> Option<Vec<Vec<u8>>> {
  if flight.len() > MAX_FLIGHT_BYTES {
    return None;
  }
  let total = u16::try_from(flight.len()).ok()?;
  let mut out = Vec::new();
  let mut offset = 0usize;
  loop {
    let end = flight.len().min(offset + FRAGMENT_PAYLOAD);
    let mut datagram = Vec::with_capacity(FRAGMENT_HEADER + (end - offset));
    datagram.push(FRAGMENT_TAG);
    datagram.extend_from_slice(&u16::try_from(offset).ok()?.to_le_bytes());
    datagram.extend_from_slice(&total.to_le_bytes());
    datagram.extend_from_slice(&flight[offset..end]);
    out.push(datagram);
    offset = end;
    if offset >= flight.len() {
      // The `>=` (not `==`) with the empty-flight case: `0 >= 0` stops after the one empty fragment.
      break;
    }
  }
  Some(out)
}

/// Whether `datagram` is a handshake fragment (routed as a handshake datagram and tagged as a fragment,
/// not a bare flight). A datagram that is not a 1-RTT packet and not a fragment is a bare handshake
/// datagram from an endpoint that does not fragment — which this dialect no longer produces, but the
/// check keeps the classification total.
pub fn is_fragment(datagram: &[u8]) -> bool {
  !is_short_header(datagram) && datagram.first() == Some(&FRAGMENT_TAG)
}

/// What pushing a fragment into a [`Reassembler`] yields.
#[derive(Debug, PartialEq, Eq)]
pub enum Reassembly {
  /// More fragments are needed; nothing to feed the TLS state yet.
  Pending,
  /// The flight is complete: its bytes, to feed to the TLS state once.
  Flight(Vec<u8>),
  /// The flight is complete but identical to the one just delivered (a peer retransmit of a flight this
  /// end already consumed): the caller must **not** feed it to the TLS state again (a repeated
  /// `read_hs` of a consumed flight faults the stream), but it is a live sign the peer is still asking.
  Repeat,
  /// The fragment did not parse, or named an offset or total past [`MAX_FLIGHT_BYTES`] or inconsistent
  /// with a fragment already held: dropped and counted by the caller, never a fault.
  Malformed,
}

/// Reassembles one handshake flight from its fragments (see the module doc). One per endpoint.
#[derive(Debug, Default)]
pub struct Reassembler {
  /// The flight's total length, once a fragment has announced it; `None` between flights.
  total: Option<usize>,
  /// The bytes received so far, sized to `total`; a fragment writes its slice, so duplicates and reorder
  /// are idempotent.
  bytes: Vec<u8>,
  /// Which byte offsets have arrived (one flag per byte): the flight is complete when all `total` are
  /// set. A byte count would miscount an overlapping duplicate; per-byte presence is exact.
  present: Vec<bool>,
  /// How many distinct bytes are present, so completion is an integer compare, not a scan.
  filled: usize,
  /// The last flight delivered whole, to recognize a peer's retransmit of an already-consumed flight.
  last_delivered: Vec<u8>,
}

impl Reassembler {
  /// A fresh reassembler.
  pub fn new() -> Reassembler {
    Reassembler::default()
  }

  /// Pushes one received fragment. Returns whether the flight is now complete (and whether it repeats the
  /// last one delivered), needs more, or the fragment was malformed.
  pub fn push(&mut self, datagram: &[u8]) -> Reassembly {
    let Some((offset, total, payload)) = parse(datagram) else {
      return Reassembly::Malformed;
    };
    if total > MAX_FLIGHT_BYTES || offset.saturating_add(payload.len()) > total {
      return Reassembly::Malformed;
    }
    // The first fragment of a flight sizes the buffer; a later fragment naming a different total is from
    // another flight (or corrupt) — refuse it rather than mix two flights' bytes.
    match self.total {
      None => self.begin(total),
      Some(seen) if seen != total => return Reassembly::Malformed,
      Some(_) => {}
    }
    for (i, byte) in payload.iter().enumerate() {
      let at = offset + i;
      if !self.present[at] {
        self.present[at] = true;
        self.bytes[at] = *byte;
        self.filled += 1;
      }
    }
    if self.filled < total {
      return Reassembly::Pending;
    }
    let flight = std::mem::take(&mut self.bytes);
    self.reset();
    if flight == self.last_delivered {
      Reassembly::Repeat
    } else {
      self.last_delivered = flight.clone();
      Reassembly::Flight(flight)
    }
  }

  /// Begins a flight of `total` bytes (sizes the buffers).
  fn begin(&mut self, total: usize) {
    self.total = Some(total);
    self.bytes = vec![0u8; total];
    self.present = vec![false; total];
    self.filled = 0;
  }

  /// Clears the in-progress flight (its bytes were taken); keeps `last_delivered` for repeat detection.
  fn reset(&mut self) {
    self.total = None;
    self.present = Vec::new();
    self.filled = 0;
  }
}

/// Parses a fragment into `(offset, total, payload)`, or `None` if it is not a well-formed fragment.
fn parse(datagram: &[u8]) -> Option<(usize, usize, &[u8])> {
  if datagram.first() != Some(&FRAGMENT_TAG) || datagram.len() < FRAGMENT_HEADER {
    return None;
  }
  let offset = usize::from(u16::from_le_bytes([datagram[1], datagram[2]]));
  let total = usize::from(u16::from_le_bytes([datagram[3], datagram[4]]));
  Some((offset, total, &datagram[FRAGMENT_HEADER..]))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A pseudo-random stream for the oracle, seeded so a failure replays (`xorshift64*`).
  struct Rng(u64);
  impl Rng {
    fn next(&mut self) -> u64 {
      let mut x = self.0;
      x ^= x >> 12;
      x ^= x << 25;
      x ^= x >> 27;
      self.0 = x;
      x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
      if n == 0 { 0 } else { usize::try_from(self.next() % n as u64).unwrap_or(0) }
    }
  }

  /// Do: fragment a flight, then push its fragments to a reassembler in an arbitrary order with
  /// duplicates. Expect: the flight comes back exactly, once, and every push before the last is
  /// `Pending` (or a duplicate that stays `Pending`). Oracle over 2,000 random flights and orders.
  #[test]
  fn a_flight_reassembles_from_its_fragments_in_any_order_with_duplicates() {
    let mut rng = Rng(0x51A7_u64 ^ 0x9E37_79B9_7F4A_7C15);
    for _ in 0..2000 {
      let len = rng.below(MAX_FLIGHT_BYTES + 1);
      let flight: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(7)).collect();
      let fragments = fragment(&flight).expect("within the bound");
      assert!(
        fragments.iter().all(|f| f.len() <= MIN_DATAGRAM_BYTES),
        "every fragment fits the path floor"
      );
      // A shuffled sequence with each fragment sometimes duplicated.
      let mut order: Vec<usize> = Vec::new();
      for i in 0..fragments.len() {
        order.push(i);
        if rng.below(2) == 0 {
          order.push(i);
        }
      }
      for i in (1..order.len()).rev() {
        order.swap(i, rng.below(i + 1));
      }
      let mut reassembler = Reassembler::new();
      let mut delivered: Option<Vec<u8>> = None;
      let complete_at = last_new_index(&order, fragments.len());
      for (step, &which) in order.iter().enumerate() {
        match reassembler.push(&fragments[which]) {
          Reassembly::Flight(f) => {
            assert!(delivered.is_none(), "the flight is delivered once");
            assert_eq!(step, complete_at, "delivered exactly when the last new byte arrived");
            delivered = Some(f);
          }
          Reassembly::Pending => {}
          other => panic!("unexpected {other:?} at step {step}"),
        }
      }
      assert_eq!(delivered.as_deref(), Some(flight.as_slice()), "the flight came back exactly");
    }
  }

  /// The step at which the last not-yet-seen fragment index appears in `order`.
  fn last_new_index(order: &[usize], count: usize) -> usize {
    let mut seen = vec![false; count];
    let mut remaining = count;
    for (step, &which) in order.iter().enumerate() {
      if !seen[which] {
        seen[which] = true;
        remaining -= 1;
        if remaining == 0 {
          return step;
        }
      }
    }
    order.len().saturating_sub(1)
  }

  /// A flight re-delivered whole (a peer retransmitting a flight already consumed) is reported `Repeat`,
  /// not fed again; a different flight after it is delivered fresh.
  #[test]
  fn a_re_delivered_flight_is_a_repeat_then_a_new_flight_is_fresh() {
    let first: Vec<u8> = (0..1500u16).map(|i| i as u8).collect();
    let second: Vec<u8> = (0..1500u16).map(|i| (i as u8) ^ 0xFF).collect();
    let mut reassembler = Reassembler::new();
    let f1 = fragment(&first).unwrap();
    for frag in &f1 {
      let _ = reassembler.push(frag);
    }
    // Re-deliver the same flight: complete again, but a repeat.
    let mut repeat = Reassembly::Pending;
    for frag in &f1 {
      repeat = reassembler.push(frag);
    }
    assert_eq!(repeat, Reassembly::Repeat, "the same flight re-delivered is a repeat");
    let f2 = fragment(&second).unwrap();
    let mut fresh = Reassembly::Pending;
    for frag in &f2 {
      fresh = reassembler.push(frag);
    }
    assert_eq!(fresh, Reassembly::Flight(second), "a different flight after is fresh");
  }

  /// Hostile fragments: a truncated header, a `total` past the bound, an offset+payload past `total`, and
  /// a fragment naming a different total mid-flight are each `Malformed` — dropped, never a panic.
  #[test]
  fn hostile_fragments_are_malformed_never_a_fault() {
    let mut reassembler = Reassembler::new();
    assert_eq!(reassembler.push(&[FRAGMENT_TAG, 0]), Reassembly::Malformed, "truncated header");
    let mut too_big = vec![FRAGMENT_TAG];
    too_big.extend_from_slice(&0u16.to_le_bytes());
    too_big.extend_from_slice(&u16::MAX.to_le_bytes());
    assert_eq!(reassembler.push(&too_big), Reassembly::Malformed, "total past the bound");
    let mut past = vec![FRAGMENT_TAG];
    past.extend_from_slice(&10u16.to_le_bytes());
    past.extend_from_slice(&4u16.to_le_bytes());
    past.extend_from_slice(&[1, 2, 3, 4]);
    assert_eq!(reassembler.push(&past), Reassembly::Malformed, "offset past total");
    // A first good fragment, then one naming a different total.
    let good = &fragment(&vec![9u8; 1500]).unwrap()[0];
    assert_eq!(reassembler.push(good), Reassembly::Pending);
    let mut other_total = vec![FRAGMENT_TAG];
    other_total.extend_from_slice(&0u16.to_le_bytes());
    other_total.extend_from_slice(&999u16.to_le_bytes());
    other_total.push(0);
    assert_eq!(reassembler.push(&other_total), Reassembly::Malformed, "a different total mid-flight");
  }

  /// A single-fragment flight (the common case: a flight under the path floor) fragments to one datagram
  /// and reassembles from it — the path a normal handshake takes, unchanged in cost.
  #[test]
  fn a_small_flight_is_one_fragment() {
    let flight = vec![0x0bu8; 700];
    let fragments = fragment(&flight).unwrap();
    assert_eq!(fragments.len(), 1, "one fragment under the path floor");
    assert!(is_fragment(&fragments[0]), "and it routes as a fragment");
    let mut reassembler = Reassembler::new();
    assert_eq!(reassembler.push(&fragments[0]), Reassembly::Flight(flight));
  }
}
