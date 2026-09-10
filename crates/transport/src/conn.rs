//! The session plane's reliability core (§4.10a §8, slice 4c): packet-number assignment, ACK
//! generation (receive side) and ACK processing with loss detection (send side). These are the
//! pieces that turn the stream layer's frames into *reliable* delivery over a lossy datagram path:
//! the receiver acknowledges what arrived, the sender frees acknowledged packets and retransmits the
//! frames of packets a persistent gap declares lost, and the [`crate::stream::StreamAssembler`]
//! dedups the retransmit. A `reliable_delivery_survives_loss` test composes this with the stream
//! send/receive sides over a lossy channel and shows every byte arrives.
//!
//! The ACK is multi-range (RFC 9000 §19.3): [`AckGenerator`] reports every contiguous run of received
//! packet numbers — so a packet received below a gap is acknowledged rather than left to a spurious
//! retransmission — up to a frame-size budget, and [`SentTracker::on_ack_frame`] frees every
//! acknowledged run. The emitted ACK *frame* is bounded by that budget; safely bounding the received
//! *set* is owed, because forgetting a received packet needs an ACK-of-ACK (RFC 9000 §13.2.4) this
//! dialect does not yet carry (see [`AckGenerator`]).
//!
//! Owed (the rest of the connection): timer-based tail-loss recovery (this detects loss only by the
//! packet-reorder threshold, so a purely-tail drop needs the timer QUIC adds — the loop here keeps
//! sending until in-flight drains, and [`SentTracker::probe_oldest`] is that recovery's mechanism), and
//! the packet header + `rustls::quic` record protection that wraps these frames on the wire.
//! Congestion control is built ([`crate::congestion`]).

use std::collections::{BTreeMap, BTreeSet};

use crate::session::{AckRange, Frame};

/// Format: RFC 9002 §6.1.1 `kPacketThreshold` — three packets of reordering are tolerated before a
/// gap below the largest acknowledged packet declares the missing packets lost. A protocol constant,
/// not a tunable. Public so the flow-control window can be sized to keep this detection working (a
/// window of at least this many packets past a loss lets the gap form).
pub const REORDER_THRESHOLD: u64 = 3;

/// Receive-side acknowledgement state: which packet numbers have arrived, and the multi-range ACK to
/// send back.
///
/// The `received` set is not yet pruned: safely forgetting a received packet needs to know the peer
/// has seen an acknowledgement covering it (an acknowledgement of one of our ACK-bearing packets — RFC
/// 9000 §13.2.4's ACK-of-ACK), which this dialect does not yet carry. A fixed-window prune is *unsafe*
/// — an aggregated ACK would stop covering older packets the sender still has in flight, and the sender
/// would declare them lost — so bounding this set is owed together with connection-level `MaxData` and
/// ACK-of-ACK. The emitted ACK *frame* is bounded regardless (see [`AckGenerator::ack_frame`]).
#[derive(Debug, Default)]
pub struct AckGenerator {
  received: BTreeSet<u64>,
}

impl AckGenerator {
  /// A generator that has seen nothing yet.
  pub fn new() -> AckGenerator {
    AckGenerator::default()
  }

  /// Records a received packet number.
  pub fn record(&mut self, pn: u64) {
    self.received.insert(pn);
  }

