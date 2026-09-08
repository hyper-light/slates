//! The session plane's reliability core (§4.10a §8, slice 4c): packet-number assignment, ACK
//! generation (receive side) and ACK processing with loss detection (send side). These are the
//! pieces that turn the stream layer's frames into *reliable* delivery over a lossy datagram path:
//! the receiver acknowledges what arrived, the sender frees acknowledged packets and retransmits the
//! frames of packets a persistent gap declares lost, and the [`crate::stream::StreamAssembler`]
//! dedups the retransmit. A `reliable_delivery_survives_loss` test composes this with the stream
//! send/receive sides over a lossy channel and shows every byte arrives.
//!
//! Owed (the rest of the connection): multi-range ACKs (this acks the top contiguous run — correct,
//! just retransmits a received-but-below-a-gap packet, which dedups), timer-based tail-loss recovery
//! (this detects loss only by the packet-reorder threshold, so a purely-tail drop needs the timer
//! QUIC adds — the loop here keeps sending until in-flight drains), congestion control, and the
//! packet header + `rustls::quic` record protection that wraps these frames on the wire.

use std::collections::{BTreeMap, BTreeSet};

use crate::session::Frame;

/// Format: RFC 9002 §6.1.1 `kPacketThreshold` — three packets of reordering are tolerated before a
/// gap below the largest acknowledged packet declares the missing packets lost. A protocol constant,
/// not a tunable.
const REORDER_THRESHOLD: u64 = 3;

/// Receive-side acknowledgement state: which packet numbers have arrived, and the ACK to send back.
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

  /// The ACK for the top contiguous run of received packet numbers, or `None` if none received.
  /// Single-range (multi-range gaps are owed): correct — the sender retransmits a received-but-below-
  /// a-gap packet, which the receiver dedups — just less efficient than acknowledging every range.
  pub fn ack_frame(&self) -> Option<Frame> {
    let largest = *self.received.iter().next_back()?;
    let mut range = 0u64;
    while range < largest && self.received.contains(&(largest - range - 1)) {
      range += 1;
    }
    Some(Frame::Ack { largest, range })
  }
}

/// One in-flight packet's carried frames, kept so a lost packet's data can be retransmitted.
#[derive(Debug, Clone)]
struct InFlight {
  frames: Vec<Frame>,
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

  /// Records that packet `pn` carried `frames` (kept for possible retransmission).
  pub fn on_sent(&mut self, pn: u64, frames: Vec<Frame>) {
    self.in_flight.insert(pn, InFlight { frames });
  }

  /// Processes an ACK for `[largest - range, largest]`: removes acknowledged packets from flight and
  /// advances the largest-acknowledged watermark.
  pub fn on_ack(&mut self, largest: u64, range: u64) {
    let low = largest.saturating_sub(range);
    let acked: Vec<u64> = self
      .in_flight
      .range(low..=largest)
      .map(|(&pn, _)| pn)
      .collect();
    for pn in acked {
      self.in_flight.remove(&pn);
    }
    self.largest_acked = Some(self.largest_acked.map_or(largest, |l| l.max(largest)));
  }

  /// Removes and returns the frames of packets now declared lost — in flight and at least the
  /// reorder threshold below the largest acknowledged packet (a gap that persisted past reordering).
  /// Retransmit these in a fresh packet.
  pub fn take_lost(&mut self) -> Vec<Frame> {
    let Some(largest) = self.largest_acked else {
      return Vec::new();
    };
    let lost: Vec<u64> = self
      .in_flight
      .iter()
      .filter(|(pn, _)| pn.saturating_add(REORDER_THRESHOLD) <= largest)
      .map(|(&pn, _)| pn)
      .collect();
    let mut frames = Vec::new();
    for pn in lost {
      if let Some(flight) = self.in_flight.remove(&pn) {
        frames.extend(flight.frames);
      }
    }
    frames
  }

  /// How many packets are unacknowledged and in flight.
  pub fn in_flight_count(&self) -> usize {
    self.in_flight.len()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::stream::{StreamAssembler, StreamSender};

  /// The ACK covers the top contiguous run and stops at a gap.
  #[test]
  fn the_ack_covers_the_top_contiguous_run() {
    let mut acks = AckGenerator::new();
    for pn in [0u64, 1, 2, 4, 5] {
      acks.record(pn);
    }
    // Received {0,1,2,4,5}: the top run is [4,5] (a gap at 3), so largest 5, range 1.
    assert_eq!(
      acks.ack_frame(),
      Some(Frame::Ack {
        largest: 5,
        range: 1
      })
    );
    acks.record(3); // filling the gap joins the runs: [0..=5].
    assert_eq!(
      acks.ack_frame(),
      Some(Frame::Ack {
        largest: 5,
        range: 5
      })
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
      lost.len(),
      2,
      "0 and 1 are declared lost (5 - 3 >= their pn)"
    );
    assert_eq!(
      sent.in_flight_count(),
      0,
      "lost packets leave flight for retransmission"
    );
  }

  /// Builds one packet's frames: the lost frames to retransmit first, then up to two fresh ones.
  fn build_packet(source: &mut StreamSender, sent: &mut SentTracker) -> Vec<Frame> {
    let mut frames = sent.take_lost();
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
    if let Some(Frame::Ack { largest, range }) = acks.ack_frame() {
      sent.on_ack(largest, range);
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
