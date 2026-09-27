//! The sans-io session-plane connection driver (§4.10a §8): the state machine that composes the
//! stream layer ([`crate::stream`]), the reliability core ([`crate::conn`]) and the flow-control
//! credit law ([`crate::flow`]) into *reliable, ordered, exactly-once* delivery of **many multiplexed
//! streams** over a lossy, reordering datagram path. It owns no socket and no clock: [`poll_transmit`]
//! yields the next packet's frames and its packet number, [`handle_incoming`] takes a received
//! packet's number and frames, and the caller (the [`crate::endpoint::Endpoint`]) does the I/O —
//! protecting, sending, receiving and unprotecting. Keeping the protocol logic sans-io is what lets
//! the oracle below drive loss and reorder *directly and deterministically*, with no OS network and no
//! injected-fault plumbing (the pattern quinn-proto and rustls follow, and the shape `session.rs`,
//! `stream.rs`, `conn.rs` and `flow.rs` are already written in).
//!
//! [`poll_transmit`]: Connection::poll_transmit
//! [`handle_incoming`]: Connection::handle_incoming
//!
//! Multiplexing: several streams share one connection (one handshake, one packet-number space, one
//! reliability and acknowledgement machine). Each carries an independent ordered byte sequence keyed by
//! stream id, with its own flow-control window; a send picks one fresh frame from the send streams in
//! round-robin so no stream starves another, and a received frame is demultiplexed to its stream's
//! reassembler. Packet numbers and loss recovery stay connection-wide — a lost packet's one frame is
//! retransmitted whatever stream it belonged to. What each carries (the reliable record classes of
//! §4.10a) is the caller's; here they are just byte streams.
//!
//! Time (the constrained-link design, research note §5.3; 2026-09-27): every entry point takes the caller's
//! clock. The connection owns the RTT estimator (RFC 9002 §5), declares loss by both of RFC 9002 §6.1's
//! thresholds (three packets, or 9/8 of the RTT), arms the probe timeout with its exponential backoff
//! (§6.2), detects persistent congestion (§7.6), samples the delivery rate of every acknowledgement
//! (`crate::delivery`), feeds a congestion controller (`crate::congestion`) that sets the window, the
//! pacing rate and the send quantum, and paces ack-eliciting packets (`crate::pacer`). [`next_timeout`]
//! is the earliest of the loss timer, the probe timer and the pacing release; [`on_timeout`] acts on it.
//! Flow control is the ratified dual-level credit law (`crate::flow`), its window auto-tuned toward the
//! path's BDP up to the session's receive ceiling.
//!
//! [`next_timeout`]: Connection::next_timeout
//! [`on_timeout`]: Connection::on_timeout

use std::collections::{BTreeMap, VecDeque};

use crate::congestion::{AckEvent, Controller, ControllerKind, LossEvent};
use crate::conn::{AckGenerator, Lost, REORDER_THRESHOLD, SentPacket, SentTracker, tracked_bytes};
use crate::delivery::DeliveryRate;
use crate::flow::FlowController;
use crate::pacer::Pacer;
use crate::rtt::RttEstimator;
use crate::session::Frame;
use crate::stream::{StreamAssembler, StreamSender};

/// The most receive reassemblers kept for streams below the exchange floor — late replies of abandoned
/// exchanges still arriving ([`Connection::discard_streams_below`]).
/// Derived: `REORDER_THRESHOLD + 1`, one in-flight packet window — a peer serves one exchange at a time
/// and this end abandons at most one per deadline, so more stragglers than fit one loss-detection window
/// can only be a peer replaying the past; the oldest is forgotten first. Anchored to `conn::REORDER_THRESHOLD`.
// The threshold is a small count (three); it cannot truncate on any pointer width, and `usize::try_from`
// is not usable in a const.
#[allow(clippy::cast_possible_truncation)]
pub const LATE_REPLY_STREAMS: usize = (REORDER_THRESHOLD + 1) as usize;

/// Format: RFC 9002 §6.2.1 — the probe timeout doubles after each consecutive expiry; the most doublings
/// applied, enough that a one-millisecond timeout climbs past any initial PTO (`1 ms × 2^12 ≈ 4 s`), and
/// a shift no larger than this cannot overflow.
const PTO_BACKOFF_SHIFT_CAP: u32 = 12;

/// The initial receive-window credit, in bytes, for a frame cap of `max_frame_len`.
/// Derived: `(REORDER_THRESHOLD + 1) × max_frame_len` — the least in-flight data that keeps
/// reorder-based loss detection working. A loss is declared when `REORDER_THRESHOLD` later packets are
/// acknowledged past a gap (RFC 9002 §6.1.1); a window that admits one lost packet plus that many past
/// it lets the gap form. Anchored to `conn::REORDER_THRESHOLD` and the frame size. The window then
/// auto-tunes toward the path's BDP (`crate::flow`), up to the session's receive ceiling.
pub fn initial_receive_window(max_frame_len: usize) -> u64 {
  (REORDER_THRESHOLD + 1).saturating_mul(max_frame_len as u64)
}

/// What a connection is built with: the packet budget it frames at (the datagram payload one packet
/// carries), the initial receive window both ends derive, the ceiling that window may auto-tune to (the
/// session's receive memory budget), and the congestion controller (the bake-off's choice; see
/// `crate::congestion`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionShape {
  /// The most stream bytes one packet carries.
  pub max_datagram: u64,
  /// The initial receive window, bytes.
  pub initial_window: u64,
  /// The receive window's ceiling, bytes.
  pub receive_ceiling: u64,
  /// The congestion controller.
  pub controller: ControllerKind,
}

impl ConnectionShape {
  /// The shape for frames of `max_frame_len` bytes: the derived initial window, a ceiling of
  /// `receive_ceiling` bytes, and `controller`.
  pub fn for_frame_cap(
    max_frame_len: usize,
    receive_ceiling: u64,
    controller: ControllerKind,
  ) -> ConnectionShape {
    ConnectionShape {
      max_datagram: max_frame_len as u64,
      initial_window: initial_receive_window(max_frame_len),
      receive_ceiling,
      controller,
    }
  }
}

/// One end of a reliable, flow-controlled, congestion-controlled, paced, multi-stream connection. The send
/// side is a set of streams (each a source bounded by the credit the peer advertises for it) served
/// round-robin, plus the connection-wide in-flight tracker and retransmission queue; the receive side is
/// a set of ordered reassemblers keyed by stream id, the acknowledgement generator, and the flow-control
/// accounting that advertises each stream's credit a window ahead of what has been read from it.
pub struct Connection {
  /// The send streams, keyed by id (each framed only within the credit the peer has advertised for it).
  send_streams: BTreeMap<u64, StreamSender>,
  /// Send stream ids in the order opened, with a cursor, for round-robin scheduling.
  send_order: Vec<u64>,
  /// The round-robin cursor into `send_order`.
  send_cursor: usize,
  /// Connection-wide in-flight packet tracking and loss detection (packet numbers are per-connection).
  sent: SentTracker,
  /// Frames freed by loss detection or a probe, awaiting retransmission.
  retransmit: VecDeque<Frame>,
  /// The receive reassemblers, keyed by stream id (created when a stream's first frame arrives).
  recv_streams: BTreeMap<u64, StreamAssembler>,
  /// Which packet numbers have arrived, and the acknowledgement to send back.
  acks: AckGenerator,
  /// The receive-side flow-control accounting (per stream): advertises credit a window ahead of reads.
  flow: FlowController,
  /// The initial receive window (what each new stream's sender starts with, R8).
  initial_window: u64,
  /// Set when an ack-eliciting packet has arrived and its acknowledgement has not yet been sent.
  ack_owed: bool,
  /// How many frames this end has retransmitted (the non-vacuity counter for the loss-recovery path).
  retransmitted: u64,
  /// The congestion controller.
  controller: Controller,
  /// The bytes in flight (sent, not yet acknowledged, declared lost, or forgotten).
  in_flight: u64,
  /// The RTT estimator (RFC 9002 §5).
  rtt: RttEstimator,
  /// When the first RTT sample was taken (persistent congestion needs losses after it, §7.6.2).
  first_rtt_sample_at: Option<u64>,
  /// The delivery-rate sampler (`crate::delivery`).
  delivery: DeliveryRate,
  /// The pacer (`crate::pacer`).
  pacer: Pacer,
  /// When the pacer next releases a packet, while ack-eliciting data waits on it.
  pacing_release: Option<u64>,
  /// The earliest time an unacknowledged packet crosses the time threshold (RFC 9002 §6.1.2).
  loss_time: Option<u64>,
  /// Consecutive probe timeouts without an acknowledgement (RFC 9002 §6.2.1's `pto_count`).
  pto_count: u32,
  /// Ack-eliciting packets the probe timeout allows past the window and the pacer (RFC 9002 §6.2.4).
  probes_owed: u32,
  /// The packet number at which the sender last found its window full with data waiting — the sender was
  /// window-limited for any packet acknowledged below it (`C.is_cwnd_limited`, draft-ietf-ccwg-bbr §2.2).
  window_full_at: Option<u64>,
  /// Send side: total fresh stream bytes sent across all streams (never counting a retransmission),
  /// the connection-wide counterpart to each stream's send offset. Held below `peer_max_data`.
  connection_sent: u64,
  /// Send side: the connection-wide flow-control ceiling the peer has advertised (`MaxData`).
  peer_max_data: u64,
  /// ACK-of-ACK bookkeeping (RFC 9000 §13.2.4): for each ack-eliciting packet this end sent that also
  /// carried an acknowledgement, the largest packet number that acknowledgement covered.
  sent_acks: BTreeMap<u64, u64>,
  /// Received packets discarded as duplicates of a packet number already processed (RFC 9000 §12.3).
  duplicates: u64,
  /// Stream frames the peer sent in breach of flow control or a stream's final size (RFC 9000 §4.1,
  /// §4.5), dropped and counted.
  violations: u64,
}

