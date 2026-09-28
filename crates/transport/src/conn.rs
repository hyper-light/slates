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
//! acknowledged run (returning the freed packet numbers, for ACK-of-ACK). The emitted ACK *frame* is
//! bounded by that budget, and the received *set* is bounded by ACK-of-ACK ([`AckGenerator::confirm`],
//! RFC 9000 §13.2.4): once the peer acknowledges one of our acknowledgement-bearing packets, the
//! packets it covered are dropped, so the set stays within the recent unconfirmed window.
//!
//! Owed (the rest of the connection): timer-based tail-loss recovery (this detects loss only by the
//! packet-reorder threshold, so a purely-tail drop needs the timer QUIC adds — the loop here keeps
//! sending until in-flight drains, and [`SentTracker::copy_oldest`] is that recovery's mechanism), and
//! the packet header + `rustls::quic` record protection that wraps these frames on the wire.
//! Congestion control is built ([`crate::congestion`]).

use std::collections::{BTreeMap, BTreeSet};

use crate::delivery::RateSnapshot;
use crate::session::{AckRange, Frame};

/// Format: RFC 9002 §6.1.1 `kPacketThreshold` — three packets of reordering are tolerated before a
/// gap below the largest acknowledged packet declares the missing packets lost. A protocol constant,
/// not a tunable. Public so the flow-control window can be sized to keep this detection working (a
/// window of at least this many packets past a loss lets the gap form).
pub const REORDER_THRESHOLD: u64 = 3;

/// Receive-side acknowledgement state: which packet numbers have arrived, as merged contiguous ranges,
/// and the multi-range ACK to send back.
///
/// **Bounded by construction.** The received set is held as at most `max_ranges` ranges (a run of
/// consecutive packets is one entry, however long), so its size is set by the gaps, not by how many
/// packets arrived. Past the cap the oldest range is dropped and the duplicate floor raised to its top:
/// a packet at or below the floor is treated as already seen, which is safe because a sender never reuses
/// a packet number (a retransmission rides a fresh one, RFC 9000 §12.3). A late original arriving below
/// the floor is discarded, and its frames have already been sent again. ACK-of-ACK (RFC 9000 §13.2.4,
/// [`AckGenerator::confirm`]) shrinks the set further once the peer holds our acknowledgement. The
/// earlier form held every packet number in a set and walked all of them for each acknowledgement: a
/// receiver whose acknowledgements the peer never acknowledged (a pure bulk receiver) grew one entry per
/// packet forever, measured at 353 tracked with a 40 bound, 2026-09-28.
#[derive(Debug)]
pub struct AckGenerator {
  /// Received ranges, `low -> high` inclusive, disjoint and never adjacent (adjacent ranges merge).
  ranges: BTreeMap<u64, u64>,
  /// The most ranges held.
  max_ranges: usize,
  /// Every packet number at or below this has been seen, or is treated as seen (see the type's doc).
  confirmed_floor: Option<u64>,
}

impl AckGenerator {
  /// A generator that has seen nothing yet and holds at most `max_ranges` ranges (at least one).
  pub fn new(max_ranges: usize) -> AckGenerator {
    AckGenerator {
      ranges: BTreeMap::new(),
      max_ranges: max_ranges.max(1),
      confirmed_floor: None,
    }
  }

  /// Whether packet number `pn` has already been received and processed — inside a held range, or at or
  /// below the floor. A duplicate's frames must not be applied again (RFC 9000 §12.3).
  pub fn is_duplicate(&self, pn: u64) -> bool {
    self.confirmed_floor.is_some_and(|floor| pn <= floor)
      || self
        .ranges
        .range(..=pn)
        .next_back()
        .is_some_and(|(_, &high)| pn <= high)
  }

