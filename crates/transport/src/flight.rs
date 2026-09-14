//! Handshake-flight fragmentation (§4.10a; RFC 9000 §19.6 CRYPTO frames, in slates' owned dialect).
//!
//! A handshake flight — the bytes `rustls::quic::Connection::write_hs` produces for one turn of the
//! TLS 1.3 handshake — can be larger than a single datagram: a mutual-TLS server flight carries the
//! server's certificate chain, and an operator's chain of a leaf plus an intermediate or two exceeds the
//! [`MIN_DATAGRAM_BYTES`](crate::endpoint::MIN_DATAGRAM_BYTES) path floor. Before this the whole flight
//! rode one datagram, so a flight above the receiver's buffer was refused typed
//! (`EndpointError::FlightTooLarge`) and a flight above the path MTU relied on IP fragmentation (which a
//! router may drop). This splits a flight into fragments that each fit the path floor and reassembles
//! them at the peer, so any chain a real deployment provisions handshakes.
//!
//! **The stream.** Each direction of a handshake is one byte stream (RFC 9000 §19.6: CRYPTO offsets are
//! cumulative within an encryption level; slates keeps one level, so one stream per direction). A flight
//! occupies `[start, start + total)` of its sender's stream, where `start` is the count of handshake
//! bytes the sender had sent before it. The receiver keeps how much of the peer's stream it has
//! **consumed** (fed to the TLS state), so a fragment is placed exactly: a flight starting at `consumed`
//! is the next one; a flight starting below it is a retransmit of one already consumed (the peer's
//! reply raced ours — it is still asking, and it must not be fed twice); a flight starting above it is
//! impossible in TLS (the peer cannot advance without our reply) and is dropped as malformed. Naming
//! the flight by its offset, not its content, is what makes a stale fragment of an earlier flight
//! harmless: before this a fragment was placed by its offset *within* a flight alone, so a late
//! fragment of flight N with the same length as flight N + 1 would have written into N + 1's buffer.
//!
//! **Framing.** A fragment is `[tag][start: u16 LE][within: u16 LE][total: u16 LE][payload…]`. The tag
//! is [`FRAGMENT_TAG`], chosen with the QUIC fixed bit (`0x40`) **clear** so the demultiplexer and the
//! endpoint still route it as a handshake datagram, not a 1-RTT packet
//! ([`crate::endpoint::is_short_header`] stays the discriminator), and distinct from any TLS 1.3
//! handshake-message first byte (a message type, all `< 0x40`). `u16` fields suffice: a direction's
//! whole handshake is a few kilobytes, and the sender refuses a stream past [`MAX_STREAM_BYTES`] or a
//! flight past [`MAX_FLIGHT_BYTES`] typed, so a hostile or corrupt field cannot allocate without bound.
//!
//! **Reliability.** Fragmentation is stateless and deterministic: the same flight at the same start
//! fragments to the same bytes every time, so a retransmit of a flight (the handshake's own loss
//! recovery) resends identical fragments, and the [`Reassembler`] is idempotent under duplicates and
//! reorder — it holds the bytes it has by offset and yields the flight once every byte is present.

use crate::endpoint::{MIN_DATAGRAM_BYTES, is_short_header};

/// Format: the first byte of a fragment — the QUIC fixed bit (0x40) clear so it routes as a handshake
/// datagram, the high bit set so it is distinct from every TLS 1.3 handshake-message type (all `< 0x40`)
/// and from a 1-RTT short header (fixed bit set).
pub const FRAGMENT_TAG: u8 = 0x80;

/// Format: the offset of the flight's stream start (`u16` LE) in a fragment: after the tag.
const AT_START: usize = 1;
/// Format: the offset of the fragment's position within the flight (`u16` LE): after the start.
const AT_WITHIN: usize = AT_START + 2;
/// Format: the offset of the flight's total length (`u16` LE): after the position.
const AT_TOTAL: usize = AT_WITHIN + 2;
/// Format: the fragment header — the tag, the flight's stream start (`u16` LE), the fragment's offset
/// within the flight (`u16` LE), the flight's total length (`u16` LE); the payload follows.
pub const FRAGMENT_HEADER: usize = AT_TOTAL + 2;