/// Why fresh data stopped being framed in one `poll_transmit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FreshStop {
  /// The packet budget was filled.
  Budget,
  /// The congestion window was full.
  Window,
  /// The pacer holds the next packet until the given time.
  Pacer(u64),
  /// The peer's connection credit is spent.
  Credit,
  /// No stream has data within its credit.
  Empty,
}

impl Connection {
  /// A fresh connection of `shape` whose clock reads `now`, drawing randomized controller timing from
  /// `seed`.
  pub fn new(shape: ConnectionShape, now: u64, seed: u64) -> Connection {
    Connection {
      send_streams: BTreeMap::new(),
      send_order: Vec::new(),
      send_cursor: 0,
      sent: SentTracker::new(),
      retransmit: VecDeque::new(),
      recv_streams: BTreeMap::new(),
      acks: AckGenerator::new(),
      flow: FlowController::new(shape.initial_window, shape.receive_ceiling),
      initial_window: shape.initial_window,
      ack_owed: false,
      retransmitted: 0,
      controller: Controller::new(shape.controller, shape.max_datagram, now, seed),
      in_flight: 0,
      rtt: RttEstimator::new(),
      first_rtt_sample_at: None,
      delivery: DeliveryRate::new(),
      pacer: Pacer::new(),
      pacing_release: None,
      loss_time: None,
      pto_count: 0,
      probes_owed: 0,
      window_full_at: None,
      connection_sent: 0,
      // The peer's initial connection credit is the initial window it advertises before any read; both
      // ends derive the same one (R8).
      peer_max_data: shape.initial_window,
      sent_acks: BTreeMap::new(),
      duplicates: 0,
      violations: 0,
    }
  }

  /// The most additional ACK ranges the acknowledgement frame may carry (RFC 9000 §19.3.1), derived so
  /// the whole ACK fits one `max_frame_len`-byte frame: the frame cap, less the ACK header (kind byte,
  /// largest, first range, range count), over the bytes each additional range costs (a gap and a
  /// length). Beyond it, older received runs are retransmitted and deduped rather than acknowledged.
  fn max_ack_ranges(max_frame_len: usize) -> usize {
    let header = size_of::<u8>() + size_of::<u64>() + size_of::<u64>() + size_of::<u16>();
    let per_range = size_of::<u64>() + size_of::<u64>();
    max_frame_len.saturating_sub(header) / per_range.max(1)
  }

  /// Opens send stream `stream_id` carrying the whole of `data`, and finishes it. The send side starts
  /// with the initial window of credit (both ends derive the same window, R8); more is granted only as
  /// the peer's `MaxStreamData` for this stream arrives, so the sender never races more than a window
  /// ahead of that stream's reader.
  pub fn open(&mut self, stream_id: u64, data: &[u8]) {
    let mut sender = StreamSender::new();
    sender.write(data);
    sender.grant_credit(self.initial_window);
    sender.finish();
    if self.send_streams.insert(stream_id, sender).is_none() {
      self.send_order.push(stream_id);
    }
  }

  /// Reseeds the congestion controller's randomized timing (the endpoint does so from the session's
  /// connection id, once the handshake has derived it).
  pub fn reseed(&mut self, seed: u64) {
    self.controller.reseed(seed);
  }

  /// Seeds the RTT estimator with a round trip measured outside the connection (the handshake's first
  /// flight, RFC 9002 §5.1), at `now`.
  pub fn seed_rtt(&mut self, sample: u64, now: u64) {
    self.rtt.on_sample(sample, 0);
    self.first_rtt_sample_at.get_or_insert(now);
  }

  /// The next packet to send at `now`, as `(packet_number, frames)`, or `None` when there is nothing to
  /// send right now. Fills the packet up to `packet_budget` stream bytes with several ack-eliciting frames
  /// (RFC 9000 §12.2): queued retransmissions first, then fresh frames round-robin, while the congestion
  /// window, the pacer and the connection credit allow — a probe the timeout owed may pass the window and
  /// the pacer (RFC 9002 §6.2.4); then, if an acknowledgement is owed, the acknowledgement followed by the
  /// current receive credit. An acknowledgement-only packet is neither paced nor window-limited.
  pub fn poll_transmit(&mut self, now: u64, packet_budget: usize) -> Option<(u64, Vec<Frame>)> {
    let budget = packet_budget as u64;
    let probe = self.probes_owed > 0;
    let mut reliable: Vec<Frame> = Vec::new();
    let stop = match self.pacing_hold(now, budget, probe) {
      Some(release) => FreshStop::Pacer(release),
      None => {
        let mut packed = 0;
        self
          .take_retransmissions(budget, probe, &mut reliable, &mut packed)
          .unwrap_or_else(|| self.take_fresh(budget, probe, &mut reliable, &mut packed))
      }
    };
    self.note_stop(stop, reliable.is_empty());
    let mut frames = reliable.clone();
    let ack_largest = self.push_acknowledgement(&mut frames, packet_budget);
    if frames.is_empty() {
      return None;
    }
    let pn = self.sent.next_pn();
    if !reliable.is_empty() {
      self.record_sent(now, pn, reliable, probe, ack_largest);
    }
    Some((pn, frames))
  }

  /// When ack-eliciting data waits and no probe is owed: the pacer's release time if it holds the next
  /// packet (sized at the budget it may fill), else `None`.
  fn pacing_hold(&mut self, now: u64, budget: u64, probe: bool) -> Option<u64> {
    if probe || (self.retransmit.is_empty() && !self.has_fresh_data()) {
      return None;
    }
    let rate = self.controller.pacing_rate(&self.rtt).max(1);
    let quantum = self.controller.send_quantum(&self.rtt);
    let release = self.pacer.release_time(now, budget, rate, quantum);
    (release > now).then_some(release)
  }

  /// Whether `bytes` more fit the congestion window over what is in flight and already `packed` (a lone
  /// packet always fits when nothing is in flight, RFC 9002 §7.5, so the window never deadlocks).
  fn window_room(&self, packed: u64, bytes: u64) -> bool {
    let in_flight = self.in_flight.saturating_add(packed);
    in_flight == 0 || in_flight.saturating_add(bytes) <= self.controller.window()
  }

  /// Moves queued retransmissions into the packet while the budget and window allow (a probe passes the
  /// window). Returns why it stopped if that ends the packet, `None` to go on to fresh data.
  fn take_retransmissions(
    &mut self,
    budget: u64,
    probe: bool,
    reliable: &mut Vec<Frame>,
    packed: &mut u64,
  ) -> Option<FreshStop> {
    while *packed < budget {
      let bytes = tracked_bytes(self.retransmit.front()?);
      if !probe && !self.window_room(*packed, bytes) {
        return Some(FreshStop::Window);
      }
      let frame = self.retransmit.pop_front()?;
      *packed = packed.saturating_add(bytes);
      reliable.push(frame);
    }
    Some(FreshStop::Budget)
  }

  /// Frames fresh stream data into the packet while the budget, the window (a probe passes it) and the
  /// peer's connection credit allow; returns why it stopped.
  fn take_fresh(
    &mut self,
    budget: u64,
    probe: bool,
    reliable: &mut Vec<Frame>,
    packed: &mut u64,
  ) -> FreshStop {
    while *packed < budget {
      let room = budget - *packed;
      let connection_credit = self.peer_max_data.saturating_sub(self.connection_sent);
      if connection_credit == 0 {
        return FreshStop::Credit;
      }
      if !probe && !self.window_room(*packed, room.min(connection_credit)) {
        return FreshStop::Window;
      }
      let fresh_cap = usize::try_from(room.min(connection_credit)).unwrap_or(usize::MAX);
      let Some(frame) = self.next_fresh_frame(fresh_cap) else {
        return FreshStop::Empty;
      };
      let bytes = tracked_bytes(&frame);
      self.connection_sent = self.connection_sent.saturating_add(bytes);
      *packed = packed.saturating_add(bytes);
      reliable.push(frame);
    }
    FreshStop::Budget
  }