  /// Records a received packet number, merging it into its neighbours, and drops the oldest range when
  /// the cap is passed.
  pub fn record(&mut self, pn: u64) {
    if self.is_duplicate(pn) {
      return;
    }
    let mut low = pn;
    let mut high = pn;
    if let Some((&below_low, &below_high)) = self.ranges.range(..pn).next_back()
      && below_high.checked_add(1) == Some(pn)
    {
      self.ranges.remove(&below_low);
      low = below_low;
    }
    if let Some(above) = pn.checked_add(1)
      && let Some(above_high) = self.ranges.remove(&above)
    {
      high = above_high;
    }
    self.ranges.insert(low, high);
    while self.ranges.len() > self.max_ranges {
      let Some((_, dropped_high)) = self.ranges.pop_first() else {
        break;
      };
      self.raise_floor(dropped_high);
    }
  }

  fn raise_floor(&mut self, to: u64) {
    self.confirmed_floor = Some(self.confirmed_floor.map_or(to, |floor| floor.max(to)));
  }

  /// Confirms the peer has received an acknowledgement of ours covering packets up to `largest` (RFC
  /// 9000 §13.2.4): those packets need never be acknowledged again, so they leave the held ranges and the
  /// floor rises to `largest` — a later network duplicate of one is still recognized
  /// ([`is_duplicate`](AckGenerator::is_duplicate)).
  pub fn confirm(&mut self, largest: u64) {
    self.raise_floor(largest);
    let above = largest.saturating_add(1);
    let kept = self.ranges.split_off(&above);
    // A range straddling the floor keeps only its part above it.
    if let Some((_, &high)) = self.ranges.iter().next_back()
      && high >= above
    {
      self.ranges = kept;
      self.ranges.insert(above, high);
    } else {
      self.ranges = kept;
    }
  }

  /// How many ranges are held — the handle a test uses to prove the state stays bounded.
  pub fn tracked(&self) -> usize {
    self.ranges.len()
  }

  /// The multi-range ACK for the received packets (RFC 9000 §19.3), or `None` if none are held: the
  /// highest run as `(largest, range)`, then up to `max_ranges` further runs below it as [`AckRange`]
  /// gap/length pairs relative to the previous run (RFC 9000 §19.3.1), highest first. Runs beyond the
  /// budget are not acknowledged here; the sender retransmits them and the receiver dedups.
  pub fn ack_frame(&self, max_ranges: usize) -> Option<Frame> {
    let mut runs = self.ranges.iter().rev();
    let (&first_low, &largest) = runs.next()?;
    let mut ranges = Vec::new();
    let mut prev_low = first_low;
    for (&low, &high) in runs.take(max_ranges) {
      // Ranges are disjoint and never adjacent, so `prev_low - high >= 2` and neither subtraction wraps.
      ranges.push(AckRange {
        gap: prev_low.saturating_sub(high).saturating_sub(2),
        len: high.saturating_sub(low),
      });
      prev_low = low;
    }
    Some(Frame::Ack {
      largest,
      range: largest.saturating_sub(first_low),
      ranges,
    })
  }
}

/// One in-flight packet: the frames it carried (kept so a lost packet's data can be retransmitted), when
/// it was sent, the bytes it counts in flight, and the delivery state it was sent with.
#[derive(Debug, Clone)]
struct InFlight {
  frames: Vec<Frame>,
  sent_at: u64,
  bytes: u64,
  rate: RateSnapshot,
}

/// What a packet that left flight was: its number, send time, bytes in flight, and delivery snapshot —
/// what the delivery-rate sampler and the congestion controller read when it is acknowledged or lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SentPacket {
  /// The packet number.
  pub pn: u64,
  /// When it was sent (the caller's clock).
  pub sent_at: u64,
  /// The stream bytes it counted in flight.
  pub bytes: u64,
  /// The delivery state it was sent with.
  pub rate: RateSnapshot,
}

