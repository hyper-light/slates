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
//! Congestion control is now enforced (RFC 9002 §7 NewReno, `crate::congestion`): fresh sends are gated
//! on the window, which grows on acknowledgement and halves on loss; only the empirical *tuning* (the
//! window's exact constants, CUBIC, pacing) awaits a real network. Flow control is the ratified
//! dual-level credit law (`crate::flow`): a fresh frame is bounded both by its stream's `MaxStreamData`
//! and by the connection-wide `MaxData` (the total fresh bytes across all streams), each advertised a
//! window ahead of what the peer has consumed. Owed: several ack-eliciting frames per packet (an MTU
//! budget), and driving the tail-loss probe from a real timeout.

use std::collections::{BTreeMap, VecDeque};

use crate::congestion::Congestion;
use crate::conn::{AckGenerator, REORDER_THRESHOLD, SentTracker, tracked_bytes};
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
  /// The NewReno congestion controller: the sender's self-limit on in-flight data, grown on
  /// acknowledgement and reduced on loss. Gates fresh sends in [`Connection::poll_transmit`].
  congestion: Congestion,
  /// Send side: total fresh stream bytes sent across all streams (never counting a retransmission),
  /// the connection-wide counterpart to each stream's send offset. Held below `peer_max_data`.
  connection_sent: u64,
  /// Send side: the connection-wide flow-control ceiling the peer has advertised (`MaxData`) — the most
  /// total stream data it will accept across all streams. Starts at the initial window (both ends derive
  /// the same one, R8) and rises monotonically as the peer reads and re-advertises.
  peer_max_data: u64,
}

impl Connection {
  /// A fresh connection with no streams yet, advertising `window_ahead` bytes of receive credit ahead
  /// of each stream's read cursor (see [`initial_receive_window`]).
  pub fn new(window_ahead: u64) -> Connection {
    // The congestion window counts in max-datagram units. This dialect sends one frame per packet, so
    // the max datagram is the connection's frame cap, which the receive window is `REORDER_THRESHOLD +
    // 1` of (see `initial_receive_window`); recover it from `window_ahead` so `new` keeps one argument.
    let max_datagram = window_ahead / (REORDER_THRESHOLD + 1);
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
      congestion: Congestion::new(max_datagram),
      connection_sent: 0,
      // The peer's initial connection credit is the initial window it advertises before any read (its
      // `FlowController::connection_max` with nothing consumed); both ends derive the same one (R8).
      peer_max_data: window_ahead,
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
    // One ack-eliciting frame: a retransmission takes priority over fresh stream data. Fresh data is
    // gated by two limits — the congestion window (a retransmission is recovery, not new load, so it is
    // never gated) and the connection-wide flow-control credit the peer advertised (`peer_max_data`),
    // which bounds the *total* fresh bytes across all streams (each stream is also bounded by its own
    // `MaxStreamData`, inside `StreamSender`). `can_send` still lets a lone frame go when nothing is in
    // flight, so congestion never stalls the connection; a fresh frame is capped to the remaining
    // connection credit so `connection_sent` never crosses the ceiling.
    let connection_credit = self.peer_max_data.saturating_sub(self.connection_sent);
    let fresh_cap = max_frame_len.min(usize::try_from(connection_credit).unwrap_or(max_frame_len));
    let (reliable, fresh) = match self.retransmit.pop_front() {
      Some(frame) => (Some(frame), false),
      None if self.congestion.can_send(max_frame_len as u64) && connection_credit > 0 => {
        (self.next_fresh_frame(fresh_cap), true)
      }
      None => (None, false),
    };

    let mut frames = Vec::new();
    if let Some(frame) = reliable.clone() {
      frames.push(frame);
    }
    if self.ack_owed
      && let Some(ack) = self.acks.ack_frame(Self::max_ack_ranges(max_frame_len))
    {
      frames.push(ack);
      // Piggyback the connection-wide credit and each received stream's credit on the acknowledgement,
      // so a lost credit frame is re-advertised with the next one (credit frames are not themselves
      // retransmitted). Absolute values make a duplicate or reordered one idempotent (the peer ignores a
      // lower grant).
      frames.push(self.flow.connection_credit_frame());
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
    // are regenerated fresh each time, never retransmitted stale. Its stream bytes enter the congestion
    // window's in-flight count (a retransmission re-adds bytes a loss or probe earlier freed); a *fresh*
    // frame's bytes also advance the connection-wide sent total (a retransmission does not — those bytes
    // were already counted against the connection window when first sent).
    if let Some(frame) = reliable {
      let bytes = tracked_bytes(&frame);
      self.congestion.on_sent(bytes);
      if fresh {
        self.connection_sent = self.connection_sent.saturating_add(bytes);
      }
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
        Frame::Ack {
          largest,
          range,
          ranges,
        } => {
          let acked = self.sent.on_ack_frame(*largest, *range, ranges);
          self.congestion.on_ack(acked);
        }
        // The peer's advertised send credit for one stream: raise that stream's send ceiling (monotonic).
        Frame::MaxStreamData { stream_id, max } => {
          if let Some(sender) = self.send_streams.get_mut(stream_id) {
            sender.grant_credit(*max);
          }
        }
        // The peer's advertised connection-wide send credit: raise the ceiling on total fresh bytes
        // across all streams (monotonic — a duplicate or reordered lower grant is ignored).
        Frame::MaxData { max } => {
          self.peer_max_data = self.peer_max_data.max(*max);
        }
      }
    }
    if ack_eliciting {
      self.ack_owed = true;
    }
    // A just-processed acknowledgement may have opened a gap past the reorder threshold.
    let lost = self.sent.take_lost();
    if let Some(highest_pn) = lost.highest_pn {
      let lost_bytes: u64 = lost.frames.iter().map(tracked_bytes).sum();
      // The largest packet number sent so far bounds this congestion event's recovery period.
      let largest_sent = self.sent.peek_next_pn().saturating_sub(1);
      self
        .congestion
        .on_loss(lost_bytes, highest_pn, largest_sent);
    }
    self.queue_retransmit(lost.frames);
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
    // The probed bytes leave the in-flight count (the retransmission re-adds them); a probe is not a
    // congestion signal, so the window is unchanged.
    let probed_bytes: u64 = frames.iter().map(tracked_bytes).sum();
    self.congestion.on_probe_removed(probed_bytes);
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
    // Advance both flow-control tiers: this stream's cursor and (inside `on_stream_consumed`) the
    // connection-wide consumed total, so the next acknowledgement advertises connection credit a window
    // ahead of it — the dual-level credit law's connection tier. The connection total is a running sum,
    // so a later `forget_stream` of this completed stream does not un-count its bytes.
    self.flow.on_stream_consumed(stream_id, read_offset);
    bytes
  }