  /// The multi-range ACK for the received packets (RFC 9000 §19.3), or `None` if none received: the
  /// highest contiguous run as `(largest, range)`, then each further run below it as an [`AckRange`]
  /// gap/length relative to the previous run (RFC 9000 §19.3.1), so a packet received below a gap is
  /// acknowledged too — not left to a spurious retransmission. `max_ranges` bounds the additional
  /// ranges so the frame fits its size budget; runs beyond it are simply not acknowledged here and the
  /// sender retransmits them (the receiver dedups), which is the single-range fallback this replaces.
  pub fn ack_frame(&self, max_ranges: usize) -> Option<Frame> {
    if self.received.is_empty() {
      return None;
    }
    // Collect contiguous runs as (high, low) inclusive, in descending order of packet number, stopping
    // once the range budget is reached (the highest runs — the sender's most recent activity — first).
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for &pn in self.received.iter().rev() {
      match runs.last_mut() {
        Some((_, low)) if *low == pn + 1 => *low = pn, // contiguous: extend the current run downward
        _ => {
          if runs.len() > max_ranges {
            break; // one first range plus `max_ranges` additional; older runs fall back to retransmit
          }
          runs.push((pn, pn)); // a gap: start a new run
        }
      }
    }
    let (largest, first_low) = runs[0];
    let range = largest - first_low;
    // Each further run below the first: the count of unacknowledged packets since the previous run's
    // low, then this run's length, both minus one (RFC 9000 §19.3.1). Consecutive runs are separated
    // by at least one unacknowledged packet, so `prev_low - high >= 2` and neither subtraction wraps.
    let mut ranges = Vec::new();
    let mut prev_low = first_low;
    for &(high, low) in &runs[1..] {
      ranges.push(AckRange {
        gap: prev_low - high - 2,
        len: high - low,
      });
      prev_low = low;
    }
    Some(Frame::Ack {
      largest,
      range,
      ranges,
    })
  }
}

/// One in-flight packet's carried frames, kept so a lost packet's data can be retransmitted.
#[derive(Debug, Clone)]
struct InFlight {
  frames: Vec<Frame>,
}

/// What a loss-detection pass declared lost: the frames to retransmit, and the largest packet number
/// among them (for the congestion controller's recovery-period guard, RFC 9002 §7.3.1). `highest_pn` is
/// `None` when nothing was lost.
#[derive(Debug, Default)]
pub struct Lost {
  /// The frames of the lost packets, to retransmit.
  pub frames: Vec<Frame>,
  /// The largest packet number that was declared lost, if any.
  pub highest_pn: Option<u64>,
}

/// The stream-data bytes a frame counts toward the in-flight congestion window: a `Stream` frame's
/// payload length, and zero for the acknowledgement and credit frames (which are never loss-tracked).
pub fn tracked_bytes(frame: &Frame) -> u64 {
  match frame {
    Frame::Stream { data, .. } => u64::try_from(data.len()).unwrap_or(u64::MAX),
    _ => 0,
  }
}

/// Send-side tracking: assigns packet numbers, records what each in-flight packet carried, processes
/// ACKs (freeing acknowledged packets), and declares loss by the reorder threshold.
#[derive(Debug, Default)]
pub struct SentTracker {
  next_pn: u64,
  in_flight: BTreeMap<u64, InFlight>,
  largest_acked: Option<u64>,
}

impl SentTracker {
  /// A tracker with nothing sent.
  pub fn new() -> SentTracker {
    SentTracker::default()
  }

  /// Assigns the next monotonic packet number.
  pub fn next_pn(&mut self) -> u64 {
    let pn = self.next_pn;
    self.next_pn = self.next_pn.saturating_add(1);
    pn
  }

  /// The next packet number that will be assigned, without advancing it — the packet-number cursor,
  /// so a caller can assert it stays monotonic across exchanges (no reuse under the keys).
  pub fn peek_next_pn(&self) -> u64 {
    self.next_pn
  }

  /// Records that packet `pn` carried `frames` (kept for possible retransmission).
  pub fn on_sent(&mut self, pn: u64, frames: Vec<Frame>) {
    self.in_flight.insert(pn, InFlight { frames });
  }

  /// Processes an ACK for `[largest - range, largest]`: removes acknowledged packets from flight and
  /// advances the largest-acknowledged watermark. Returns the stream-data bytes newly acknowledged (for
  /// the congestion controller to free from its in-flight count and grow the window by).
  pub fn on_ack(&mut self, largest: u64, range: u64) -> u64 {
    let low = largest.saturating_sub(range);
    let acked: Vec<u64> = self
      .in_flight
      .range(low..=largest)
      .map(|(&pn, _)| pn)
      .collect();
    let mut bytes = 0u64;
    for pn in acked {
      if let Some(flight) = self.in_flight.remove(&pn) {
        bytes = bytes.saturating_add(flight.frames.iter().map(tracked_bytes).sum());
      }
    }
    self.largest_acked = Some(self.largest_acked.map_or(largest, |l| l.max(largest)));
    bytes
  }