/// What processing an ACK freed: the stream-data bytes newly acknowledged (for the congestion
/// controller), the packet numbers removed from flight (for ACK-of-ACK, so the connection can confirm
/// which of its own acknowledgement-bearing packets the peer has now received), and each freed packet's
/// record (for the delivery-rate sample and the RTT).
#[derive(Debug, Default)]
pub struct Acked {
  /// The stream-data bytes newly acknowledged.
  pub bytes: u64,
  /// The packet numbers freed from flight by this acknowledgement.
  pub pns: Vec<u64>,
  /// The freed packets, in packet-number order.
  pub packets: Vec<SentPacket>,
  /// The frames the freed packets carried (the connection marks their stream ranges acknowledged).
  pub frames: Vec<Frame>,
}

/// What a loss-detection pass declared lost: the frames to retransmit, the largest packet number among
/// them (for the congestion controller's recovery-period guard, RFC 9002 §7.3.1), and each lost packet's
/// record. `highest_pn` is `None` when nothing was lost.
#[derive(Debug, Default)]
pub struct Lost {
  /// The frames of the lost packets, to retransmit.
  pub frames: Vec<Frame>,
  /// The packet numbers declared lost (for the connection to drop their ACK-of-ACK bookkeeping — a lost
  /// packet is never acknowledged, so its entry would otherwise linger).
  pub pns: Vec<u64>,
  /// The largest packet number that was declared lost, if any.
  pub highest_pn: Option<u64>,
  /// The lost packets, in packet-number order.
  pub packets: Vec<SentPacket>,
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
  /// The send times of acknowledged packets that could still fall inside a span of lost packets — the
  /// evidence the persistent-congestion test needs (an acknowledgement inside the span refutes it).
  /// Pruned below the oldest packet still in flight, since any future loss span starts at or after it,
  /// so it holds at most the packets sent during one in-flight window.
  acked_sent_at: BTreeSet<u64>,
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

  /// Records that packet `pn`, sent at `sent_at` with delivery snapshot `rate`, carried `frames` (kept for
  /// possible retransmission) counting `bytes` in flight.
  pub fn on_sent(&mut self, pn: u64, frames: Vec<Frame>, sent_at: u64, rate: RateSnapshot) {
    let bytes = frames.iter().map(tracked_bytes).sum();
    self.in_flight.insert(
      pn,
      InFlight {
        frames,
        sent_at,
        bytes,
        rate,
      },
    );
  }

  /// Processes an ACK for `[largest - range, largest]`: removes acknowledged packets from flight and
  /// advances the largest-acknowledged watermark. Returns the stream-data bytes newly acknowledged (for
  /// the congestion controller) and the packet numbers freed (for ACK-of-ACK — the connection looks up
  /// which of its own acknowledgement-bearing packets these were).
  pub fn on_ack(&mut self, largest: u64, range: u64) -> Acked {
    let low = largest.saturating_sub(range);
    let pns: Vec<u64> = self
      .in_flight
      .range(low..=largest)
      .map(|(&pn, _)| pn)
      .collect();
    let mut bytes = 0u64;
    let mut packets = Vec::with_capacity(pns.len());
    let mut frames = Vec::new();
    for &pn in &pns {
      if let Some(flight) = self.in_flight.remove(&pn) {
        bytes = bytes.saturating_add(flight.bytes);
        self.acked_sent_at.insert(flight.sent_at);
        packets.push(SentPacket {
          pn,
          sent_at: flight.sent_at,
          bytes: flight.bytes,
          rate: flight.rate,
        });
        frames.extend(flight.frames);
      }
    }
    self.largest_acked = Some(self.largest_acked.map_or(largest, |l| l.max(largest)));
    self.prune_acked_sent_at();
    Acked {
      bytes,
      pns,
      packets,
      frames,
    }
  }