  /// The stream ids seen on the receive side so far (a frame has arrived for each).
  pub fn recv_stream_ids(&self) -> Vec<u64> {
    self.recv_streams.keys().copied().collect()
  }

  /// The sender's current congestion window in bytes — the most in-flight data it allows itself. Grows
  /// on acknowledgement and reduces on loss; exposed so a test can witness that response.
  pub fn congestion_window(&self) -> u64 {
    self.congestion.window()
  }

  /// The bytes currently in flight (sent, not yet acknowledged, freed, or declared lost).
  pub fn bytes_in_flight(&self) -> u64 {
    self.congestion.in_flight()
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

  /// Forgets a completed stream's send and receive state, so a long-lived connection carrying many
  /// exchanges does not accumulate finished streams without bound. The connection-wide packet-number
  /// space, acknowledgement and flow state are untouched (only the per-stream buffers are released), so
  /// packet numbers stay monotonic across exchanges — never reused, per RFC 9000 §12.3. (Compacting the
  /// acknowledgement set of a very long-lived connection is a separate bound, owed.)
  pub fn forget_stream(&mut self, stream_id: u64) {
    self.send_streams.remove(&stream_id);
    self.send_order.retain(|&id| id != stream_id);
    self.send_cursor = 0;
    self.recv_streams.remove(&stream_id);
    // Forget the per-stream flow-control watermark too, so reusing this id starts fresh; the
    // connection-wide consumed total is kept (its bytes stay counted, so the peer's credit never
    // regresses). Leaving a stale watermark would stall a reused stream (its offsets fall below it).
    self.flow.forget_stream(stream_id);
  }

  /// The next packet number this connection will assign — its packet-number cursor. Monotonic across
  /// every exchange the connection carries; a test asserts it never regresses (no reuse under the
  /// 1-RTT keys, RFC 9001 §5.3).
  pub fn tx_packet_number(&self) -> u64 {
    self.sent.peek_next_pn()
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

  /// Pumps every packet `from` wants to send into `to`, dropping the one whose packet number equals
  /// `drop_pn` the first time it is seen (then clearing it, so exactly one packet is dropped). Returns
  /// whether anything was sent. A small observing pump for the congestion test.
  fn pump_dropping(from: &mut Connection, to: &mut Connection, drop_pn: &mut Option<u64>) -> bool {
    let mut sent_any = false;
    while let Some((pn, frames)) = from.poll_transmit(FRAME_CAP) {
      sent_any = true;
      if *drop_pn == Some(pn) {
        *drop_pn = None;
        continue;
      }
      to.handle_incoming(pn, &frames);
    }
    sent_any
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

  /// Pumps sender→receiver, asserting the connection-wide never-whole-object bound (`Σ send ≤ Σ read +
  /// window`) on every transmission and tracking the largest total in flight seen. Returns whether
  /// anything was sent.
  fn pump_bounded(
    sender: &mut Connection,
    receiver: &mut Connection,
    channel: &mut Channel,
    streams: &[(u64, Vec<u8>)],
    window: u64,
    peak: &mut u64,
  ) -> bool {
    let mut sent = false;
    while let Some((pn, frames)) = sender.poll_transmit(FRAME_CAP) {
      sent = true;
      let total_sent: u64 = streams.iter().map(|(id, _)| sender.send_offset(*id)).sum();
      let total_read: u64 = streams
        .iter()
        .map(|(id, _)| receiver.read_offset(*id))
        .sum();
      *peak = (*peak).max(total_sent - total_read);
      assert!(
        total_sent <= total_read + window,
        "the connection raced {total_sent} > {total_read} + {window} ahead across all streams"
      );
      if !channel.drops() {
        receiver.handle_incoming(pn, &frames);
      }
    }
    sent
  }

  /// AC (§4.10a §8, the connection tier of the dual-level credit law): the connection-wide flow-control
  /// window bounds the *total* unread bytes across all streams — so several streams that each fit inside
  /// their own per-stream window cannot together race the connection more than one window ahead of the
  /// reader. Each stream here is smaller than the per-stream window, so per-stream credit never blocks;
  /// only the connection `MaxData` can bound the sum. Every byte still arrives.
  #[test]
  fn the_connection_window_bounds_total_in_flight_across_streams() {
    let window = initial_receive_window(FRAME_CAP);
    // Three streams of 20 bytes each: 20 < the per-stream window, so no stream is blocked on its own
    // credit; the 60-byte total is more than the connection window, so `MaxData` must throttle it.
    let streams: Vec<(u64, Vec<u8>)> = vec![
      (1, stream_content(1, 20)),
      (3, stream_content(2, 20)),
      (7, stream_content(3, 20)),
    ];
    assert!(
      (streams.len() as u64) * 20 > window && 20 < window,
      "the total exceeds the connection window while each stream fits its per-stream window"
    );

    let mut sender = Connection::new(window);
    for (id, content) in &streams {
      sender.open(*id, content);
    }
    let mut receiver = Connection::new(window);
    let mut received: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut channel = Channel::new(Vec::new());
    let mut peak_in_flight = 0u64;

    let mut guard = 0u64;
    loop {
      guard += 1;
      assert!(guard < 1_000_000, "the connection must make progress");
      // Pump sender→receiver, checking the connection-wide never-whole-object bound on every send.
      let sent = pump_bounded(
        &mut sender,
        &mut receiver,
        &mut channel,
        &streams,
        window,
        &mut peak_in_flight,
      );
      for id in receiver.recv_stream_ids() {
        received
          .entry(id)
          .or_default()
          .extend(receiver.read_stream(id));
      }
      let acked = pump(&mut receiver, &mut sender, &mut channel, None);
      let all_recv = streams
        .iter()
        .all(|(id, _)| receiver.recv_stream_complete(*id));
      if sender.send_complete() && all_recv {
        break;
      }
      if !(sent || acked) && !sender.probe() {
        break;
      }
    }

    for (id, content) in &streams {
      assert_eq!(
        received.get(id),
        Some(content),
        "stream {id} arrived exactly"
      );
    }
    // Non-vacuity: the connection window was actually the binding constraint — the in-flight total
    // reached within a frame of it (had `MaxData` not throttled, all 60 bytes would have been in flight
    // at once, far past the window).
    assert!(
      peak_in_flight >= window - FRAME_CAP as u64,
      "the connection window was never saturated ({peak_in_flight} < {window}), so the test proves nothing"
    );
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

  /// AC (§4.10a §8, RFC 9002 §7): congestion control is live end to end — acknowledgements grow the
  /// sender's window (slow start), and a detected loss reduces it, while the reliability core still
  /// delivers the whole stream. The window's rise (peak above the start) and its later fall (a strictly
  /// smaller value after a growth) are the non-vacuity witnesses that the controller actually gates the
  /// send, not a dead path.
  #[test]
  fn a_loss_reduces_the_congestion_window_while_delivery_still_completes() {
    let window = initial_receive_window(FRAME_CAP);
    // Many packets, so slow start ramps the window well above its start before the loss is detected.
    let content = stream_content(1, 30 * FRAME_CAP);
    let mut sender = Connection::new(window);
    sender.open(1, &content);
    let mut receiver = Connection::new(window);
    let start_window = sender.congestion_window();

    let mut received = Vec::new();
    let mut peak = start_window;
    let mut prev = start_window;
    let mut saw_reduction = false;
    // Drop mid-stream packet 3 once, so a later acknowledgement's gap declares it lost past the reorder
    // threshold; `None` on the acknowledgement path (no drops back).
    let mut drop_pn = Some(3u64);
    let mut guard = 0u64;
    loop {
      guard += 1;
      assert!(guard < 1_000_000, "the connection must make progress");
      let sent_any = pump_dropping(&mut sender, &mut receiver, &mut drop_pn);
      received.extend(receiver.read_stream(1));
      let acked_any = pump_dropping(&mut receiver, &mut sender, &mut None);
      let now = sender.congestion_window();
      peak = peak.max(now);
      saw_reduction |= now < prev;
      prev = now;
      if sender.send_complete() && receiver.recv_stream_complete(1) {
        break;
      }
      if !sent_any && !acked_any && !sender.probe() {
        break;
      }
    }

    assert_eq!(
      received, content,
      "the stream arrived exactly despite the loss"
    );
    assert!(sender.retransmitted() >= 1, "the loss-recovery path ran");
    assert!(
      peak > start_window,
      "acknowledgements grew the window (slow start)"
    );
    assert!(
      saw_reduction,
      "the detected loss reduced the congestion window"
    );
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