/// Derived: the most flight bytes one fragment carries — the path-floor datagram less the header, so a
/// fragment never needs IP fragmentation. Anchored to [`MIN_DATAGRAM_BYTES`].
pub const FRAGMENT_PAYLOAD: usize = MIN_DATAGRAM_BYTES - FRAGMENT_HEADER;

/// Shape: the largest handshake flight the endpoint fragments or reassembles — the reassembler will not
/// buffer past it, so a corrupt or hostile `total` cannot allocate without bound. A mutual-TLS flight
/// with a certificate chain of several certificates is a few kilobytes; this is sixteen path-floor
/// fragments, a generous chain.
pub const MAX_FLIGHT_BYTES: usize = 16 * MIN_DATAGRAM_BYTES;

/// Derived: the most fragments one flight spans, for bounding a receive loop. Anchored to
/// [`MAX_FLIGHT_BYTES`] and [`FRAGMENT_PAYLOAD`].
pub const MAX_FLIGHT_FRAGMENTS: usize = MAX_FLIGHT_BYTES.div_ceil(FRAGMENT_PAYLOAD);

/// Format: the most bytes one direction of a handshake may send in all — what the `u16` stream offset
/// can name. A TLS 1.3 direction is a few kilobytes; this is the field's range.
pub const MAX_STREAM_BYTES: usize = u16::MAX as usize;

/// Splits `flight`, which begins at stream offset `start`, into fragments each at most one path-floor
/// datagram. A flight that fits one fragment produces one. `None` if the flight is longer than
/// [`MAX_FLIGHT_BYTES`], or would end past [`MAX_STREAM_BYTES`] — the caller refuses it typed rather
/// than sending an unreassemblable stream. An empty flight produces no fragments (nothing to say).
pub fn fragment(flight: &[u8], start: usize) -> Option<Vec<Vec<u8>>> {
  if flight.len() > MAX_FLIGHT_BYTES || start.checked_add(flight.len())? > MAX_STREAM_BYTES {
    return None;
  }
  let start_word = u16::try_from(start).ok()?;
  let total_word = u16::try_from(flight.len()).ok()?;
  let mut out = Vec::with_capacity(flight.len().div_ceil(FRAGMENT_PAYLOAD));
  let mut within = 0usize;
  while within < flight.len() {
    let end = flight.len().min(within + FRAGMENT_PAYLOAD);
    let mut datagram = Vec::with_capacity(FRAGMENT_HEADER + (end - within));
    datagram.push(FRAGMENT_TAG);
    datagram.extend_from_slice(&start_word.to_le_bytes());
    datagram.extend_from_slice(&u16::try_from(within).ok()?.to_le_bytes());
    datagram.extend_from_slice(&total_word.to_le_bytes());
    datagram.extend_from_slice(&flight[within..end]);
    out.push(datagram);
    within = end;
  }
  Some(out)
}

/// Whether `datagram` is a handshake fragment (routed as a handshake datagram and tagged as a fragment).
pub fn is_fragment(datagram: &[u8]) -> bool {
  !is_short_header(datagram) && datagram.first() == Some(&FRAGMENT_TAG)
}

/// What pushing a fragment into a [`Reassembler`] yields.
#[derive(Debug, PartialEq, Eq)]
pub enum Reassembly {
  /// More fragments of the next flight are needed; nothing to feed the TLS state yet.
  Pending,
  /// The next flight is complete: its bytes, to feed to the TLS state once.
  Flight(Vec<u8>),
  /// A fragment of a flight already consumed (the peer retransmitted it — its reply raced ours): not
  /// to be fed again (a repeated `read_hs` of a consumed flight faults the stream), but a live sign the
  /// peer is still asking, so the caller resends its own flight.
  Repeat,
  /// The fragment did not parse, named a flight past the bounds, one ahead of the stream (impossible in
  /// TLS), or one inconsistent with the fragments already held: dropped and counted by the caller,
  /// never a fault.
  Malformed,
}