  /// Processes a whole ACK frame: its first range `[largest - range, largest]`, then each additional
  /// range below it (RFC 9000 §19.3.1), freeing every acknowledged packet from flight. Returns the
  /// total stream-data bytes newly acknowledged (for the congestion controller) and every freed packet
  /// number (for ACK-of-ACK). Decoding each additional range relative to the previous run's low:
  /// `high = prev_low - gap - 2`, `low = high - len`. Saturating arithmetic means a malformed range
  /// from a hostile peer can only under-acknowledge (a packet not in flight frees nothing), never panic.
  pub fn on_ack_frame(&mut self, largest: u64, range: u64, ranges: &[AckRange]) -> Acked {
    let mut acked = self.on_ack(largest, range);
    let mut prev_low = largest.saturating_sub(range);
    for r in ranges {
      let high = prev_low.saturating_sub(r.gap).saturating_sub(2);
      let low = high.saturating_sub(r.len);
      let more = self.on_ack(high, high.saturating_sub(low));
      acked.bytes = acked.bytes.saturating_add(more.bytes);
      acked.pns.extend(more.pns);
      acked.packets.extend(more.packets);
      acked.frames.extend(more.frames);
      prev_low = low;
    }
    acked
  }

  /// Removes and returns the packets now declared lost (RFC 9002 §6.1): in flight below the largest
  /// acknowledged packet and either at least [`REORDER_THRESHOLD`] packets below it (§6.1.1) or sent at
  /// least `loss_delay` before `now` (§6.1.2's time threshold — what recovers a loss the packet threshold
  /// cannot see, and what keeps reordering within a round trip from reading as loss). Also returns when
  /// the next such packet crosses the time threshold (`loss_time`, §6.1.2), for the connection's timer;
  /// `None` when no unacknowledged packet sits below the largest acknowledged.
  pub fn take_lost(&mut self, now: u64, loss_delay: u64) -> (Lost, Option<u64>) {
    let Some(largest) = self.largest_acked else {
      return (Lost::default(), None);
    };
    // Before the clock has run a whole loss delay, no packet can be old enough to be lost by time.
    let lost_send_time = now.checked_sub(loss_delay);
    let mut lost = Vec::new();
    let mut loss_time: Option<u64> = None;
    for (&pn, flight) in self.in_flight.range(..largest) {
      let by_time = lost_send_time.is_some_and(|limit| flight.sent_at <= limit);
      if pn.saturating_add(REORDER_THRESHOLD) <= largest || by_time {
        lost.push(pn);
      } else {
        let due = flight.sent_at.saturating_add(loss_delay);
        loss_time = Some(loss_time.map_or(due, |earliest| earliest.min(due)));
      }
    }
    let highest_pn = lost.iter().copied().max();
    let mut frames = Vec::new();
    let mut packets = Vec::with_capacity(lost.len());
    for &pn in &lost {
      if let Some(flight) = self.in_flight.remove(&pn) {
        packets.push(SentPacket {
          pn,
          sent_at: flight.sent_at,
          bytes: flight.bytes,
          rate: flight.rate,
        });
        frames.extend(flight.frames);
      }
    }
    (
      Lost {
        frames,
        pns: lost,
        highest_pn,
        packets,
      },
      loss_time,
    )
  }

  /// The send time of the most recent ack-eliciting packet still in flight — the anchor the probe
  /// timeout is armed from (RFC 9002 §6.2.1: `time_of_last_ack_eliciting_packet`).
  pub fn newest_sent_at(&self) -> Option<u64> {
    // Packet numbers are assigned in send order on a monotonic clock, so the newest packet in flight is
    // the highest-numbered one: an O(log n) lookup, not a scan of every packet in flight (a scan made the
    // bake-off quadratic at a few thousand packets in flight, 2026-09-27).
    self
      .in_flight
      .last_key_value()
      .map(|(_, flight)| flight.sent_at)
  }