  /// Processes a whole ACK frame: its first range `[largest - range, largest]`, then each additional
  /// range below it (RFC 9000 §19.3.1), freeing every acknowledged packet from flight. Returns the
  /// total stream-data bytes newly acknowledged (for the congestion controller). Decoding each
  /// additional range relative to the previous run's low: `high = prev_low - gap - 2`, `low =
  /// high - len`. Saturating arithmetic means a malformed range from a hostile peer can only
  /// under-acknowledge (a packet not in flight frees nothing), never panic or free the wrong packet.
  pub fn on_ack_frame(&mut self, largest: u64, range: u64, ranges: &[AckRange]) -> u64 {
    let mut bytes = self.on_ack(largest, range);
    let mut prev_low = largest.saturating_sub(range);
    for r in ranges {
      let high = prev_low.saturating_sub(r.gap).saturating_sub(2);
      let low = high.saturating_sub(r.len);
      bytes = bytes.saturating_add(self.on_ack(high, high.saturating_sub(low)));
      prev_low = low;
    }
    bytes
  }

  /// Removes and returns the frames of packets now declared lost — in flight and at least the
  /// reorder threshold below the largest acknowledged packet (a gap that persisted past reordering) —
  /// with the largest lost packet number (for the congestion recovery-period guard). Retransmit the
  /// frames in a fresh packet.
  pub fn take_lost(&mut self) -> Lost {
    let Some(largest) = self.largest_acked else {
      return Lost::default();
    };
    let lost: Vec<u64> = self
      .in_flight
      .iter()
      .filter(|(pn, _)| pn.saturating_add(REORDER_THRESHOLD) <= largest)
      .map(|(&pn, _)| pn)
      .collect();
    let highest_pn = lost.iter().copied().max();
    let mut frames = Vec::new();
    for pn in lost {
      if let Some(flight) = self.in_flight.remove(&pn) {
        frames.extend(flight.frames);
      }
    }
    Lost { frames, highest_pn }
  }

  /// How many packets are unacknowledged and in flight.
  pub fn in_flight_count(&self) -> usize {
    self.in_flight.len()
  }

  /// The largest packet number the peer has acknowledged, or `None` before the first ACK — the value
  /// that sizes the truncated packet number a sender writes (RFC 9000 §17.1 via `packet_number`).
  pub fn largest_acked(&self) -> Option<u64> {
    self.largest_acked
  }