  /// Records why sending stopped: a full window with data waiting marks the sender window-limited
  /// (`C.is_cwnd_limited`); running out of data marks it application-limited (draft §4.1.2.4); the pacer's
  /// hold is remembered as the next release.
  fn note_stop(&mut self, stop: FreshStop, nothing_packed: bool) {
    match stop {
      FreshStop::Window if self.has_fresh_data() || !self.retransmit.is_empty() => {
        self.window_full_at = Some(self.sent.peek_next_pn());
      }
      FreshStop::Empty | FreshStop::Credit if nothing_packed && self.retransmit.is_empty() => {
        self
          .delivery
          .check_app_limited(true, self.in_flight, self.controller.window());
      }
      _ => {}
    }
    self.pacing_release = match stop {
      FreshStop::Pacer(release) => Some(release),
      _ => None,
    };
  }

  /// Tracks ack-eliciting packet `pn` carrying `reliable`, sent at `now`: the controller's and sampler's
  /// send hooks, the pacer's tokens (a probe spends none), the bytes in flight, and ACK-of-ACK.
  fn record_sent(
    &mut self,
    now: u64,
    pn: u64,
    reliable: Vec<Frame>,
    probe: bool,
    ack_largest: Option<u64>,
  ) {
    if let Some(largest) = ack_largest {
      self.sent_acks.insert(pn, largest);
    }
    let bytes: u64 = reliable.iter().map(tracked_bytes).sum();
    self
      .controller
      .on_sent(now, pn, self.in_flight, self.delivery.is_app_limited());
    let snapshot = self.delivery.on_sent(now, self.in_flight, bytes);
    if probe {
      self.probes_owed -= 1;
    } else {
      let rate = self.controller.pacing_rate(&self.rtt).max(1);
      let quantum = self.controller.send_quantum(&self.rtt);
      self.pacer.on_sent(now, bytes, rate, quantum);
    }
    self.in_flight = self.in_flight.saturating_add(bytes);
    self.sent.on_sent(pn, reliable, now, snapshot);
  }

  /// Whether any send stream has data it could frame within its credit.
  fn has_fresh_data(&self) -> bool {
    self.send_streams.values().any(StreamSender::has_sendable)
  }

  /// If an acknowledgement is owed, appends it — followed by the connection-wide credit and each
  /// received stream's credit, piggybacked so a lost credit frame re-advertises with the next
  /// acknowledgement — to `frames`, clears the owed flag, and returns the largest packet number the
  /// acknowledgement covered (for ACK-of-ACK bookkeeping). `None` when none is owed or generated.
  fn push_acknowledgement(&mut self, frames: &mut Vec<Frame>, max_frame_len: usize) -> Option<u64> {
    if !self.ack_owed {
      return None;
    }
    let ack = self.acks.ack_frame(Self::max_ack_ranges(max_frame_len))?;
    let largest = if let Frame::Ack { largest, .. } = ack {
      Some(largest)
    } else {
      None
    };
    frames.push(ack);
    frames.push(self.flow.connection_credit_frame());
    for &stream_id in self.recv_streams.keys() {
      frames.push(self.flow.stream_credit_frame(stream_id));
    }
    self.ack_owed = false;
    largest
  }

  /// The next fresh stream frame to send, chosen round-robin across the send streams so none starves,
  /// or `None` when no send stream has data within its credit right now. Advances the round-robin
  /// cursor past the stream served.
  fn next_fresh_frame(&mut self, max_frame_len: usize) -> Option<Frame> {
    let count = self.send_order.len();
    for step in 0..count {
      let index = (self.send_cursor + step) % count;
      let stream_id = self.send_order[index];
      if let Some(sender) = self.send_streams.get_mut(&stream_id)
        && let Some(frame) = sender.next_frame(stream_id, max_frame_len)
      {
        self.send_cursor = (index + 1) % count;
        return Some(frame);
      }
    }
    None
  }

  /// Takes a packet received at `now`: records its number for acknowledgement, demultiplexes each stream
  /// frame to its reassembler, processes any acknowledgement (the RTT sample, the delivery-rate sample,
  /// loss detection and the congestion controller), applies each stream's advertised send credit, and
  /// queues for retransmission whatever is declared lost. An acknowledgement-only packet does not oblige
  /// an acknowledgement in return (RFC 9002 §2).
  pub fn handle_incoming(&mut self, now: u64, pn: u64, frames: &[Frame]) {
    // A packet whose number was already processed is a network duplicate (RFC 9000 §12.3): dropped whole.
    if self.acks.is_duplicate(pn) {
      self.duplicates = self.duplicates.saturating_add(1);
      return;
    }
    self.acks.record(pn);
    let mut ack_eliciting = false;
    for frame in frames {
      match frame {
        Frame::Stream {
          stream_id,
          offset,
          fin,
          data,
        } => {
          ack_eliciting = true;
          let window = self.flow.stream_max(*stream_id);
          let initial = self.initial_window;
          let assembler = self
            .recv_streams
            .entry(*stream_id)
            .or_insert_with(|| StreamAssembler::new(initial));
          // The window is the flow-control ceiling the sender could not exceed; a duplicate or
          // reordered segment is deduped. A refusal means the peer broke flow control or sent two final
          // sizes (RFC 9000 §4.1, §4.5): the data is dropped and the violation counted, never ignored.
          assembler.grant_window(window);
          if assembler.offer(*offset, data, *fin).is_err() {
            self.violations = self.violations.saturating_add(1);
          }
        }
        Frame::Ack {
          largest,
          range,
          ranges,
        } => self.on_ack_frame(now, *largest, *range, ranges),
        Frame::MaxStreamData { stream_id, max } => {
          if let Some(sender) = self.send_streams.get_mut(stream_id) {
            sender.grant_credit(*max);
          }
        }
        Frame::MaxData { max } => {
          self.peer_max_data = self.peer_max_data.max(*max);
        }
      }
    }
    if ack_eliciting {
      self.ack_owed = true;
    }
  }

  /// Processes one acknowledgement frame at `now` (RFC 9002 §B.4 `OnAckReceived`).
  fn on_ack_frame(
    &mut self,
    now: u64,
    largest: u64,
    range: u64,
    ranges: &[crate::session::AckRange],
  ) {
    let acked = self.sent.on_ack_frame(largest, range, ranges);
    if acked.packets.is_empty() {
      return;
    }
    // ACK-of-ACK (RFC 9000 §13.2.4).
    for pn in &acked.pns {
      if let Some(covered) = self.sent_acks.remove(pn) {
        self.acks.confirm(covered);
      }
    }
    // The RTT sample (RFC 9002 §5.1): the largest acknowledged packet, if this acknowledgement newly
    // acknowledged it.
    if let Some(newest) = acked.packets.iter().max_by_key(|packet| packet.pn)
      && newest.pn == largest
    {
      self.rtt.on_sample(now.saturating_sub(newest.sent_at), 0);
      self.first_rtt_sample_at.get_or_insert(now);
    }
    self.pto_count = 0;
    self.in_flight = self.in_flight.saturating_sub(acked.bytes);
    self.delivery.begin_ack();
    for packet in &acked.packets {
      self
        .delivery
        .on_packet_acked(now, packet.pn, packet.bytes, &packet.rate);
    }
    let lost = self.detect_losses(now);
    let min_rtt = self.rtt.has_sample().then(|| self.rtt.min_rtt());
    let sample = self.delivery.finish_ack(now, min_rtt);
    let newest_acked = acked
      .packets
      .iter()
      .map(|packet| packet.pn)
      .max()
      .unwrap_or(0);
    let cwnd_limited = self.window_full_at.is_some_and(|full| full > newest_acked);
    let ack_event = AckEvent {
      now,
      acked: &acked.packets,
      sample,
      in_flight: self.in_flight,
      delivered: self.delivery.delivered(),
      cwnd_limited,
      rtt: &self.rtt,
    };
    let loss_event = (!lost.packets.is_empty()).then(|| LossEvent {
      now,
      lost: &lost.packets,
      largest_sent: self.sent.peek_next_pn().saturating_sub(1),
      in_flight: self.in_flight,
      lost_total: self.delivery.lost(),
      persistent: self.persistent_congestion(&lost.packets),
      srtt: self.rtt.smoothed_rtt_or_initial(),
    });
    self
      .controller
      .on_ack_and_loss(&ack_event, loss_event.as_ref());
    if self.controller.take_app_limited() {
      self.delivery.mark_app_limited(self.in_flight);
    }
    self.requeue_lost(lost);
  }

  /// Runs loss detection at `now` (RFC 9002 §6.1): removes the lost packets from flight and the sampler's
  /// accounting, and re-arms the loss timer. The caller hands the packets to the controller and requeues
  /// their frames.
  fn detect_losses(&mut self, now: u64) -> Lost {
    let (lost, loss_time) = self.sent.take_lost(now, self.rtt.loss_delay());
    self.loss_time = loss_time;
    let bytes: u64 = lost.packets.iter().map(|packet| packet.bytes).sum();
    self.in_flight = self.in_flight.saturating_sub(bytes);
    if bytes > 0 {
      self.delivery.on_lost(bytes);
    }
    lost
  }

  /// Whether `lost` establishes persistent congestion (RFC 9002 §7.6.2): an RTT sample existed before the
  /// earliest of them, and they span the persistent-congestion duration with nothing acknowledged between.
  fn persistent_congestion(&self, lost: &[SentPacket]) -> bool {
    let Some(first_sample) = self.first_rtt_sample_at else {
      return false;
    };
    let earliest = lost.iter().map(|packet| packet.sent_at).min().unwrap_or(0);
    first_sample < earliest
      && self
        .sent
        .persistent_congestion(lost, self.rtt.persistent_congestion_duration(0))
  }