  /// Drops acknowledged send times older than the oldest packet still in flight (no future loss span can
  /// reach back past it), or all of them when nothing is in flight.
  fn prune_acked_sent_at(&mut self) {
    match self
      .in_flight
      .first_key_value()
      .map(|(_, flight)| flight.sent_at)
    {
      Some(oldest) => self.acked_sent_at = self.acked_sent_at.split_off(&oldest),
      None => self.acked_sent_at.clear(),
    }
  }

  /// How many packets are unacknowledged and in flight.
  pub fn in_flight_count(&self) -> usize {
    self.in_flight.len()
  }

  /// Drops stream `stream_id`'s data frames from every packet still in flight — the stream was
  /// forgotten (its exchange abandoned or complete), so a loss or a probe must not retransmit its bytes
  /// (RFC 9000 §2.4: data of a reset stream is not retransmitted) — and forgets the packets that carried
  /// nothing else. Returns the stream bytes dropped, so the caller can take them out of the congestion
  /// controller's in-flight count. Packets that still carry other frames stay tracked for their
  /// acknowledgement.
  pub fn forget_stream(&mut self, stream_id: u64) -> u64 {
    let mut dropped = 0u64;
    self.in_flight.retain(|_, packet| {
      let mut from_packet = 0u64;
      packet.frames.retain(|frame| match frame {
        Frame::Stream {
          stream_id: id,
          data,
          ..
        } if *id == stream_id => {
          from_packet = from_packet.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
          false
        }
        _ => true,
      });
      packet.bytes = packet.bytes.saturating_sub(from_packet);
      dropped = dropped.saturating_add(from_packet);
      !packet.frames.is_empty()
    });
    dropped
  }

  /// Drops from in-flight packet `pn` every frame equal to one of `copied` — they have just been sent again
  /// in a newer probe, so this older copy is redundant — and forgets the packet if nothing else remains in
  /// it; its other frames stay tracked. Returns the stream bytes dropped, for the caller's in-flight count.
  /// Nothing happens for a packet no longer in flight.
  pub fn drop_copied(&mut self, pn: u64, copied: &[Frame]) -> u64 {
    let Some(packet) = self.in_flight.get_mut(&pn) else {
      return 0;
    };
    let mut dropped = 0u64;
    packet.frames.retain(|frame| {
      let redundant = copied.contains(frame);
      if redundant {
        dropped = dropped.saturating_add(tracked_bytes(frame));
      }
      !redundant
    });
    packet.bytes = packet.bytes.saturating_sub(dropped);
    if packet.frames.is_empty() {
      self.in_flight.remove(&pn);
    }
    dropped
  }

  /// The largest packet number the peer has acknowledged, or `None` before the first ACK — the value
  /// that sizes the truncated packet number a sender writes (RFC 9000 §17.1 via `packet_number`).
  pub fn largest_acked(&self) -> Option<u64> {
    self.largest_acked
  }

  /// A copy of the oldest in-flight packet's frames, for a probe (RFC 9002 §6.2.4): the probe carries the
  /// data again in a new packet while the original stays in flight, to be acknowledged or declared lost by
  /// the usual thresholds once the probe's acknowledgement arrives — so a tail loss recovered by a probe
  /// is still reported as a loss to the congestion controller and to the persistent-congestion test.
  /// Empty when nothing is in flight.
  pub fn copy_oldest(&self) -> Vec<Frame> {
    self
      .in_flight
      .values()
      .next()
      .map(|flight| flight.frames.clone())
      .unwrap_or_default()
  }

