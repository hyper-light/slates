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
//! Owed: connection-level `MaxData` (only per-stream `MaxStreamData` is enforced), congestion control
//! (whose validation needs a real network), several ack-eliciting frames per packet (an MTU budget),
//! and driving the tail-loss probe from a real timeout.

use std::collections::{BTreeMap, VecDeque};

use crate::conn::{AckGenerator, REORDER_THRESHOLD, SentTracker};
use crate::flow::FlowController;
use crate::session::Frame;
use crate::stream::{StreamAssembler, StreamSender};

/// The initial receive-window credit, in bytes, for a frame cap of `max_frame_len`.
/// Derived: `(REORDER_THRESHOLD + 1) × max_frame_len` — the least in-flight data that keeps
/// reorder-based loss detection working. A loss is declared when `REORDER_THRESHOLD` later packets are
/// acknowledged past a gap (RFC 9002 §6.1.1); a window that admits one lost packet plus that many past
/// it lets the gap form. Anchored to `conn::REORDER_THRESHOLD` and the frame size. The BDP-autotuned
/// growth above this floor (the design's `k × frame_cap` with `k` measured) is owed.
pub fn initial_receive_window(max_frame_len: usize) -> u64 {
  (REORDER_THRESHOLD + 1).saturating_mul(max_frame_len as u64)
}

/// One end of a reliable, flow-controlled, multi-stream connection. The send side is a set of streams
/// (each a source bounded by the credit the peer advertises for it) served round-robin, plus the
/// connection-wide in-flight tracker and retransmission queue; the receive side is a set of ordered
/// reassemblers keyed by stream id, the acknowledgement generator, and the flow-control accounting that
/// advertises each stream's credit a bounded window ahead of what has been read from it. A non-vacuity
/// counter records how many frames have actually been retransmitted.
pub struct Connection {
  /// The send streams, keyed by id (each framed only within the credit the peer has advertised for it).
  send_streams: BTreeMap<u64, StreamSender>,
  /// Send stream ids in the order opened, with a cursor, for round-robin scheduling.
  send_order: Vec<u64>,
  /// The round-robin cursor into `send_order`.
  send_cursor: usize,
  /// Connection-wide in-flight packet tracking and loss detection (packet numbers are per-connection).
  sent: SentTracker,
  /// Frames freed by loss detection or a probe, awaiting retransmission (one per packet).
  retransmit: VecDeque<Frame>,
  /// The receive reassemblers, keyed by stream id (created when a stream's first frame arrives).
  recv_streams: BTreeMap<u64, StreamAssembler>,
  /// Which packet numbers have arrived, and the acknowledgement to send back.
  acks: AckGenerator,
  /// The receive-side flow-control accounting (per stream): advertises credit a window ahead of reads.
  flow: FlowController,
  /// The credit window kept ahead of each read cursor (bytes).
  window_ahead: u64,
  /// Set when an ack-eliciting packet has arrived and its acknowledgement has not yet been sent.
  ack_owed: bool,
  /// How many frames this end has retransmitted (the non-vacuity counter for the loss-recovery path).
  retransmitted: u64,
}

impl Connection {
  /// A fresh connection with no streams yet, advertising `window_ahead` bytes of receive credit ahead
  /// of each stream's read cursor (see [`initial_receive_window`]).
  pub fn new(window_ahead: u64) -> Connection {
    Connection {
      send_streams: BTreeMap::new(),
      send_order: Vec::new(),
      send_cursor: 0,
      sent: SentTracker::new(),
      retransmit: VecDeque::new(),
      recv_streams: BTreeMap::new(),
      acks: AckGenerator::new(),
      flow: FlowController::new(window_ahead),
      window_ahead,
      ack_owed: false,
      retransmitted: 0,
    }
  }

  /// Opens send stream `stream_id` carrying the whole of `data`, and finishes it. The send side starts
  /// with the initial window of credit (both ends derive the same window, R8); more is granted only as
  /// the peer's `MaxStreamData` for this stream arrives, so the sender never races more than a window
  /// ahead of that stream's reader.
  pub fn open(&mut self, stream_id: u64, data: &[u8]) {
    let mut sender = StreamSender::new();
    sender.write(data);
    sender.grant_credit(self.window_ahead);
    sender.finish();
    if self.send_streams.insert(stream_id, sender).is_none() {
      self.send_order.push(stream_id);
    }
  }