  /// Removes and returns the frames of the oldest in-flight packet, for a probe retransmission when the
  /// reorder threshold cannot detect a loss — a lost *last* packet has no later acknowledged packets to
  /// open a gap past it (RFC 9002 §6.2 handles this with a probe timeout; this is that recovery's
  /// mechanism, driven when the connection would otherwise stall with packets still in flight). Returns
  /// empty when nothing is in flight.
  pub fn probe_oldest(&mut self) -> Vec<Frame> {
    let Some((&pn, _)) = self.in_flight.iter().next() else {
      return Vec::new();
    };
    self
      .in_flight
      .remove(&pn)
      .map(|flight| flight.frames)
      .unwrap_or_default()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::stream::{StreamAssembler, StreamSender};

  /// Shape: an additional-range budget larger than any gap these small tests create, so the ACK
  /// reports every run (the cap itself is exercised by `the_ack_range_budget_bounds_the_frame`).
  const AMPLE_RANGES: usize = 8;

  /// The ACK reports *every* contiguous run of received packet numbers as multiple ranges (RFC 9000
  /// §19.3), so a packet received below a gap is acknowledged, not only the top run.
  #[test]
  fn the_ack_reports_every_received_run() {
    let mut acks = AckGenerator::new();
    for pn in [0u64, 1, 2, 4, 5] {
      acks.record(pn);
    }
    // Received {0,1,2,4,5}: top run [4,5] → largest 5, range 1; then run [0,2] below the gap at 3 →
    // one unacknowledged packet (3) so gap 0, three acknowledged (0,1,2) so len 2.
    assert_eq!(
      acks.ack_frame(AMPLE_RANGES),
      Some(Frame::Ack {
        largest: 5,
        range: 1,
        ranges: vec![AckRange { gap: 0, len: 2 }],
      })
    );
    acks.record(3); // filling the gap joins the runs: [0..=5], a single range.
    assert_eq!(
      acks.ack_frame(AMPLE_RANGES),
      Some(Frame::Ack {
        largest: 5,
        range: 5,
        ranges: vec![],
      })
    );
  }

  /// The additional-range budget bounds the ACK frame: with more gaps than the budget allows, only the
  /// first range plus `max_ranges` further runs are reported (the highest, most-recent ones); older
  /// runs are left for the sender to retransmit-and-dedup (the single-range fallback this generalizes).
  #[test]
  fn the_ack_range_budget_bounds_the_frame() {
    let mut acks = AckGenerator::new();
    // Received every even packet 0..=10: runs {10},{8},{6},{4},{2},{0} — six singleton runs.
    for pn in [0u64, 2, 4, 6, 8, 10] {
      acks.record(pn);
    }
    // A budget of 2 additional ranges reports the top run plus the next two below it: {10},{8},{6}.
    let ack = acks.ack_frame(2).expect("an ACK is owed");
    let Frame::Ack {
      largest,
      range,
      ranges,
    } = ack
    else {
      panic!("an ACK frame");
    };
    assert_eq!((largest, range), (10, 0), "top run is the singleton {{10}}");
    assert_eq!(
      ranges.len(),
      2,
      "only two additional ranges within the budget"
    );
    // Each further even singleton is one unacknowledged packet below the last (gap 0, len 0): {8},{6}.
    assert_eq!(
      ranges,
      vec![AckRange { gap: 0, len: 0 }, AckRange { gap: 0, len: 0 }]
    );
  }

  /// A multi-range ACK frees every acknowledged run from flight, across the gaps — not just the top
  /// run (the round-trip of `AckGenerator::ack_frame` through `SentTracker::on_ack_frame`).
  #[test]
  fn a_multi_range_ack_frees_every_run() {
    let mut sent = SentTracker::new();
    for pn in 0..6 {
      assert_eq!(sent.next_pn(), pn);
      sent.on_sent(
        pn,
        vec![Frame::Stream {
          stream_id: 1,
          offset: pn,
          fin: false,
          data: vec![0u8],
        }],
      );
    }
    // Received {0,1,2,4,5} (packet 3 dropped): the generator's ACK acknowledges both runs.
    let mut acks = AckGenerator::new();
    for pn in [0u64, 1, 2, 4, 5] {
      acks.record(pn);
    }
    let Some(Frame::Ack {
      largest,
      range,
      ranges,
    }) = acks.ack_frame(AMPLE_RANGES)
    else {
      panic!("an ACK is owed");
    };
    let bytes = sent.on_ack_frame(largest, range, &ranges);
    assert_eq!(
      bytes, 5,
      "five 1-byte stream packets acknowledged across the gap"
    );
    assert_eq!(
      sent.in_flight_count(),
      1,
      "only packet 3 (never received) stays in flight"
    );
  }

  /// An ACK frees the acknowledged packets from flight; a gap past the reorder threshold is lost.
  #[test]
  fn an_ack_frees_flight_and_a_gap_is_lost() {
    let mut sent = SentTracker::new();
    for pn in 0..6 {
      let got = sent.next_pn();
      assert_eq!(got, pn);
      sent.on_sent(pn, vec![Frame::MaxData { max: pn }]);
    }
    // Acknowledge [2,5]; 0 and 1 remain in flight, both >= REORDER_THRESHOLD below largest (5).
    sent.on_ack(5, 3);
    assert_eq!(sent.in_flight_count(), 2, "0 and 1 still in flight");
    let lost = sent.take_lost();
    assert_eq!(
      lost.frames.len(),
      2,
      "0 and 1 are declared lost (5 - 3 >= their pn)"
    );
    assert_eq!(lost.highest_pn, Some(1), "the largest lost pn is 1");
    assert_eq!(
      sent.in_flight_count(),
      0,
      "lost packets leave flight for retransmission"
    );
  }

  /// A probe removes the oldest in-flight packet's frames when no ACK-driven loss can be detected (a
  /// lost tail), so the connection can retransmit and drain.
  #[test]
  fn a_probe_takes_the_oldest_in_flight_packet() {
    let mut sent = SentTracker::new();
    sent.on_sent(0, vec![Frame::MaxData { max: 7 }]);
    sent.on_sent(1, vec![Frame::MaxData { max: 8 }]);
    // No ACK ever arrives (a tail loss), so `take_lost` finds nothing.
    assert!(sent.take_lost().frames.is_empty());
    // The probe frees the oldest (pn 0) for retransmission.
    assert_eq!(sent.probe_oldest(), vec![Frame::MaxData { max: 7 }]);
    assert_eq!(sent.in_flight_count(), 1, "only the oldest was taken");
    assert_eq!(sent.probe_oldest(), vec![Frame::MaxData { max: 8 }]);
    assert!(sent.probe_oldest().is_empty(), "nothing left to probe");
  }

  /// Builds one packet's frames: the lost frames to retransmit first, then up to two fresh ones.
  fn build_packet(source: &mut StreamSender, sent: &mut SentTracker) -> Vec<Frame> {
    let mut frames = sent.take_lost().frames;
    for _ in 0..2 {
      match source.next_frame(1, 8) {
        Some(frame) => frames.push(frame),
        None => break,
      }
    }
    frames
  }

  /// Delivers a packet to the receiver: records it, offers its stream frames, drains the newly
  /// contiguous bytes into `received`, and processes the ACK it sends back.
  fn deliver(
    pn: u64,
    frames: &[Frame],
    acks: &mut AckGenerator,
    assembler: &mut StreamAssembler,
    sent: &mut SentTracker,
    received: &mut Vec<u8>,
  ) {
    acks.record(pn);
    for frame in frames {
      if let Frame::Stream {
        offset, fin, data, ..
      } = frame
      {
        assembler.offer(*offset, data, *fin).unwrap();
      }
    }
    received.extend_from_slice(&assembler.read());
    if let Some(Frame::Ack {
      largest,
      range,
      ranges,
    }) = acks.ack_frame(AMPLE_RANGES)
    {
      sent.on_ack_frame(largest, range, &ranges);
    }
  }

  /// The reliability core composes with the stream layer to deliver every byte over a lossy channel:
  /// a dropped packet's frames are retransmitted once the gap is declared lost, and the assembler
  /// dedups. Do X (send a stream while dropping a packet), expect Y (the whole stream arrives).
  #[test]
  fn reliable_delivery_survives_loss() {
    let content: Vec<u8> = (0..160u16)
      .map(|i| u8::try_from(i % 251).unwrap_or(0))
      .collect();

    let mut source = StreamSender::new();
    source.write(&content);
    source.grant_credit(content.len() as u64);
    source.finish();

    let mut sent = SentTracker::new();
    let mut acks = AckGenerator::new();
    let mut assembler = StreamAssembler::new(content.len() as u64);
    let mut received = Vec::new();
    let mut dropped_once = false;

    let mut guard = 0;
    while !(assembler.is_complete() && sent.in_flight_count() == 0) {
      guard += 1;
      assert!(guard < 10_000, "the loop must make progress");
      let frames = build_packet(&mut source, &mut sent);
      if frames.is_empty() {
        break; // nothing new and nothing lost — the non-tail drop never leaves this at completion.
      }
      let pn = sent.next_pn();
      sent.on_sent(pn, frames.clone());
      // The lossy channel drops the second packet (pn 1) exactly once — a non-tail drop, so later
      // packets advance the largest-acked past the reorder threshold and the loss is detected.
      let drop_this = pn == 1 && !dropped_once;
      if pn == 1 {
        dropped_once = true;
      }
      if !drop_this {
        deliver(
          pn,
          &frames,
          &mut acks,
          &mut assembler,
          &mut sent,
          &mut received,
        );
      }
    }

    assert!(
      assembler.is_complete(),
      "every byte was delivered despite the drop"
    );
    assert_eq!(
      received, content,
      "the reassembled stream equals the source, once each"
    );
  }
}