  /// Whether `lost` establishes persistent congestion (RFC 9002 §7.6.2): two of its packets sent at
  /// least `duration` apart with no packet sent between them acknowledged — the path delivered nothing
  /// for longer than the probe timeouts that should have recovered it. (The RFC also requires an RTT
  /// sample before the earliest of them; the connection checks that, holding the RTT estimator.)
  pub fn persistent_congestion(&self, lost: &[SentPacket], duration: u64) -> bool {
    let (Some(first), Some(last)) = (
      lost.iter().map(|packet| packet.sent_at).min(),
      lost.iter().map(|packet| packet.sent_at).max(),
    ) else {
      return false;
    };
    last.saturating_sub(first) >= duration
      && self.acked_sent_at.range(first..=last).next().is_none()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::stream::{StreamAssembler, StreamSender};

  /// Shape: an additional-range budget larger than any gap these small tests create, so the ACK
  /// reports every run (the cap itself is exercised by `the_ack_range_budget_bounds_the_frame`).
  const AMPLE_RANGES: usize = 8;
  /// Shape: the ranges the tests' generators hold — ample for every pattern below except where a test
  /// sets its own cap.
  const TEST_RANGES: usize = 64;

  /// A packet number is a duplicate once it has been received (RFC 9000 §12.3), and stays a duplicate
  /// after ACK-of-ACK prunes it from the tracked set: the confirmed floor recognizes it, since a
  /// sender's numbers only ever increase and one at or below the floor can only be a repeat. A genuinely
  /// higher, unseen number is not a duplicate.
  #[test]
  fn a_seen_or_confirmed_packet_number_is_a_duplicate() {
    let mut acks = AckGenerator::new(TEST_RANGES);
    assert!(!acks.is_duplicate(5), "unseen before it arrives");
    acks.record(5);
    assert!(acks.is_duplicate(5), "seen and still tracked");

    // ACK-of-ACK confirms up to 5 and drops it from the tracked set — yet 5, and anything below, remain
    // duplicates: the sender will never issue a number that low again (a retransmit rides a fresh one).
    acks.confirm(5);
    assert_eq!(acks.tracked(), 0, "confirm pruned the tracked set");
    assert!(
      acks.is_duplicate(5),
      "still a duplicate below the confirmed floor"
    );
    assert!(
      acks.is_duplicate(2),
      "anything at or below the floor is a duplicate"
    );
    assert!(!acks.is_duplicate(6), "a higher, unseen number is new");
  }

  /// The received set is held as merged ranges, so a long in-order run costs one entry, and a gappy
  /// arrival pattern costs at most the cap: the oldest range is dropped and the duplicate floor rises to
  /// its top, so a packet from a dropped range still reads as a duplicate while a new higher one does not.
  /// Regression: every packet number was held individually, and an acknowledgement-only receiver's set grew
  /// one entry per packet (353 against a bound of 40, 2026-09-28). Do X (100,000 in-order packets, then
  /// every other packet of a further 1,000), expect Y (one range, then never more than the cap).
  #[test]
  fn the_received_set_stays_bounded_however_many_packets_arrive() {
    let mut acks = in_order_then(100_000);
    let mut peak = 0;
    for pn in (100_001..101_001u64).step_by(2) {
      acks.record(pn);
      peak = peak.max(acks.tracked());
    }
    assert_eq!(peak, CAP, "the gaps filled the cap and never passed it");
    assert!(
      acks.is_duplicate(50_000),
      "a packet from a dropped range is a duplicate"
    );
    assert!(acks.is_duplicate(100_999), "a held packet is a duplicate");
    assert!(
      !acks.is_duplicate(101_000),
      "an unseen packet between held ranges is new"
    );
    assert!(!acks.is_duplicate(101_001), "a higher unseen packet is new");
    let Some(Frame::Ack {
      largest, ranges, ..
    }) = acks.ack_frame(AMPLE_RANGES)
    else {
      panic!("an ACK is owed");
    };
    assert_eq!(
      largest, 100_999,
      "the acknowledgement leads with the newest packet"
    );
    assert_eq!(ranges.len(), CAP - 1, "it reports every held range");
  }

  /// Shape: the range cap of the bounded-set test.
  const CAP: usize = 4;

  /// A generator of cap [`CAP`] that has received packets `0..count` in order — which is one range.
  fn in_order_then(count: u64) -> AckGenerator {
    let mut acks = AckGenerator::new(CAP);
    for pn in 0..count {
      acks.record(pn);
    }
    assert_eq!(acks.tracked(), 1, "an in-order run is one range");
    acks
  }

  /// Filling a gap merges the ranges on both sides into one, and a confirmation that falls inside a range
  /// keeps only the part above it.
  #[test]
  fn ranges_merge_across_a_filled_gap_and_confirmation_trims_a_straddling_range() {
    let mut acks = AckGenerator::new(TEST_RANGES);
    for pn in [1u64, 2, 3, 5, 6, 7] {
      acks.record(pn);
    }
    assert_eq!(acks.tracked(), 2);
    acks.record(4);
    assert_eq!(acks.tracked(), 1, "the gap filled: [1, 7] is one range");
    acks.confirm(4);
    assert_eq!(
      acks.ack_frame(AMPLE_RANGES),
      Some(Frame::Ack {
        largest: 7,
        range: 2,
        ranges: vec![],
      }),
      "only [5, 7] remains to acknowledge"
    );
    assert!(acks.is_duplicate(3), "below the confirmed floor");
  }

  /// The ACK reports *every* contiguous run of received packet numbers as multiple ranges (RFC 9000
  /// §19.3), so a packet received below a gap is acknowledged, not only the top run.
  #[test]
  fn the_ack_reports_every_received_run() {
    let mut acks = AckGenerator::new(TEST_RANGES);
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
    let mut acks = AckGenerator::new(TEST_RANGES);
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
        0,
        RateSnapshot::default(),
      );
    }
    // Received {0,1,2,4,5} (packet 3 dropped): the generator's ACK acknowledges both runs.
    let mut acks = AckGenerator::new(TEST_RANGES);
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
    let acked = sent.on_ack_frame(largest, range, &ranges);
    assert_eq!(
      acked.bytes, 5,
      "five 1-byte stream packets acknowledged across the gap"
    );
    assert_eq!(acked.pns.len(), 5, "five packet numbers freed (0,1,2,4,5)");
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
      sent.on_sent(
        pn,
        vec![Frame::MaxData { max: pn }],
        0,
        RateSnapshot::default(),
      );
    }
    // Acknowledge [2,5]; 0 and 1 remain in flight, both >= REORDER_THRESHOLD below largest (5).
    sent.on_ack(5, 3);
    assert_eq!(sent.in_flight_count(), 2, "0 and 1 still in flight");
    let lost = sent.take_lost(0, 1).0;
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

  /// A probe copies the oldest in-flight packet's frames when no ACK-driven loss can be detected (a lost
  /// tail), leaving the original in flight to be acknowledged or declared lost (RFC 9002 §6.2.4).
  #[test]
  fn a_probe_copies_the_oldest_in_flight_packet() {
    let mut sent = SentTracker::new();
    sent.on_sent(
      0,
      vec![Frame::MaxData { max: 7 }],
      0,
      RateSnapshot::default(),
    );
    sent.on_sent(
      1,
      vec![Frame::MaxData { max: 8 }],
      0,
      RateSnapshot::default(),
    );
    // No ACK ever arrives (a tail loss), so `take_lost` finds nothing.
    assert!(sent.take_lost(0, 1).0.frames.is_empty());
    assert_eq!(sent.copy_oldest(), vec![Frame::MaxData { max: 7 }]);
    assert_eq!(sent.in_flight_count(), 2, "the original stays in flight");
    assert!(
      SentTracker::new().copy_oldest().is_empty(),
      "nothing to copy with nothing in flight"
    );
  }

  /// Builds one packet's frames: the lost frames to retransmit first, then up to two fresh ones.
  fn build_packet(source: &mut StreamSender, sent: &mut SentTracker) -> Vec<Frame> {
    let mut frames = sent.take_lost(0, 1).0.frames;
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
    let mut acks = AckGenerator::new(TEST_RANGES);
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
      sent.on_sent(pn, frames.clone(), 0, RateSnapshot::default());
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