  /// The next packet to send, as `(packet_number, frames)`, or `None` when there is nothing to send
  /// right now (nothing to retransmit, no send stream with fresh data within its credit, and no
  /// acknowledgement owed). Puts at most one ack-eliciting frame in the packet — a retransmitted frame
  /// first, otherwise a fresh frame from the next send stream that has one (round-robin) — and, if an
  /// acknowledgement is owed, appends it followed by the current per-stream receive credit. Only a
  /// packet that carries an ack-eliciting frame is tracked for loss (RFC 9002 §2).
  pub fn poll_transmit(&mut self, max_frame_len: usize) -> Option<(u64, Vec<Frame>)> {
    // One ack-eliciting frame: a retransmission takes priority over fresh stream data.
    let reliable = match self.retransmit.pop_front() {
      Some(frame) => Some(frame),
      None => self.next_fresh_frame(max_frame_len),
    };

    let mut frames = Vec::new();
    if let Some(frame) = reliable.clone() {
      frames.push(frame);
    }
    if self.ack_owed
      && let Some(ack) = self.acks.ack_frame()
    {
      frames.push(ack);
      // Piggyback each received stream's current credit on the acknowledgement, so a lost credit frame
      // is re-advertised with the next one (a `MaxStreamData` is not itself retransmitted). Absolute
      // values make a duplicate or reordered one idempotent (the sender ignores a lower grant).
      for &stream_id in self.recv_streams.keys() {
        frames.push(self.flow.stream_credit_frame(stream_id));
      }
      self.ack_owed = false;
    }
    if frames.is_empty() {
      return None;
    }

    let pn = self.sent.next_pn();
    // Track only the ack-eliciting frame for loss/retransmission; the acknowledgement and credit frames
    // are regenerated fresh each time, never retransmitted stale.
    if let Some(frame) = reliable {
      self.sent.on_sent(pn, vec![frame]);
    }
    Some((pn, frames))
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

  /// Takes a received packet: records its number for acknowledgement, demultiplexes each stream frame
  /// to its reassembler (creating one on first sight, and admitting the segment within the flow-control
  /// window the sender was never allowed to exceed), processes any acknowledgement, applies each
  /// stream's advertised send credit, and queues for retransmission whatever a new acknowledgement
  /// declares lost. An acknowledgement-only packet does not oblige an acknowledgement in return
  /// (RFC 9002 §2), which keeps two ends from trading acknowledgements forever.
  pub fn handle_incoming(&mut self, pn: u64, frames: &[Frame]) {
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
          let assembler = self
            .recv_streams
            .entry(*stream_id)
            .or_insert_with(|| StreamAssembler::new(self.window_ahead));
          // The window is the flow-control ceiling the sender could not exceed; a duplicate or
          // reordered segment is deduped, and a refusal would mean the peer broke flow control.
          assembler.grant_window(window);
          let _ = assembler.offer(*offset, data, *fin);
        }
        Frame::Ack { largest, range } => {
          self.sent.on_ack(*largest, *range);
        }
        // The peer's advertised send credit for one stream: raise that stream's send ceiling (monotonic).
        Frame::MaxStreamData { stream_id, max } => {
          if let Some(sender) = self.send_streams.get_mut(stream_id) {
            sender.grant_credit(*max);
          }
        }
        // Connection-level credit (owed) and PADDING: nothing to do yet.
        Frame::MaxData { .. } => {}
      }
    }
    if ack_eliciting {
      self.ack_owed = true;
    }
    // A just-processed acknowledgement may have opened a gap past the reorder threshold.
    let lost = self.sent.take_lost();
    self.queue_retransmit(lost);
  }

  /// Retransmits the oldest in-flight packet when the connection has stalled with packets still in
  /// flight — the probe that recovers a lost tail the reorder threshold cannot see. Returns whether
  /// anything was queued. The caller drives this only when [`poll_transmit`] has returned `None` yet
  /// delivery is not complete.
  ///
  /// [`poll_transmit`]: Connection::poll_transmit
  pub fn probe(&mut self) -> bool {
    let frames = self.sent.probe_oldest();
    let probed = !frames.is_empty();
    self.queue_retransmit(frames);
    probed
  }

  /// Drains the bytes now contiguous on receive stream `stream_id` (in order, each once) and slides
  /// that stream's flow-control window forward by what was consumed, so the next acknowledgement
  /// advertises fresh credit a window ahead of its new read cursor. Empty if the stream is unknown.
  pub fn read_stream(&mut self, stream_id: u64) -> Vec<u8> {
    let Some(assembler) = self.recv_streams.get_mut(&stream_id) else {
      return Vec::new();
    };
    let bytes = assembler.read();
    let read_offset = assembler.read_offset();
    self.flow.on_stream_consumed(stream_id, read_offset);
    bytes
  }

  /// The stream ids seen on the receive side so far (a frame has arrived for each).
  pub fn recv_stream_ids(&self) -> Vec<u64> {
    self.recv_streams.keys().copied().collect()
  }

  /// Whether receive stream `stream_id` is complete (all bytes through its `fin`). False if unknown.
  pub fn recv_stream_complete(&self, stream_id: u64) -> bool {
    self
      .recv_streams
      .get(&stream_id)
      .is_some_and(StreamAssembler::is_complete)
  }

  /// Whether every send stream has originated its whole data and every ack-eliciting packet has been
  /// acknowledged (nothing buffered to retransmit, nothing in flight).
  pub fn send_complete(&self) -> bool {
    self.retransmit.is_empty()
      && self.sent.in_flight_count() == 0
      && self.send_streams.values().all(StreamSender::is_drained)
  }

  /// How many frames this end has retransmitted — the non-vacuity counter proving the loss-recovery
  /// path actually ran, so a test over a lossy channel cannot pass with a dead retransmit path.
  pub fn retransmitted(&self) -> u64 {
    self.retransmitted
  }

  /// The largest packet number the peer has acknowledged (for sizing the truncated packet number the
  /// wire carries); `None` before the first acknowledgement.
  pub fn tx_largest_acked(&self) -> Option<u64> {
    self.sent.largest_acked()
  }

  /// The absolute offset send stream `stream_id` has framed so far — for asserting the
  /// never-whole-object invariant. Zero if the stream is unknown.
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

  /// The frame length cap the driver frames at, in the tests.
  const FRAME_CAP: usize = 8;

  /// A deterministic lossy, reordering channel the oracle drives the two ends over, dropping the
  /// transmission steps named in `drop_steps` (counted across both directions).
  struct Channel {
    drop_steps: Vec<u64>,
    step: u64,
  }

  impl Channel {
    fn new(drop_steps: Vec<u64>) -> Channel {
      Channel {
        drop_steps,
        step: 0,
      }
    }

    fn drops(&mut self) -> bool {
      let dropped = self.drop_steps.contains(&self.step);
      self.step = self.step.saturating_add(1);
      dropped
    }
  }

  /// The never-whole-object invariant to check while pumping a direction: the send streams and the
  /// window, so `from.send_offset(id) ≤ to.read_offset(id) + window` can be asserted per stream.
  type Invariant<'a> = Option<(&'a [(u64, Vec<u8>)], u64)>;

  /// Pumps every packet one end wants to send into the other over the channel (dropping the steps the
  /// channel names), returning whether anything was sent. Asserts the never-whole-object invariant for
  /// each stream on every transmission when `invariant` is given (the sending direction, where `from`
  /// has the send streams and `to` the read cursors).
  fn pump(
    from: &mut Connection,
    to: &mut Connection,
    channel: &mut Channel,
    invariant: Invariant<'_>,
  ) -> bool {
    let mut sent_any = false;
    while let Some((pn, frames)) = from.poll_transmit(FRAME_CAP) {
      sent_any = true;
      if let Some((streams, window)) = invariant {
        for (id, _) in streams {
          assert!(
            from.send_offset(*id) <= to.read_offset(*id) + window,
            "stream {id}: the sender raced more than a window ahead of the reader"
          );
        }
      }
      if !channel.drops() {
        to.handle_incoming(pn, &frames);
      }
    }
    sent_any
  }

  /// Runs a transfer of `streams` (each `(stream_id, content)`) from a sender to a receiver over
  /// `channel`, returning what the receiver reassembled per stream and how many frames were
  /// retransmitted. Asserts the never-whole-object invariant on every transmission.
  fn transfer(streams: &[(u64, Vec<u8>)], mut channel: Channel) -> (BTreeMap<u64, Vec<u8>>, u64) {
    let window = initial_receive_window(FRAME_CAP);
    let mut sender = Connection::new(window);
    for (id, content) in streams {
      sender.open(*id, content);
    }
    let mut receiver = Connection::new(window);
    let mut received: BTreeMap<u64, Vec<u8>> = BTreeMap::new();

    let mut guard = 0u64;
    loop {
      guard += 1;
      assert!(guard < 1_000_000, "the connection must make progress");

      let sent = pump(
        &mut sender,
        &mut receiver,
        &mut channel,
        Some((streams, window)),
      );
      for id in receiver.recv_stream_ids() {
        received
          .entry(id)
          .or_default()
          .extend(receiver.read_stream(id));
      }
      let acked = pump(&mut receiver, &mut sender, &mut channel, None);
      let progress = sent || acked;

      let all_recv = streams
        .iter()
        .all(|(id, _)| receiver.recv_stream_complete(*id));
      if sender.send_complete() && all_recv {
        break;
      }
      if !progress && !sender.probe() {
        break;
      }
    }
    (received, sender.retransmitted())
  }

  /// A stream of `len` bytes with a per-stream fingerprint, so a demultiplexing mix-up would show.
  fn stream_content(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
      .map(|i| u8::try_from((usize::from(seed).wrapping_add(i)) % 251).unwrap_or(0))
      .collect()
  }

  /// AC (§4.10a §8): three streams multiplexed over one connection each arrive exactly, in order, with
  /// no loss — and none is confused for another (each has its own fingerprint).
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

  /// AC (§4.10a §8): a lone data packet that is dropped — a tail loss the reorder threshold cannot
  /// see, since no later packet follows it — is recovered by the probe, so even a one-packet stream
  /// arrives. Guarantees the probe path runs (the retransmit counter proves it).
  #[test]
  fn a_single_packet_tail_loss_is_probed() {
    let streams = vec![(1u64, stream_content(4, 4))];
    let (received, retransmits) = transfer(&streams, Channel::new(vec![0]));
    assert_eq!(
      received.get(&1),
      Some(&stream_content(4, 4)),
      "the lone packet arrived"
    );
    assert!(
      retransmits >= 1,
      "the probe recovered the single lost packet"
    );
  }

  /// AC (§4.10a §8): the same multiplexed streams all arrive exactly despite a dropped packet, which
  /// is recovered by the reliability core — the retransmit counter is the non-vacuity check.
  #[test]
  fn multiplexed_streams_survive_loss() {
    let streams = vec![
      (1u64, stream_content(1, 400)),
      (2u64, stream_content(9, 400)),
    ];
    let (received, retransmits) = transfer(&streams, Channel::new(vec![3, 4]));
    for (id, content) in &streams {
      assert_eq!(
        received.get(id),
        Some(content),
        "stream {id} arrived despite loss"
      );
    }
    assert!(retransmits >= 1, "the loss-recovery path actually ran");
  }

  proptest! {
    /// The behavioural oracle (R5): for any set of streams and any loss pattern, the receiver
    /// reassembles each stream exactly — in order, each byte once, never confused with another.
    #[test]
    fn any_streams_any_loss_still_deliver(
      lens in prop::collection::vec(0usize..300, 1..4),
      drops in prop::collection::vec(0u64..150, 0..30),
    ) {
      let streams: Vec<(u64, Vec<u8>)> = lens
        .iter()
        .enumerate()
        .map(|(i, &len)| (u64::try_from(i * 2 + 1).unwrap_or(1), stream_content(u8::try_from(i).unwrap_or(0), len)))
        .collect();
      let (received, _) = transfer(&streams, Channel::new(drops));
      for (id, content) in &streams {
        prop_assert_eq!(received.get(id), Some(content));
      }
    }
  }
}