  /// Queues the frames of lost packets for retransmission and drops their ACK-of-ACK entries; bounds the
  /// ACK-of-ACK map to the unacknowledged window.
  fn requeue_lost(&mut self, lost: Lost) {
    for pn in &lost.pns {
      self.sent_acks.remove(pn);
    }
    if let Some(largest_acked) = self.sent.largest_acked() {
      self.sent_acks = self.sent_acks.split_off(&largest_acked);
    }
    self.queue_retransmit(lost.frames);
  }

  /// The earliest time this connection needs [`on_timeout`](Connection::on_timeout): the loss timer, the
  /// probe timeout while ack-eliciting packets are in flight (RFC 9002 §6.2.1, backed off by
  /// `2^pto_count`), and the pacer's release while data waits on it. `None` when nothing is pending.
  pub fn next_timeout(&self) -> Option<u64> {
    [self.loss_time.or(self.pto_deadline()), self.pacing_release]
      .into_iter()
      .flatten()
      .min()
  }

  /// When the probe timeout fires while ack-eliciting packets are in flight: the most recent such packet's
  /// send time plus the PTO doubled per consecutive expiry (RFC 9002 §6.2.1), the backed-off value held
  /// under the larger of the PTO and the initial PTO — a peer that has gone silent is retried a few times a
  /// second, not once per minute after a dozen doublings (the fleet's takeover and re-dial rely on it; the
  /// endpoint held the same bound since 2026-09-13).
  fn pto_deadline(&self) -> Option<u64> {
    if self.probes_owed > 0 {
      // A probe is owed and not yet sent: the caller's next flush sends it; the timer re-arms from it.
      return None;
    }
    let sent = self.sent.newest_sent_at()?;
    let pto = self.rtt.pto(0);
    let backoff = 1u64 << self.pto_count.min(PTO_BACKOFF_SHIFT_CAP);
    let timeout = pto
      .saturating_mul(backoff)
      .min(pto.max(self.rtt.initial_pto()));
    Some(sent.saturating_add(timeout))
  }

  /// Acts on the timers due at `now` (RFC 9002 §6.2 `OnLossDetectionTimeout`): a due loss timer declares
  /// the packets past the time threshold lost; else a due probe timeout owes a probe packet (new data if
  /// any, else the oldest in-flight packet's frames retransmitted), which may pass the window and the
  /// pacer, and doubles the next timeout. A due pacing release needs nothing: the next `poll_transmit`
  /// sends. Returns whether anything was queued to send.
  pub fn on_timeout(&mut self, now: u64) -> bool {
    if self.loss_time.is_some_and(|due| due <= now) {
      let lost = self.detect_losses(now);
      if lost.packets.is_empty() {
        return false;
      }
      let event = LossEvent {
        now,
        lost: &lost.packets,
        largest_sent: self.sent.peek_next_pn().saturating_sub(1),
        in_flight: self.in_flight,
        lost_total: self.delivery.lost(),
        persistent: self.persistent_congestion(&lost.packets),
        srtt: self.rtt.smoothed_rtt_or_initial(),
      };
      self.controller.on_loss(&event);
      self.requeue_lost(lost);
      return true;
    }
    if self.pto_deadline().is_none_or(|due| due > now) {
      return false;
    }
    self.pto_count = self.pto_count.saturating_add(1);
    self.probes_owed = 1;
    // The probe carries new data only if new data can actually leave now — a stream with data inside its
    // own credit but no connection credit left cannot, and a probe that sends nothing leaves the timer
    // firing at the same instant forever (found by the loss oracle, 2026-09-27).
    let fresh_can_leave = self.has_fresh_data() && self.peer_max_data > self.connection_sent;
    if !fresh_can_leave && self.retransmit.is_empty() {
      // No new data: the probe carries a copy of the oldest in-flight packet's frames (RFC 9002 §6.2.4);
      // the original stays in flight until acknowledged or declared lost. A probe is not a congestion
      // signal; a loss it later reveals is.
      let frames = self.sent.copy_oldest();
      self.queue_retransmit(frames);
    }
    true
  }

  /// Sends a copy of the oldest in-flight packet as a probe now, whatever the timers say — the tail-loss
  /// probe a caller that owns its own deadline drives (RFC 9002 §6.2.4: the original stays in flight).
  /// Returns whether anything was queued. Prefer [`on_timeout`](Connection::on_timeout), which arms it
  /// from the RTT.
  pub fn probe(&mut self) -> bool {
    let frames = self.sent.copy_oldest();
    let probed = !frames.is_empty();
    if probed {
      self.probes_owed = 1;
    }
    self.queue_retransmit(frames);
    probed
  }

  /// Allocates the next packet number and a bare, decryptable payload — a re-advertisement of the
  /// connection's current flow-control credit — for a **handshake-confirmation** packet
  /// ([`Endpoint::establish`](crate::Endpoint::establish)); see RFC 9000 §19.20, RFC 9001 §4.1.2. It is
  /// not ack-eliciting and not tracked; the endpoint resends a confirmation with a fresh number each
  /// probe timeout, so no number is ever reused under the packet keys (RFC 9001 §9.5).
  pub fn emit_confirm(&mut self) -> (u64, Vec<Frame>) {
    let pn = self.sent.next_pn();
    (pn, vec![self.flow.connection_credit_frame()])
  }

  /// Drains the bytes now contiguous on receive stream `stream_id` (in order, each once) at `now`, slides
  /// that stream's flow-control window forward by what was consumed, and auto-tunes the window, so the
  /// next acknowledgement advertises fresh credit a window ahead of the new read cursor. Empty if the
  /// stream is unknown.
  pub fn read_stream(&mut self, now: u64, stream_id: u64) -> Vec<u8> {
    let Some(assembler) = self.recv_streams.get_mut(&stream_id) else {
      return Vec::new();
    };
    let bytes = assembler.read();
    let read_offset = assembler.read_offset();
    self.flow.on_stream_consumed(stream_id, read_offset);
    if !bytes.is_empty() {
      let rtt = self.rtt.has_sample().then(|| self.rtt.smoothed_rtt());
      self.flow.autotune(now, rtt);
    }
    bytes
  }

  /// The stream ids seen on the receive side so far (a frame has arrived for each).
  pub fn recv_stream_ids(&self) -> Vec<u64> {
    self.recv_streams.keys().copied().collect()
  }

  /// Drains and discards every receive stream whose id is below `floor` — the late replies of exchanges
  /// the caller has abandoned, or late copies of requests already served (a stream id is never reused
  /// within a connection, RFC 9000 §2.1). The bytes are **read**, not dropped, so every byte the peer sent
  /// is credited back to it; a stream that has reached its `fin` has its receive half forgotten (never its
  /// send half: a reply may still be in flight on the id), and the stragglers below the floor are capped at
  /// [`LATE_REPLY_STREAMS`], the oldest forgotten first.
  pub fn discard_streams_below(&mut self, now: u64, floor: u64) {
    let late: Vec<u64> = self
      .recv_streams
      .keys()
      .copied()
      .filter(|&id| id < floor)
      .collect();
    for id in &late {
      let _ = self.read_stream(now, *id);
      if self.recv_stream_complete(*id) {
        self.forget_recv_stream(*id);
      }
    }
    let mut lingering: Vec<u64> = self
      .recv_streams
      .keys()
      .copied()
      .filter(|&id| id < floor)
      .collect();
    while lingering.len() > LATE_REPLY_STREAMS {
      let oldest = lingering.remove(0);
      self.forget_recv_stream(oldest);
    }
  }

  /// Forgets the receive half of stream `stream_id` only — its reassembler and its per-stream receive
  /// accounting — leaving whatever this end is still sending on the same id. A request and its reply share
  /// one id (`Endpoint::serve_once`), so a late copy of a served request must never take the reply in
  /// flight with it: the whole-stream forget did, and the reply was counted complete unacknowledged
  /// (`docs/bugs/2026-09-27-a-late-request-copy-forgot-the-reply.md`).
  fn forget_recv_stream(&mut self, stream_id: u64) {
    self.recv_streams.remove(&stream_id);
    self.flow.forget_stream(stream_id);
  }

  /// The sender's current congestion window in bytes.
  pub fn congestion_window(&self) -> u64 {
    self.controller.window()
  }

  /// The congestion controller (for the bake-off's reports).
  pub fn controller(&self) -> &Controller {
    &self.controller
  }

  /// The bytes currently in flight.
  pub fn bytes_in_flight(&self) -> u64 {
    self.in_flight
  }

  /// The RTT estimator.
  pub fn rtt(&self) -> &RttEstimator {
    &self.rtt
  }

  /// The receive window currently advertised ahead of consumption, and how many times it has grown.
  pub fn receive_window(&self) -> (u64, u64) {
    (self.flow.window(), self.flow.growths())
  }

  /// How many packet numbers the receive side still tracks for acknowledgement.
  pub fn acks_tracked(&self) -> usize {
    self.acks.tracked()
  }