/// Reassembles the peer's handshake stream flight by flight (see the module doc). One per endpoint.
#[derive(Debug, Default)]
pub struct Reassembler {
  /// Bytes of the peer's stream consumed so far: the start of the next flight.
  consumed: usize,
  /// The next flight's total length, once a fragment has announced it; `None` between flights.
  total: Option<usize>,
  /// The next flight's bytes received so far, sized to `total`; a fragment writes its slice, so
  /// duplicates and reorder are idempotent.
  bytes: Vec<u8>,
  /// Which bytes of the next flight have arrived (one flag per byte): complete when all are set. A byte
  /// count would miscount an overlapping duplicate; per-byte presence is exact.
  present: Vec<bool>,
  /// How many distinct bytes of the next flight are present, so completion is one compare.
  filled: usize,
}

impl Reassembler {
  /// A fresh reassembler at the start of the peer's stream.
  pub fn new() -> Reassembler {
    Reassembler::default()
  }

  /// Bytes of the peer's stream consumed so far (the start of the flight awaited).
  pub fn consumed(&self) -> usize {
    self.consumed
  }

  /// Pushes one received fragment. Returns whether the next flight is now complete, needs more, was a
  /// retransmit of a consumed flight, or was malformed.
  pub fn push(&mut self, datagram: &[u8]) -> Reassembly {
    let Some((start, within, total, payload)) = parse(datagram) else {
      return Reassembly::Malformed;
    };
    if total > MAX_FLIGHT_BYTES
      || total == 0
      || within.saturating_add(payload.len()) > total
      || start.saturating_add(total) > MAX_STREAM_BYTES
    {
      return Reassembly::Malformed;
    }
    // Placed by the flight's stream start: behind the consumed prefix is a retransmit of a flight
    // already fed; ahead of it cannot exist (the peer needs our reply to advance); exactly at it is the
    // flight awaited. A retransmit that would straddle the consumed prefix is corrupt.
    if start < self.consumed {
      return if start + total <= self.consumed {
        Reassembly::Repeat
      } else {
        Reassembly::Malformed
      };
    }
    if start > self.consumed {
      return Reassembly::Malformed;
    }
    match self.total {
      None => self.begin(total),
      Some(seen) if seen != total => return Reassembly::Malformed,
      Some(_) => {}
    }
    for (i, byte) in payload.iter().enumerate() {
      let at = within + i;
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
    self.consumed += total;
    self.total = None;
    self.present = Vec::new();
    self.filled = 0;
    Reassembly::Flight(flight)
  }

  /// Begins the next flight of `total` bytes (sizes the buffers).
  fn begin(&mut self, total: usize) {
    self.total = Some(total);
    self.bytes = vec![0u8; total];
    self.present = vec![false; total];
    self.filled = 0;
  }
}

/// Parses a fragment into `(start, within, total, payload)`, or `None` if it is not a well-formed
/// fragment.
fn parse(datagram: &[u8]) -> Option<(usize, usize, usize, &[u8])> {
  if datagram.first() != Some(&FRAGMENT_TAG) || datagram.len() < FRAGMENT_HEADER {
    return None;
  }
  let word = |at: usize| usize::from(u16::from_le_bytes([datagram[at], datagram[at + 1]]));
  Some((
    word(AT_START),
    word(AT_WITHIN),
    word(AT_TOTAL),
    &datagram[FRAGMENT_HEADER..],
  ))
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
      if n == 0 {
        0
      } else {
        usize::try_from(self.next() % u64::try_from(n).unwrap_or(u64::MAX)).unwrap_or(0)
      }
    }
  }

  /// Shape: the oracle's sample — random flights and delivery orders, enough to cover every fragment
  /// count up to the bound many times over.
  const ORACLE_CASES: usize = 2_000;

  /// A flight of `len` bytes whose content is a function of its position and `salt`, so two flights
  /// of one length differ.
  fn flight_of(len: usize, salt: u8) -> Vec<u8> {
    (0..len)
      .map(|i| {
        u8::try_from(i % 251)
          .unwrap_or(0)
          .wrapping_mul(31)
          .wrapping_add(salt)
      })
      .collect()
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

  /// A shuffled delivery order of `count` fragments with each sometimes duplicated.
  fn shuffled_with_duplicates(rng: &mut Rng, count: usize) -> Vec<usize> {
    let mut order: Vec<usize> = Vec::new();
    for i in 0..count {
      order.push(i);
      if rng.below(2) == 0 {
        order.push(i);
      }
    }
    for i in (1..order.len()).rev() {
      order.swap(i, rng.below(i + 1));
    }
    order
  }

  /// Do: fragment a flight at a random stream start, push its fragments in an arbitrary order with
  /// duplicates. Expect: every fragment fits the path floor; the flight comes back exactly, once, at the
  /// step the last new byte arrived, and the stream advanced by the flight. Oracle over random flights
  /// and orders.
  #[test]
  fn a_flight_reassembles_from_its_fragments_in_any_order_with_duplicates() {
    let mut rng = Rng(0x51A7_u64 ^ 0x9E37_79B9_7F4A_7C15);
    for case in 0..ORACLE_CASES {
      let len = 1 + rng.below(MAX_FLIGHT_BYTES);
      let flight = flight_of(len, u8::try_from(case % 200).unwrap_or(0));
      let fragments = fragment(&flight, 0).expect("within the bound");
      assert!(
        fragments.iter().all(|f| f.len() <= MIN_DATAGRAM_BYTES),
        "every fragment fits the path floor"
      );
      assert_eq!(
        fragments.len(),
        len.div_ceil(FRAGMENT_PAYLOAD),
        "as many fragments as needed"
      );
      let order = shuffled_with_duplicates(&mut rng, fragments.len());
      let (delivered, consumed) = deliver(&fragments, &order);
      assert_eq!(
        delivered.as_deref(),
        Some(flight.as_slice()),
        "the flight came back exactly"
      );
      assert_eq!(consumed, len, "the stream advanced by the flight");
    }
  }

  /// Delivers `fragments` in `order` to a fresh reassembler, asserting each step's verdict against the
  /// step the last new byte arrives; returns the flight delivered and the stream consumed.
  fn deliver(fragments: &[Vec<u8>], order: &[usize]) -> (Option<Vec<u8>>, usize) {
    let complete_at = last_new_index(order, fragments.len());
    let mut reassembler = Reassembler::new();
    let mut delivered: Option<Vec<u8>> = None;
    for (step, &which) in order.iter().enumerate() {
      match reassembler.push(&fragments[which]) {
        Reassembly::Flight(f) => {
          assert!(delivered.is_none(), "the flight is delivered once");
          assert_eq!(
            step, complete_at,
            "delivered when the last new byte arrived"
          );
          delivered = Some(f);
        }
        Reassembly::Pending => assert!(step < complete_at, "pending only before completion"),
        Reassembly::Repeat => assert!(step > complete_at, "a repeat only after completion"),
        Reassembly::Malformed => panic!("a well-formed fragment was refused at step {step}"),
      }
    }
    (delivered, reassembler.consumed())
  }

  /// A conversation of consecutive flights on one stream, each starting where the last ended, with
  /// the peer's retransmit of a consumed flight in between: the retransmit is a `Repeat` (its fragments
  /// name a start behind the consumed prefix), and a stale fragment of the earlier flight arriving
  /// *after* the next flight began is a `Repeat` too — never written into the next flight, which comes
  /// back intact. This is the case the per-flight offset scheme got wrong.
  #[test]
  fn consecutive_flights_and_a_stale_fragment_of_the_previous_one() {
    let first = flight_of(1500, 1);
    let second = flight_of(1500, 2); // the same length as the first, the confusable case
    let mut reassembler = Reassembler::new();
    let f1 = fragment(&first, 0).unwrap();
    let f2 = fragment(&second, first.len()).unwrap();
    let mut last = Reassembly::Pending;
    for frag in &f1 {
      last = reassembler.push(frag);
    }
    assert_eq!(last, Reassembly::Flight(first.clone()));
    // The whole first flight again: a repeat, on every fragment.
    for frag in &f1 {
      assert_eq!(reassembler.push(frag), Reassembly::Repeat);
    }
    // The second flight begins; a stale fragment of the first arrives in the middle of it.
    assert_eq!(reassembler.push(&f2[0]), Reassembly::Pending);
    assert_eq!(
      reassembler.push(&f1[1]),
      Reassembly::Repeat,
      "the stale fragment is placed by its start"
    );
    assert_eq!(
      reassembler.push(&f2[1]),
      Reassembly::Flight(second),
      "the second flight is intact"
    );
    assert_eq!(reassembler.consumed(), 3000);
  }

  /// A fragment header with the given fields and no payload.
  fn header(start: u16, within: u16, total: u16) -> Vec<u8> {
    let mut d = vec![FRAGMENT_TAG];
    d.extend_from_slice(&start.to_le_bytes());
    d.extend_from_slice(&within.to_le_bytes());
    d.extend_from_slice(&total.to_le_bytes());
    d
  }

  /// Hostile fragments at the header: a truncated header, a total past the bound, a zero total, an
  /// offset+payload past the total, and a flight ahead of the stream are each `Malformed` — dropped,
  /// never a panic.
  #[test]
  fn hostile_fragment_headers_are_malformed_never_a_fault() {
    let mut reassembler = Reassembler::new();
    assert_eq!(
      reassembler.push(&[FRAGMENT_TAG, 0]),
      Reassembly::Malformed,
      "truncated header"
    );
    assert_eq!(
      reassembler.push(&header(0, 0, u16::MAX)),
      Reassembly::Malformed,
      "total past the bound"
    );
    assert_eq!(
      reassembler.push(&header(0, 0, 0)),
      Reassembly::Malformed,
      "a zero total"
    );
    let mut past = header(0, 10, 4);
    past.extend_from_slice(&[1, 2, 3, 4]);
    assert_eq!(
      reassembler.push(&past),
      Reassembly::Malformed,
      "offset past total"
    );
    let mut ahead = header(100, 0, 4);
    ahead.extend_from_slice(&[1, 2, 3, 4]);
    assert_eq!(
      reassembler.push(&ahead),
      Reassembly::Malformed,
      "a flight ahead of the stream"
    );
  }

  /// Hostile fragments against the stream: a retransmit that straddles the consumed prefix, and a
  /// fragment naming a different total mid-flight, are `Malformed` — the flight in progress is kept.
  #[test]
  fn hostile_fragments_against_the_stream_are_malformed_never_a_fault() {
    let mut reassembler = Reassembler::new();
    let flight = flight_of(100, 9);
    for frag in fragment(&flight, 0).unwrap() {
      let _ = reassembler.push(&frag);
    }
    assert_eq!(reassembler.consumed(), 100);
    let mut straddle = header(50, 0, 100);
    straddle.push(0);
    assert_eq!(
      reassembler.push(&straddle),
      Reassembly::Malformed,
      "a retransmit straddling the prefix"
    );
    let next = fragment(&flight_of(1500, 3), 100).unwrap();
    assert_eq!(reassembler.push(&next[0]), Reassembly::Pending);
    let mut other_total = header(100, 0, 999);
    other_total.push(0);
    assert_eq!(
      reassembler.push(&other_total),
      Reassembly::Malformed,
      "a different total mid-flight"
    );
    assert_eq!(
      reassembler.push(&next[1]),
      Reassembly::Flight(flight_of(1500, 3)),
      "the flight in progress was kept"
    );
  }

  /// A flight under the path floor (the common case) is one fragment, routes as a fragment, and costs
  /// one datagram — the normal handshake's path, unchanged in datagram count; a flight past the bound
  /// or past the stream's range is refused before a byte leaves.
  #[test]
  fn a_small_flight_is_one_fragment_and_an_oversize_one_is_refused() {
    let flight = vec![0x0bu8; 700];
    let fragments = fragment(&flight, 0).unwrap();
    assert_eq!(fragments.len(), 1, "one fragment under the path floor");
    assert!(is_fragment(&fragments[0]), "and it routes as a fragment");
    let mut reassembler = Reassembler::new();
    assert_eq!(reassembler.push(&fragments[0]), Reassembly::Flight(flight));
    assert!(
      fragment(&vec![0u8; MAX_FLIGHT_BYTES + 1], 0).is_none(),
      "past the flight bound"
    );
    assert!(
      fragment(&[0u8; 10], MAX_STREAM_BYTES - 5).is_none(),
      "past the stream's range"
    );
    assert!(
      fragment(&[], 0).unwrap().is_empty(),
      "an empty flight sends nothing"
    );
  }
}