  /// Whether receive stream `stream_id` is complete (all bytes through its `fin`). False if unknown.
  pub fn recv_stream_complete(&self, stream_id: u64) -> bool {
    self
      .recv_streams
      .get(&stream_id)
      .is_some_and(StreamAssembler::is_complete)
  }

  /// Ack-eliciting packets sent and not yet acknowledged.
  pub fn in_flight_count(&self) -> usize {
    self.sent.in_flight_count()
  }

  /// Whether every send stream has originated its whole data and every ack-eliciting packet has been
  /// acknowledged (nothing buffered to retransmit, nothing in flight).
  pub fn send_complete(&self) -> bool {
    self.retransmit.is_empty()
      && self.sent.in_flight_count() == 0
      && self.send_streams.values().all(StreamSender::is_drained)
  }

  /// Forgets a completed or abandoned stream's send and receive state; nothing of it is retransmitted
  /// afterwards, its in-flight bytes leave the window, and the connection credit they took is refunded
  /// (see the notes kept on the `SentTracker` and `FlowController` methods this calls).
  pub fn forget_stream(&mut self, stream_id: u64) {
    self.send_streams.remove(&stream_id);
    self.send_order.retain(|&id| id != stream_id);
    self.send_cursor = 0;
    self.recv_streams.remove(&stream_id);
    self
      .retransmit
      .retain(|frame| !matches!(frame, Frame::Stream { stream_id: id, .. } if *id == stream_id));
    let dropped = self.sent.forget_stream(stream_id);
    self.in_flight = self.in_flight.saturating_sub(dropped);
    self.connection_sent = self.connection_sent.saturating_sub(dropped);
    self.flow.forget_stream(stream_id);
  }

  /// The next packet number this connection will assign — its packet-number cursor.
  pub fn tx_packet_number(&self) -> u64 {
    self.sent.peek_next_pn()
  }

  /// How many frames this end has retransmitted — the non-vacuity counter for the loss-recovery path.
  pub fn retransmitted(&self) -> u64 {
    self.retransmitted
  }

  /// How many stream frames the peer sent in breach of flow control or a final size (dropped).
  pub fn protocol_violations(&self) -> u64 {
    self.violations
  }

  /// How many received packets were discarded as duplicates (RFC 9000 §12.3).
  pub fn duplicates_discarded(&self) -> u64 {
    self.duplicates
  }

  /// The largest packet number the peer has acknowledged.
  pub fn tx_largest_acked(&self) -> Option<u64> {
    self.sent.largest_acked()
  }

  /// The absolute offset send stream `stream_id` has framed so far. Zero if the stream is unknown.
  pub fn send_offset(&self, stream_id: u64) -> u64 {
    self
      .send_streams
      .get(&stream_id)
      .map_or(0, StreamSender::send_offset)
  }

  /// The absolute offset receive stream `stream_id` has delivered so far. Zero if unknown.
  pub fn read_offset(&self, stream_id: u64) -> u64 {
    self
      .recv_streams
      .get(&stream_id)
      .map_or(0, StreamAssembler::read_offset)
  }

  /// Queues frames for retransmission, counting them.
  fn queue_retransmit(&mut self, frames: Vec<Frame>) {
    self.retransmitted = self.retransmitted.saturating_add(frames.len() as u64);
    self.retransmit.extend(frames);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use proptest::prelude::*;

  /// Shape: the frame length cap the driver frames at in these tests — small, so a short stream spans
  /// many packets and every loss, reorder and credit path runs.
  const FRAME_CAP: usize = 8;
  /// Shape: a packet budget of several frames for the multi-frame tests.
  const PACKET_BUDGET: usize = 4 * FRAME_CAP;
  /// Shape: a millisecond.
  const MS: u64 = 1_000_000;
  /// Every control law in the bake-off; each oracle runs over all of them, since exact delivery must hold
  /// whichever law sets the window.
  const LAWS: [ControllerKind; 5] = [
    ControllerKind::NewReno,
    ControllerKind::Cubic,
    ControllerKind::Bbr,
    ControllerKind::Copa,
    ControllerKind::CopaMeta,
  ];

  /// A connection framing at `cap` whose receive window stays at the initial window (the ceiling equals
  /// it), under `law`.
  fn fixed_window(cap: usize, law: ControllerKind) -> Connection {
    let window = initial_receive_window(cap);
    Connection::new(
      ConnectionShape {
        max_datagram: cap as u64,
        initial_window: window,
        receive_ceiling: window,
        controller: law,
      },
      0,
      1,
    )
  }

  /// Which transmissions the wire drops (counted across both directions, from zero) and whether it
  /// reverses each batch a sender emits (a deterministic reordering).
  struct Channel {
    drop_steps: Vec<u64>,
    step: u64,
    reorder: bool,
  }

  impl Channel {
    fn new(drop_steps: Vec<u64>) -> Channel {
      Channel {
        drop_steps,
        step: 0,
        reorder: false,
      }
    }

    fn reordering(drop_steps: Vec<u64>) -> Channel {
      Channel {
        drop_steps,
        step: 0,
        reorder: true,
      }
    }

    fn drops(&mut self) -> bool {
      let dropped = self.drop_steps.contains(&self.step);
      self.step = self.step.saturating_add(1);
      dropped
    }
  }

  /// A two-ended wire with a virtual clock: packets arrive `delay` after they are sent (in send order, or
  /// batch-reversed), and when nothing can move the clock jumps to the next arrival or the earlier of the
  /// two connections' timers — how the endpoint drives a connection over a real path.
  struct Wire {
    now: u64,
    delay: u64,
    in_flight: BTreeMap<(u64, u64), (bool, u64, Vec<Frame>)>,
    sequence: u64,
    channel: Channel,
    /// The most stream frames any packet sent on this wire carried (the packing path's witness).
    widest: usize,
  }

  impl Wire {
    fn new(delay: u64, channel: Channel) -> Wire {
      Wire {
        now: 0,
        delay,
        in_flight: BTreeMap::new(),
        sequence: 0,
        channel,
        widest: 0,
      }
    }

    /// Sends everything `from` wants to at the current time (`to_b`: the direction), returning how many
    /// packets left. `check` runs after each send, before the packet is handed to the wire.
    fn send(
      &mut self,
      from: &mut Connection,
      to_b: bool,
      budget: usize,
      mut check: impl FnMut(&Connection),
    ) -> usize {
      let mut batch = Vec::new();
      while let Some((pn, frames)) = from.poll_transmit(self.now, budget) {
        check(from);
        let stream_frames = frames
          .iter()
          .filter(|f| matches!(f, Frame::Stream { .. }))
          .count();
        self.widest = self.widest.max(stream_frames);
        batch.push((pn, frames));
      }
      let count = batch.len();
      if self.channel.reorder {
        batch.reverse();
      }
      for (pn, frames) in batch {
        if !self.channel.drops() {
          let arrival = self.now.saturating_add(self.delay);
          self
            .in_flight
            .insert((arrival, self.sequence), (to_b, pn, frames));
          self.sequence += 1;
        }
      }
      count
    }

    /// Delivers every packet due by now; returns how many.
    fn deliver(&mut self, a: &mut Connection, b: &mut Connection) -> usize {
      let mut delivered = 0;
      while let Some(entry) = self.in_flight.first_entry() {
        if entry.key().0 > self.now {
          break;
        }
        let (to_b, pn, frames) = entry.remove();
        let target = if to_b { &mut *b } else { &mut *a };
        target.handle_incoming(self.now, pn, &frames);
        delivered += 1;
      }
      delivered
    }

    /// Advances the clock to the next event (an arrival, or either side's timer) and runs the timers due
    /// then. False when nothing is pending at all: the run has stalled or finished.
    fn advance(&mut self, a: &mut Connection, b: &mut Connection) -> bool {
      let next = [
        self.in_flight.keys().next().map(|(arrival, _)| *arrival),
        a.next_timeout(),
        b.next_timeout(),
      ]
      .into_iter()
      .flatten()
      .min();
      let Some(next) = next else {
        return false;
      };
      self.now = self.now.max(next);
      for side in [a, b] {
        if side.next_timeout().is_some_and(|due| due <= self.now) {
          side.on_timeout(self.now);
        }
      }
      true
    }
  }

  /// Runs `streams` from a sender to a receiver under `law` over `wire`, framing at `budget`, and returns
  /// what the receiver reassembled per stream and the sender (for its counters). Asserts the
  /// never-whole-object invariant on every send.
  fn transfer_on(
    streams: &[(u64, Vec<u8>)],
    law: ControllerKind,
    budget: usize,
    mut wire: Wire,
  ) -> (BTreeMap<u64, Vec<u8>>, Connection) {
    let window = initial_receive_window(budget);
    let mut sender = fixed_window(budget, law);
    for (id, content) in streams {
      sender.open(*id, content);
    }
    let mut receiver = fixed_window(budget, law);
    let mut received: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut guard = 0u64;
    loop {
      guard += 1;
      assert!(guard < 1_000_000, "the connection must make progress");
      let reads: Vec<(u64, u64)> = streams
        .iter()
        .map(|(id, _)| (*id, receiver.read_offset(*id)))
        .collect();
      let mut moved = wire.send(&mut sender, true, budget, |from| {
        for (id, read) in &reads {
          assert!(
            from.send_offset(*id) <= read + window,
            "stream {id}: the sender raced more than a window ahead of the reader"
          );
        }
      });
      let now = wire.now;
      moved += wire.deliver(&mut sender, &mut receiver);
      for id in receiver.recv_stream_ids() {
        received
          .entry(id)
          .or_default()
          .extend(receiver.read_stream(now, id));
      }
      moved += wire.send(&mut receiver, false, budget, |_| {});
      moved += wire.deliver(&mut sender, &mut receiver);
      let all_recv = streams
        .iter()
        .all(|(id, _)| receiver.recv_stream_complete(*id));
      if sender.send_complete() && all_recv {
        break;
      }
      if moved == 0 && !wire.advance(&mut sender, &mut receiver) {
        break;
      }
    }
    (received, sender)
  }

  /// Like [`transfer_on`] with no path delay.
  fn transfer(streams: &[(u64, Vec<u8>)], channel: Channel) -> (BTreeMap<u64, Vec<u8>>, u64) {
    let (received, sender) = transfer_on(
      streams,
      ControllerKind::NewReno,
      FRAME_CAP,
      Wire::new(MS, channel),
    );
    (received, sender.retransmitted())
  }

  /// A stream of `len` bytes with a per-stream fingerprint, so a demultiplexing mix-up would show.
  fn stream_content(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
      .map(|i| u8::try_from((usize::from(seed).wrapping_add(i)) % 251).unwrap_or(0))
      .collect()
  }

  /// Sends one request on stream 1 and shows the probe path live: unacknowledged, a probe resends it
  /// (the retransmit counter moves). Returns the sender with the request still in flight.
  fn request_in_flight_and_probed() -> Connection {
    let mut sender = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    sender.open(1, &stream_content(9, 40));
    let (_pn, frames) = sender
      .poll_transmit(0, FRAME_CAP)
      .expect("the request goes out");
    assert!(
      frames
        .iter()
        .any(|f| matches!(f, Frame::Stream { stream_id: 1, .. }))
    );
    assert_eq!(sender.in_flight_count(), 1);
    assert!(sender.probe(), "a probe finds the packet in flight");
    let (_pn, resent) = sender
      .poll_transmit(0, FRAME_CAP)
      .expect("the probe retransmits");
    assert!(
      resent
        .iter()
        .any(|f| matches!(f, Frame::Stream { stream_id: 1, .. }))
    );
    assert!(sender.retransmitted() >= 1, "the retransmit path ran");
    sender
  }

  /// AC (§4.8 "Membership" — a probe abandoned at its deadline; RFC 9000 §2.4): once a stream is
  /// forgotten, none of its data is ever retransmitted and its bytes leave the in-flight accounting.
  /// Non-vacuous: before the forget, the same probe resends the packet.
  #[test]
  fn a_forgotten_streams_frames_are_never_retransmitted() {
    let mut sender = request_in_flight_and_probed();
    let retransmitted_before = sender.retransmitted();
    sender.forget_stream(1);
    assert_eq!(
      sender.in_flight_count(),
      0,
      "its packets carried nothing else"
    );
    assert_eq!(
      sender.bytes_in_flight(),
      0,
      "its bytes left the in-flight accounting"
    );
    assert!(!sender.probe(), "a probe finds nothing to resend");
    assert!(
      sender.poll_transmit(0, FRAME_CAP).is_none(),
      "no frame of the forgotten stream is retransmitted"
    );
    assert_eq!(sender.retransmitted(), retransmitted_before);
  }

  /// AC (§4.10a §8, RFC 9000 §12.3): a packet whose number was already processed is discarded — its
  /// frames are not applied again and it owes no fresh acknowledgement.
  #[test]
  fn a_duplicate_packet_number_is_discarded_not_processed_again() {
    let mut sender = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    let mut receiver = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    sender.open(7, &stream_content(0xAB, 40));
    let (pn, frames) = sender
      .poll_transmit(0, FRAME_CAP)
      .expect("the request goes out");
    receiver.handle_incoming(0, pn, &frames);
    assert!(
      !receiver.read_stream(0, 7).is_empty(),
      "the first receipt delivered bytes"
    );
    assert_eq!(receiver.duplicates_discarded(), 0);
    assert!(
      receiver.poll_transmit(0, FRAME_CAP).is_some(),
      "the first receipt owes an acknowledgement"
    );
    receiver.handle_incoming(0, pn, &frames);
    assert_eq!(receiver.duplicates_discarded(), 1);
    assert!(
      receiver.read_stream(0, 7).is_empty(),
      "the duplicate delivered no further bytes"
    );
    assert!(
      receiver.poll_transmit(0, FRAME_CAP).is_none(),
      "the duplicate owed no fresh acknowledgement"
    );
  }

  /// AC (§4.9; RFC 9000 §12.2): a packet carries several frames when the budget allows, and multiplexed
  /// streams still arrive exactly once, in order, under loss and reorder, under every law. Non-vacuous:
  /// a packet carried more than one stream frame, and the loss path ran.
  #[test]
  fn several_frames_per_packet_survive_loss_and_reorder() {
    let streams = vec![
      (1u64, stream_content(1, 500)),
      (3u64, stream_content(2, 20)),
      (7u64, stream_content(3, 300)),
    ];
    for law in LAWS {
      let (received, widest, retransmitted) = packed_transfer(law, &streams);
      for (id, content) in &streams {
        assert_eq!(
          received.get(id),
          Some(content),
          "{law:?}: stream {id} arrived exactly"
        );
      }
      assert!(
        widest > 1,
        "{law:?}: the widest packet carried {widest} stream frames"
      );
      assert!(retransmitted > 0, "{law:?}: the loss path ran");
    }
  }

  /// Runs `streams` framed at [`PACKET_BUDGET`] under `law`, dropping the third and eleventh packets and
  /// reversing each sender batch; returns what arrived, the most stream frames one packet carried, and the
  /// frames retransmitted.
  fn packed_transfer(
    law: ControllerKind,
    streams: &[(u64, Vec<u8>)],
  ) -> (BTreeMap<u64, Vec<u8>>, usize, u64) {
    let mut sender = fixed_window(PACKET_BUDGET, law);
    for (id, content) in streams {
      sender.open(*id, content);
    }
    let mut receiver = fixed_window(PACKET_BUDGET, law);
    let mut wire = Wire::new(MS, Channel::reordering(vec![3, 11]));
    let mut received: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for _ in 0..1_000_000 {
      let mut moved = wire.send(&mut sender, true, PACKET_BUDGET, |_| {});
      moved += wire.deliver(&mut sender, &mut receiver);
      for id in receiver.recv_stream_ids() {
        received
          .entry(id)
          .or_default()
          .extend(receiver.read_stream(wire.now, id));
      }
      moved += wire.send(&mut receiver, false, PACKET_BUDGET, |_| {});
      moved += wire.deliver(&mut sender, &mut receiver);
      if sender.send_complete()
        && streams
          .iter()
          .all(|(id, _)| receiver.recv_stream_complete(*id))
      {
        break;
      }
      if moved == 0 {
        assert!(wire.advance(&mut sender, &mut receiver), "{law:?}: stalled");
      }
    }
    (received, wire.widest, sender.retransmitted())
  }

  /// AC (§4.10a §8): three streams multiplexed over one connection each arrive exactly, in order, with no
  /// loss — and none is confused for another.
  #[test]
  fn three_streams_multiplex_losslessly() {
    let streams = vec![
      (1u64, stream_content(1, 500)),
      (3u64, stream_content(2, 20)),
      (7u64, stream_content(3, 300)),
    ];
    let (received, retransmits) = transfer(&streams, Channel::new(Vec::new()));
    for (id, content) in &streams {
      assert_eq!(
        received.get(id),
        Some(content),
        "stream {id} arrived exactly"
      );
    }
    assert_eq!(retransmits, 0, "nothing retransmitted with no loss");
  }

  /// AC (§4.10a §8, the connection tier of the dual-level credit law): the connection-wide flow-control
  /// window bounds the total unread bytes across all streams. Non-vacuous: the in-flight total reached
  /// within a frame of the window.
  #[test]
  fn the_connection_window_bounds_total_in_flight_across_streams() {
    let window = initial_receive_window(FRAME_CAP);
    let streams: Vec<(u64, Vec<u8>)> = vec![
      (1, stream_content(1, 20)),
      (3, stream_content(2, 20)),
      (7, stream_content(3, 20)),
    ];
    assert!((streams.len() as u64) * 20 > window && 20 < window);
    let (received, peak) = window_bounded_transfer(&streams, window);
    for (id, content) in &streams {
      assert_eq!(
        received.get(id),
        Some(content),
        "stream {id} arrived exactly"
      );
    }
    assert!(
      peak >= window - FRAME_CAP as u64,
      "the connection window was never saturated ({peak})"
    );
  }

  /// Runs `streams` over a lossless path, asserting on every send that the total sent across streams is
  /// within `window` of the total read; returns what arrived and the largest such lead seen.
  fn window_bounded_transfer(
    streams: &[(u64, Vec<u8>)],
    window: u64,
  ) -> (BTreeMap<u64, Vec<u8>>, u64) {
    let mut sender = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    for (id, content) in streams {
      sender.open(*id, content);
    }
    let mut receiver = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    let mut wire = Wire::new(MS, Channel::new(Vec::new()));
    let mut received: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut peak = 0u64;
    for _ in 0..1_000_000 {
      let reads: u64 = streams
        .iter()
        .map(|(id, _)| receiver.read_offset(*id))
        .sum();
      let mut moved = wire.send(&mut sender, true, FRAME_CAP, |from| {
        let sent: u64 = streams.iter().map(|(id, _)| from.send_offset(*id)).sum();
        peak = peak.max(sent - reads);
        assert!(
          sent <= reads + window,
          "the connection raced {sent} > {reads} + {window}"
        );
      });
      moved += wire.deliver(&mut sender, &mut receiver);
      for id in receiver.recv_stream_ids() {
        received
          .entry(id)
          .or_default()
          .extend(receiver.read_stream(wire.now, id));
      }
      moved += wire.send(&mut receiver, false, FRAME_CAP, |_| {});
      moved += wire.deliver(&mut sender, &mut receiver);
      if sender.send_complete()
        && streams
          .iter()
          .all(|(id, _)| receiver.recv_stream_complete(*id))
      {
        break;
      }
      if moved == 0 {
        assert!(wire.advance(&mut sender, &mut receiver), "stalled");
      }
    }
    (received, peak)
  }

  /// AC (§4.10a §8, RFC 9000 §13.2.4 ACK-of-ACK): on a connection reused across many exchanges the
  /// receive-side acknowledgement set stays bounded while every exchange still delivers exactly.
  #[test]
  fn ack_of_ack_bounds_the_receive_set_over_a_reused_connection() {
    /// Shape: enough exchanges that an unpruned set would dwarf the bound below.
    const EXCHANGES: u64 = 50;
    /// Shape: a few packets each way per exchange.
    const BODY: usize = 24;
    let mut a = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    let mut b = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    let mut now = 0u64;
    let mut peak_tracked = 0usize;
    for i in 0..EXCHANGES {
      let sid = i * 2 + 1;
      a.open(sid, &stream_content(u8::try_from(i % 7).unwrap_or(0), BODY));
      b.open(
        sid,
        &stream_content(u8::try_from(i % 5).unwrap_or(0).wrapping_add(100), BODY),
      );
      for guard in 0..100_000 {
        assert!(guard < 99_999, "the exchange must make progress");
        // One packet each way per step, so each data packet also carries the pending acknowledgement.
        let a_sent = a.poll_transmit(now, FRAME_CAP);
        if let Some((pn, frames)) = &a_sent {
          b.handle_incoming(now, *pn, frames);
        }
        let _ = b.read_stream(now, sid);
        let b_sent = b.poll_transmit(now, FRAME_CAP);
        if let Some((pn, frames)) = &b_sent {
          a.handle_incoming(now, *pn, frames);
        }
        let _ = a.read_stream(now, sid);
        peak_tracked = peak_tracked.max(a.acks_tracked()).max(b.acks_tracked());
        if a.recv_stream_complete(sid)
          && b.recv_stream_complete(sid)
          && a.send_complete()
          && b.send_complete()
        {
          break;
        }
        if a_sent.is_none() && b_sent.is_none() {
          now = [a.next_timeout(), b.next_timeout()]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(now + MS)
            .max(now);
          a.on_timeout(now);
          b.on_timeout(now);
        }
      }
      a.forget_stream(sid);
      b.forget_stream(sid);
    }
    assert!(peak_tracked > 0, "exchanges actually ran and were tracked");
    assert!(
      peak_tracked < 40,
      "ACK-of-ACK kept the receive set bounded (peak {peak_tracked})"
    );
  }

  /// AC (§4.10a §8; RFC 9002 §6.2): a lone data packet that is dropped — a tail loss neither threshold
  /// can see, since nothing after it is acknowledged — is recovered by the probe timeout, under every law.
  #[test]
  fn a_single_packet_tail_loss_is_probed() {
    let streams = vec![(1u64, stream_content(4, 4))];
    for law in LAWS {
      let (received, sender) = transfer_on(
        &streams,
        law,
        FRAME_CAP,
        Wire::new(MS, Channel::new(vec![0])),
      );
      assert_eq!(
        received.get(&1),
        Some(&stream_content(4, 4)),
        "{law:?}: the lone packet arrived"
      );
      assert!(
        sender.retransmitted() >= 1,
        "{law:?}: the probe recovered it"
      );
    }
  }

  /// RFC 9002 §5.1: the round trip is measured from the largest newly acknowledged packet's send to the
  /// acknowledgement — over a path of 20 ms each way, 40 ms.
  #[test]
  fn the_rtt_is_measured_from_send_to_acknowledgement() {
    let (_, sender) = transfer_on(
      &[(1u64, stream_content(1, 24))],
      ControllerKind::NewReno,
      FRAME_CAP,
      Wire::new(20 * MS, Channel::new(Vec::new())),
    );
    assert!(sender.rtt().has_sample());
    assert_eq!(
      sender.rtt().min_rtt(),
      40 * MS,
      "the minimum is the path's round trip"
    );
  }

  /// AC (§4.10a §8): multiplexed streams all arrive exactly despite dropped packets, under every law.
  #[test]
  fn multiplexed_streams_survive_loss() {
    let streams = vec![
      (1u64, stream_content(1, 400)),
      (2u64, stream_content(9, 400)),
    ];
    for law in LAWS {
      let (received, sender) = transfer_on(
        &streams,
        law,
        FRAME_CAP,
        Wire::new(MS, Channel::new(vec![3, 4])),
      );
      for (id, content) in &streams {
        assert_eq!(
          received.get(id),
          Some(content),
          "{law:?}: stream {id} arrived despite loss"
        );
      }
      assert!(
        sender.retransmitted() >= 1,
        "{law:?}: the loss-recovery path ran"
      );
    }
  }

  /// RFC 9002 §6.1.2: a packet lost behind fewer than three later packets is still declared lost once it
  /// is older than 9/8 of the RTT — by the loss timer, not a probe timeout — and retransmitted.
  #[test]
  fn the_time_threshold_declares_a_loss_the_packet_threshold_cannot() {
    let mut sender = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    let mut receiver = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    sender.seed_rtt(10 * MS, 0);
    sender.open(1, &stream_content(1, 3 * FRAME_CAP));
    let mut packets: Vec<(u64, Vec<Frame>)> = Vec::new();
    let mut sent_at = 0;
    while packets.len() < 3 {
      match sender.poll_transmit(sent_at, FRAME_CAP) {
        Some(packet) => packets.push(packet),
        None => sent_at = sender.next_timeout().expect("the pacer's release"),
      }
    }
    assert!(
      sent_at < 5 * MS,
      "the three left within the first few milliseconds"
    );
    // Packet 0 is lost; 1 and 2 arrive and are acknowledged at 10 ms.
    for (pn, frames) in &packets[1..] {
      receiver.handle_incoming(5 * MS, *pn, frames);
    }
    let (pn, ack) = receiver
      .poll_transmit(5 * MS, FRAME_CAP)
      .expect("an acknowledgement");
    sender.handle_incoming(10 * MS, pn, &ack);
    assert_eq!(
      sender.retransmitted(),
      0,
      "one packet behind two is not lost by count"
    );
    let due = sender.next_timeout().expect("the loss timer is armed");
    assert!(
      due <= 10 * MS + 10 * MS * 9 / 8,
      "armed at 9/8 of the RTT past the send, not a PTO"
    );
    assert!(sender.on_timeout(due));
    assert_eq!(
      sender.retransmitted(),
      1,
      "the timer declared packet 0 lost"
    );
  }

  /// RFC 9002 §7.7: once the RTT is known, ack-eliciting packets leave spaced at the pacing rate rather
  /// than all at once, after an initial burst of one send quantum.
  #[test]
  fn data_is_paced_after_the_first_quantum() {
    // An initial window covering the whole stream, so flow credit never stops the sender (the test has no
    // receiver to grant more) and only the pacer spaces the packets.
    let shape = ConnectionShape {
      max_datagram: 1000,
      initial_window: 20_000,
      receive_ceiling: 20_000,
      controller: ControllerKind::NewReno,
    };
    let mut sender = Connection::new(shape, 0, 1);
    sender.seed_rtt(100 * MS, 0);
    sender.open(1, &vec![7u8; 20_000]);
    let mut sends = Vec::new();
    let mut now = 0;
    for _ in 0..1000 {
      if sends.len() == 6 {
        break;
      }
      match sender.poll_transmit(now, 1000) {
        Some(_) => sends.push(now),
        None => {
          now = sender
            .pacing_release
            .expect("only the pacer holds the sender")
        }
      }
    }
    assert_eq!(sends.len(), 6, "all six packets left");
    // cwnd 10 kB over 100 ms at 1.25× is 125 kB/s: a 1000-byte packet every 8 ms after the quantum
    // (the 2-packet floor) leaves.
    assert_eq!(&sends[..2], &[0, 0], "the first quantum leaves at once");
    for pair in sends[2..].windows(2) {
      assert_eq!(pair[1] - pair[0], 8 * MS, "then one packet per 8 ms");
    }
  }

  /// RFC 9002 §7.6.2: losses spanning more than three probe timeouts with nothing acknowledged between —
  /// the path went dark — collapse the window to the minimum.
  #[test]
  fn persistent_congestion_collapses_the_window() {
    let mut sender = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    let mut receiver = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    sender.seed_rtt(10 * MS, 0);
    sender.open(1, &stream_content(1, 20 * FRAME_CAP));
    // Everything sent in the first 500 ms is lost — the original flight and the probes the backed-off
    // timeout sends at about 31, 91, 211 and 451 ms — then the last probe gets through and is
    // acknowledged: the lost packets span 1–211 ms, past three PTOs (3 × 30 ms), with nothing acknowledged
    // between them.
    let mut now = 1;
    let mut last = None;
    while now < 500 * MS {
      if let Some(packet) = sender.poll_transmit(now, FRAME_CAP) {
        last = Some((now, packet));
      } else {
        let next = sender.next_timeout().unwrap_or(now + MS).max(now + 1);
        if next >= 500 * MS {
          break;
        }
        now = next;
        sender.on_timeout(now);
      }
    }
    // The path comes back: the last probe arrives a millisecond after it left, and its acknowledgement a
    // millisecond later — a prompt round trip, as a restored path gives.
    let (sent_at, (pn, frames)) = last.expect("something was sent");
    receiver.handle_incoming(sent_at + MS, pn, &frames);
    let (ack_pn, ack) = receiver
      .poll_transmit(sent_at + MS, FRAME_CAP)
      .expect("an acknowledgement");
    sender.handle_incoming(sent_at + 2 * MS, ack_pn, &ack);
    assert_eq!(
      sender.congestion_window(),
      2 * FRAME_CAP as u64,
      "the window collapsed to the minimum"
    );
  }

  /// §4.10a §8 "BDP-autotuned" (Chromium QUIC's rule): over a path whose BDP far exceeds the initial
  /// window, the receive window doubles whenever a window is read within two round trips, up to the
  /// ceiling and never past it — and the transfer completes.
  #[test]
  fn the_receive_window_autotunes_to_its_ceiling() {
    let cap = 1200usize;
    let initial = initial_receive_window(cap);
    let ceiling = 64 * initial;
    let shape = ConnectionShape {
      max_datagram: cap as u64,
      initial_window: initial,
      receive_ceiling: ceiling,
      controller: ControllerKind::NewReno,
    };
    let mut sender = Connection::new(shape, 0, 1);
    let mut receiver = Connection::new(shape, 0, 1);
    // A receiver only acknowledges, which draws no RTT sample of its own; a session's handshake seeds both
    // ends' estimates (`Endpoint::establish`), which this does in its place.
    sender.seed_rtt(40 * MS, 0);
    receiver.seed_rtt(40 * MS, 0);
    let content = vec![5u8; 4 << 20];
    sender.open(1, &content);
    let mut wire = Wire::new(20 * MS, Channel::new(Vec::new()));
    let mut received = Vec::new();
    for _ in 0..10_000_000 {
      let mut moved = wire.send(&mut sender, true, cap, |_| {});
      moved += wire.deliver(&mut sender, &mut receiver);
      received.extend(receiver.read_stream(wire.now, 1));
      moved += wire.send(&mut receiver, false, cap, |_| {});
      moved += wire.deliver(&mut sender, &mut receiver);
      if receiver.recv_stream_complete(1) && sender.send_complete() {
        break;
      }
      if moved == 0 {
        assert!(wire.advance(&mut sender, &mut receiver), "stalled");
      }
    }
    assert_eq!(received.len(), content.len(), "the transfer completed");
    let (window, growths) = receiver.receive_window();
    assert!(growths > 0, "the window grew ({growths} times)");
    assert_eq!(window, ceiling, "and stopped at the ceiling");
  }

  proptest! {
    /// The behavioural oracle (R5): for any streams, any loss pattern, and every control law, the
    /// receiver reassembles each stream exactly — in order, each byte once, never confused.
    #[test]
    fn any_streams_any_loss_still_deliver(
      lens in prop::collection::vec(0usize..300, 1..4),
      drops in prop::collection::vec(0u64..150, 0..30),
      law in 0usize..LAWS.len(),
    ) {
      let streams: Vec<(u64, Vec<u8>)> = lens
        .iter()
        .enumerate()
        .map(|(i, &len)| (u64::try_from(i * 2 + 1).unwrap_or(1), stream_content(u8::try_from(i).unwrap_or(0), len)))
        .collect();
      let (received, _) = transfer_on(&streams, LAWS[law], FRAME_CAP, Wire::new(MS, Channel::new(drops)));
      for (id, content) in &streams {
        prop_assert_eq!(received.get(id), Some(content));
      }
    }

    /// The reordering oracle: any streams, any loss, out-of-order delivery, every law — still exact.
    #[test]
    fn any_streams_survive_loss_and_reorder(
      lens in prop::collection::vec(0usize..200, 1..4),
      drops in prop::collection::vec(0u64..120, 0..20),
      law in 0usize..LAWS.len(),
    ) {
      let streams: Vec<(u64, Vec<u8>)> = lens
        .iter()
        .enumerate()
        .map(|(i, &len)| (u64::try_from(i * 2 + 1).unwrap_or(1), stream_content(u8::try_from(i).unwrap_or(0), len)))
        .collect();
      let (received, _) = transfer_on(&streams, LAWS[law], FRAME_CAP, Wire::new(MS, Channel::reordering(drops)));
      for (id, content) in &streams {
        prop_assert_eq!(received.get(id), Some(content));
      }
    }
  }

  /// A long transfer under steady random loss (5 % both ways, the bake-off's thin-link case) completes
  /// under every law and many seeds — no deadlock between the sender's credit and the receiver's
  /// acknowledgements.
  #[test]
  fn a_long_transfer_under_random_loss_completes() {
    for law in LAWS {
      for seed in 1..=20u64 {
        let mut rng = slates_machine::stats::Xorshift::new(seed);
        let drops: Vec<u64> = (0..20_000u64).filter(|_| rng.below(20) == 0).collect();
        let streams = vec![(1u64, stream_content(7, 16 * 1024))];
        let (received, sender) =
          transfer_on(&streams, law, 64, Wire::new(10 * MS, Channel::new(drops)));
        assert_eq!(
          received.get(&1).map(Vec::len),
          Some(16 * 1024),
          "{law:?} seed {seed}: stalled with {} in flight, window {}, rtx {}",
          sender.bytes_in_flight(),
          sender.congestion_window(),
          sender.retransmitted()
        );
      }
    }
  }

  /// §4.8 request/reply on one stream id (`Endpoint::serve_once`): the server has served a request and
  /// its reply is in flight on the same id; a late copy of the request then arrives (a probe the client
  /// sent before it heard anything) and the server discards the stale request below its floor. Discarding
  /// forgets the stale receive half only: the reply stays in flight and is still delivered. Regression:
  /// the discard forgot the whole stream, so the reply left tracking unacknowledged, counted complete, and
  /// the client waited forever (a 64 kbit/s, 5 %-loss bake-off run deadlocked; 2026-09-27).
  #[test]
  fn discarding_a_late_request_copy_keeps_the_reply_in_flight() {
    let id = 7;
    let mut client = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    let mut server = fixed_window(FRAME_CAP, ControllerKind::NewReno);
    client.open(id, &stream_content(1, 4));
    let (request_pn, request) = client.poll_transmit(0, FRAME_CAP).expect("the request");
    server.handle_incoming(0, request_pn, &request);
    assert_eq!(
      server.read_stream(0, id).len(),
      4,
      "the server read the request"
    );
    assert!(server.recv_stream_complete(id));
    // The server replies on the same id; the reply packet is lost on the path.
    server.open(id, &stream_content(2, 4));
    let (_lost_pn, _lost_reply) = server.poll_transmit(0, FRAME_CAP).expect("the reply");
    // The client, having heard nothing, probes: a copy of its request in a new packet reaches the server.
    assert!(client.probe());
    let (probe_pn, probe) = client.poll_transmit(MS, FRAME_CAP).expect("the probe");
    server.handle_incoming(MS, probe_pn, &probe);
    // The server's exchange floor has passed the id: the late copy is discarded.
    server.discard_streams_below(MS, id + 1);
    assert!(!server.send_complete(), "the reply is still owed");
    assert_eq!(
      server.in_flight_count(),
      1,
      "the reply packet is still in flight"
    );
    // The reply is eventually recovered and delivered: the probe timeout resends it.
    let now = server.next_timeout().expect("the reply's probe timer");
    assert!(server.on_timeout(now), "the probe timeout owes a probe");
    while let Some((pn, frames)) = server.poll_transmit(now, FRAME_CAP) {
      client.handle_incoming(now, pn, &frames);
    }
    assert_eq!(
      client.read_stream(now, id),
      stream_content(2, 4),
      "the reply reached the client"
    );
    assert!(client.recv_stream_complete(id));
  }
}
