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
  /// ACK-of-ACK bookkeeping (RFC 9000 §13.2.4): for each ack-eliciting packet this end sent that also
  /// carried an acknowledgement, the largest packet number that acknowledgement covered. When the peer
  /// acknowledges such a packet, the entry is consumed to bound the receive-side ack set. Entries are
  /// dropped on acknowledgement or loss, so the map stays within the in-flight ack-bearing packets.
  sent_acks: BTreeMap<u64, u64>,
  /// Received packets discarded as duplicates of a packet number already processed (RFC 9000 §12.3) —
  /// the non-vacuity counter proving the dedup path runs (a redelivered datagram never re-applies its
  /// frames).
  duplicates: u64,
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
      sent_acks: BTreeMap::new(),
      duplicates: 0,
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
    // The largest packet number this packet's acknowledgement covers, if it carries one — recorded for
    // ACK-of-ACK once the packet is known to be ack-eliciting (so the peer will acknowledge it back).
    let ack_largest = self.push_acknowledgement(&mut frames, max_frame_len);
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
      // ACK-of-ACK: this packet is ack-eliciting (it carries a stream frame), so the peer will
      // acknowledge it; if it also carries an acknowledgement of ours, remember what that covered, so
      // that when the peer acks this packet we can stop tracking the packets it acknowledged.
      if let Some(largest) = ack_largest {
        self.sent_acks.insert(pn, largest);
      }
      self.sent.on_sent(pn, vec![frame]);
    }
    Some((pn, frames))
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

  /// Takes a received packet: records its number for acknowledgement, demultiplexes each stream frame
  /// to its reassembler (creating one on first sight, and admitting the segment within the flow-control
  /// window the sender was never allowed to exceed), processes any acknowledgement, applies each
  /// stream's advertised send credit, and queues for retransmission whatever a new acknowledgement
  /// declares lost. An acknowledgement-only packet does not oblige an acknowledgement in return
  /// (RFC 9002 §2), which keeps two ends from trading acknowledgements forever.
  ///
  /// Returns the largest packet number this packet's acknowledgements *newly* freed, if any — the RTT
  /// sample point (RFC 9002 §5.1). The caller, which holds the clock, measures the round trip as `now`
  /// minus that packet's send time and folds it into its [`crate::rtt::RttEstimator`].
  pub fn handle_incoming(&mut self, pn: u64, frames: &[Frame]) -> Option<u64> {
    // Discard a packet whose number was already processed (RFC 9000 §12.3): its frames must not be
    // applied twice. A retransmission always rides a *fresh* number (a probe takes `next_pn`), so a
    // repeated number is never a re-request but a true duplicate the network delivered twice — the
    // shape that let a redelivered probe reply read as a live ack before this. Dropped whole: no stream
    // re-offer, no acknowledgement re-processed, no RTT sample, no loss pass, and no re-acknowledgement
    // owed (the peer holds our original ack — it did not retransmit, the network duplicated).
    if self.acks.is_duplicate(pn) {
      self.duplicates = self.duplicates.saturating_add(1);
      return None;
    }
    self.acks.record(pn);
    let mut ack_eliciting = false;
    let mut newly_acked_largest: Option<u64> = None;
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
          self.congestion.on_ack(acked.bytes);
          // The largest packet number this ACK newly frees is the RTT sample point (RFC 9002 §5.1);
          // fold it into the running maximum this call reports, for the caller's RTT estimator.
          newly_acked_largest = newly_acked_largest.max(acked.pns.iter().copied().max());
          // ACK-of-ACK (RFC 9000 §13.2.4): for each of our packets the peer just acknowledged that had
          // carried an acknowledgement of ours, the peer now has that acknowledgement — so we can stop
          // tracking the packets it covered, bounding the receive-side ack set.
          for pn in acked.pns {
            if let Some(covered) = self.sent_acks.remove(&pn) {
              self.acks.confirm(covered);
            }
          }
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
    self.detect_and_queue_losses();
    newly_acked_largest
  }

  /// After an acknowledgement is processed, runs loss detection and queues what it declares lost for
  /// retransmission: a gap past the reorder threshold declares its packets lost (their bytes leave the
  /// congestion window as a loss event, RFC 9002 §7.3.1), those packets' ACK-of-ACK entries are dropped
  /// (a lost packet is never acknowledged, so its entry would otherwise linger and its acknowledgement
  /// content is regenerated in the retransmission), and the ACK-of-ACK map is bounded to the recent
  /// unacknowledged window (an entry at or below the peer's largest acknowledged that survived the
  /// confirm and loss passes is a probed or abandoned packet the peer will never acknowledge).
  fn detect_and_queue_losses(&mut self) {
    let lost = self.sent.take_lost();
    if let Some(highest_pn) = lost.highest_pn {
      let lost_bytes: u64 = lost.frames.iter().map(tracked_bytes).sum();
      // The largest packet number sent so far bounds this congestion event's recovery period.
      let largest_sent = self.sent.peek_next_pn().saturating_sub(1);
      self
        .congestion
        .on_loss(lost_bytes, highest_pn, largest_sent);
    }
    for pn in &lost.pns {
      self.sent_acks.remove(pn);
    }
    if let Some(largest_acked) = self.sent.largest_acked() {
      self.sent_acks = self.sent_acks.split_off(&largest_acked);
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

  /// Allocates the next packet number and a bare, decryptable payload — a re-advertisement of the
  /// connection's current flow-control credit — for a **handshake-confirmation** packet
  /// ([`Endpoint::establish`](crate::Endpoint::establish)). TLS 1.3 leaves the server unable to know its
  /// final flight was received (the client finishes on *sending* it, the server on *receiving* it); QUIC
  /// closes that with a HANDSHAKE_DONE frame the peer can decrypt only once it holds the 1-RTT keys
  /// (RFC 9000 §19.20, RFC 9001 §4.1.2). This is the minimal equivalent over slates's dialect: a real
  /// packet, so it carries a fresh number from this connection's send space and is never confused with a
  /// raw handshake datagram, but built from a `MaxData` credit frame — idempotent (re-advertising the
  /// same ceiling changes nothing) and **not** ack-eliciting (RFC 9002 §2), so decoding it obliges no
  /// acknowledgement and two ends never trade confirmations forever. It is not tracked for
  /// retransmission: the endpoint resends a confirmation with a *fresh* number each probe timeout, so no
  /// number is ever reused under the packet keys (RFC 9001 §9.5).
  pub fn emit_confirm(&mut self) -> (u64, Vec<Frame>) {
    let pn = self.sent.next_pn();
    (pn, vec![self.flow.connection_credit_frame()])
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

  /// How many packet numbers the receive side still tracks for acknowledgement — the handle a test uses
  /// to prove ACK-of-ACK keeps this set bounded on a long-lived or reused connection (Banned #8).
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

  /// Ack-eliciting packets sent and not yet acknowledged: what a probe timeout would retransmit. Zero on
  /// an idle connection, which therefore needs no probe timer (RFC 9002 §6.2.1 arms the PTO only while
  /// such packets are in flight).
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
    // Nothing of the stream is retransmitted after it is forgotten: not from the queue of frames already
    // declared lost, and not from a packet still in flight that a later loss or probe would resend —
    // otherwise an exchange abandoned at its deadline (a fleet probe) keeps re-asking the peer with the
    // stale request, and the peer's answers to it shadow every later exchange on the id.
    self
      .retransmit
      .retain(|frame| !matches!(frame, Frame::Stream { stream_id: id, .. } if *id == stream_id));
    let dropped = self.sent.forget_stream(stream_id);
    self.congestion.on_probe_removed(dropped);
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

  /// How many received packets were discarded as duplicates of an already-processed packet number (RFC
  /// 9000 §12.3) — the non-vacuity counter proving the dedup path runs, so a redelivered datagram's
  /// frames are provably never applied twice.
  pub fn duplicates_discarded(&self) -> u64 {
    self.duplicates
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

  /// Like [`pump`], but delivers each batch of packets `from` emits in *reverse* order — a deterministic
  /// reordering of the datagram path (the design's "lossy, reordering" path). Out-of-order arrival
  /// exercises the receiver's reassembly buffer, multi-range ACK formation across the gaps reorder
  /// opens, and the loss detector: a packet far enough behind the largest acknowledged is declared lost
  /// and retransmitted (RFC 9002 §6.1.1), and the assembler dedups it, so delivery is still exactly-once.
  fn pump_reordering(from: &mut Connection, to: &mut Connection, channel: &mut Channel) -> bool {
    let mut batch = Vec::new();
    while let Some(packet) = from.poll_transmit(FRAME_CAP) {
      batch.push(packet);
    }
    let sent_any = !batch.is_empty();
    for (pn, frames) in batch.into_iter().rev() {
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

  /// Sends one request on stream 1 and shows the probe path live: unacknowledged, a probe resends it
  /// (the retransmit counter moves). Returns the sender with the request still in flight.
  fn request_in_flight_and_probed() -> Connection {
    let mut sender = Connection::new(initial_receive_window(FRAME_CAP));
    sender.open(1, &stream_content(9, 40));
    let (_pn, frames) = sender
      .poll_transmit(FRAME_CAP)
      .expect("the request goes out");
    assert!(
      frames
        .iter()
        .any(|f| matches!(f, Frame::Stream { stream_id: 1, .. }))
    );
    assert_eq!(sender.in_flight_count(), 1);
    assert!(sender.probe(), "a probe finds the packet in flight");
    let (_pn, resent) = sender
      .poll_transmit(FRAME_CAP)
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
  /// forgotten, none of its data is ever retransmitted — not from a packet still in flight that a probe
  /// would resend, and not from the lost-frame queue — and its bytes leave the in-flight accounting. Non-
  /// vacuous: before the forget, the same probe resends the packet (`request_in_flight_and_probed`), so
  /// the path that would have re-asked with the stale request is shown live and then shown closed.
  #[test]
  fn a_forgotten_streams_frames_are_never_retransmitted() {
    let mut sender = request_in_flight_and_probed();
    let retransmitted_before = sender.retransmitted();
    // The exchange is abandoned: forgotten. Nothing of it may go out again.
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
      sender.poll_transmit(FRAME_CAP).is_none(),
      "no frame of the forgotten stream is retransmitted"
    );
    assert_eq!(sender.retransmitted(), retransmitted_before);
  }

  /// AC (§4.10a §8, RFC 9000 §12.3): a packet whose number was already processed is discarded — its
  /// frames are not applied again and it owes no fresh acknowledgement. Retransmissions ride fresh
  /// numbers, so a repeated number is always a network duplicate (the redelivered probe reply that used
  /// to read as a live ack was exactly this). Non-vacuous: the duplicate counter moves, and the receiver
  /// that owed an acknowledgement after the first receipt owes none after the duplicate.
  #[test]
  fn a_duplicate_packet_number_is_discarded_not_processed_again() {
    let window = initial_receive_window(FRAME_CAP);
    let mut sender = Connection::new(window);
    let mut receiver = Connection::new(window);
    sender.open(7, &stream_content(0xAB, 40));
    let (pn, frames) = sender
      .poll_transmit(FRAME_CAP)
      .expect("the request goes out");

    // First receipt: this packet's stream bytes are delivered and an acknowledgement becomes owed.
    assert_eq!(receiver.handle_incoming(pn, &frames), None);
    assert!(
      !receiver.read_stream(7).is_empty(),
      "the first receipt delivered the packet's stream bytes"
    );
    assert_eq!(receiver.duplicates_discarded(), 0);
    assert!(
      receiver.poll_transmit(FRAME_CAP).is_some(),
      "the first receipt owes an acknowledgement"
    );

    // The very same packet number arrives again (a network duplicate): discarded and counted, it
    // delivers no further bytes and owes no new acknowledgement — it created no work at all.
    assert_eq!(receiver.handle_incoming(pn, &frames), None);
    assert_eq!(receiver.duplicates_discarded(), 1);
    assert!(
      receiver.read_stream(7).is_empty(),
      "the duplicate delivered no further bytes"
    );
    assert!(
      receiver.poll_transmit(FRAME_CAP).is_none(),
      "the duplicate owed no fresh acknowledgement"
    );
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

  /// AC (§4.10a §8, RFC 9000 §13.2.4 ACK-of-ACK): on a connection reused across many exchanges the
  /// receive-side acknowledgement set stays bounded — once the peer acknowledges one of our
  /// acknowledgement-bearing packets, the packets it covered are dropped — rather than growing without
  /// bound (Banned #8). Runs many bidirectional exchanges over one connection pair and asserts the
  /// tracked set never grows past a small bound while every exchange still delivers exactly.
  #[test]
  fn ack_of_ack_bounds_the_receive_set_over_a_reused_connection() {
    /// Shape: enough exchanges that an unpruned set (≈ exchanges × packets-each-way) would dwarf the
    /// bound below; with pruning it stays near one exchange's worth.
    const EXCHANGES: u64 = 50;
    /// Shape: a few packets each way per exchange (fits one window, so no flow stall).
    const BODY: usize = 24;

    let window = initial_receive_window(FRAME_CAP);
    let mut a = Connection::new(window);
    let mut b = Connection::new(window);
    let mut channel = Channel::new(Vec::new());
    let mut peak_tracked = 0usize;

    for i in 0..EXCHANGES {
      let sid = i * 2 + 1; // a fresh stream id per exchange
      a.open(sid, &stream_content(u8::try_from(i % 7).unwrap_or(0), BODY));
      b.open(
        sid,
        &stream_content(u8::try_from(i % 5).unwrap_or(0).wrapping_add(100), BODY),
      );
      let mut guard = 0u64;
      loop {
        guard += 1;
        assert!(guard < 100_000, "the exchange must make progress");
        // One packet each way per step, draining the receiver before the reverse send. Interleaving one
        // at a time is what makes each data packet also carry the pending acknowledgement (rather than a
        // separate pure-ACK packet the peer never acknowledges), so ACK-of-ACK actually confirms — the
        // realistic behaviour a fully-drained pump would hide.
        let a_sent = a.poll_transmit(FRAME_CAP);
        if let Some((pn, frames)) = &a_sent
          && !channel.drops()
        {
          b.handle_incoming(*pn, frames);
        }
        let _ = b.read_stream(sid);
        let b_sent = b.poll_transmit(FRAME_CAP);
        if let Some((pn, frames)) = &b_sent
          && !channel.drops()
        {
          a.handle_incoming(*pn, frames);
        }
        let _ = a.read_stream(sid);
        peak_tracked = peak_tracked.max(a.acks_tracked()).max(b.acks_tracked());
        let delivered = a.recv_stream_complete(sid) && b.recv_stream_complete(sid);
        if delivered && a.send_complete() && b.send_complete() {
          break;
        }
        if a_sent.is_none() && b_sent.is_none() {
          a.probe();
          b.probe();
        }
      }
      a.forget_stream(sid);
      b.forget_stream(sid);
    }

    // The bound (with non-vacuity): many exchanges ran — an unpruned set would be on the order of
    // EXCHANGES × (BODY / FRAME_CAP) ≈ 150 — yet ACK-of-ACK held the tracked set within a couple of
    // exchanges' worth. A generous ceiling well below the unpruned size proves the pruning is real.
    assert!(peak_tracked > 0, "exchanges actually ran and were tracked");
    assert!(
      peak_tracked < 40,
      "ACK-of-ACK kept the receive set bounded across {EXCHANGES} exchanges (peak {peak_tracked})"
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

  /// `handle_incoming` reports the largest packet number an acknowledgement *newly* frees — the RTT
  /// sample point (RFC 9002 §5.1) the endpoint measures the round trip from — and reports `None` for a
  /// packet that frees nothing (an acknowledgement-only packet carries no data to acknowledge back).
  #[test]
  fn handle_incoming_reports_the_rtt_sample_point() {
    let window = initial_receive_window(FRAME_CAP);
    let mut a = Connection::new(window);
    let mut b = Connection::new(window);
    a.open(1, &stream_content(1, 24));
    // `a` sends its data packets (all ack-eliciting) to `b`; none carry an acknowledgement yet, so `b`
    // reports no sample point for them.
    let mut a_pns = Vec::new();
    while let Some((pn, frames)) = a.poll_transmit(FRAME_CAP) {
      a_pns.push(pn);
      assert_eq!(
        b.handle_incoming(pn, &frames),
        None,
        "a's data packets acknowledge nothing back"
      );
    }
    let _ = b.read_stream(1);
    // `b`'s acknowledgement frees a's data packets; `a` reports the largest as the sample point.
    let mut sample_point = None;
    while let Some((pn, frames)) = b.poll_transmit(FRAME_CAP) {
      if let Some(largest) = a.handle_incoming(pn, &frames) {
        sample_point = Some(largest);
      }
    }
    assert_eq!(
      sample_point,
      a_pns.iter().copied().max(),
      "the sample point is a's largest acknowledged packet"
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

    /// The reordering oracle (R5, the design's "lossy, reordering datagram path"): for any streams, any
    /// loss, AND out-of-order delivery, the receiver still reassembles each stream exactly. The forward
    /// direction delivers each batch reversed, so packets arrive out of order — stressing the reassembly
    /// buffer, multi-range ACK formation across the gaps reorder opens, and loss-detection-then-dedup
    /// when reorder pushes a packet past the reorder threshold. Every byte still arrives, once.
    #[test]
    fn any_streams_survive_loss_and_reorder(
      lens in prop::collection::vec(0usize..200, 1..4),
      drops in prop::collection::vec(0u64..120, 0..20),
    ) {
      let streams: Vec<(u64, Vec<u8>)> = lens
        .iter()
        .enumerate()
        .map(|(i, &len)| (u64::try_from(i * 2 + 1).unwrap_or(1), stream_content(u8::try_from(i).unwrap_or(0), len)))
        .collect();
      let window = initial_receive_window(FRAME_CAP);
      let mut sender = Connection::new(window);
      for (id, content) in &streams {
        sender.open(*id, content);
      }
      let mut receiver = Connection::new(window);
      let mut channel = Channel::new(drops);
      let mut received: BTreeMap<u64, Vec<u8>> = BTreeMap::new();

      let mut guard = 0u64;
      loop {
        guard += 1;
        prop_assert!(guard < 1_000_000, "the connection must make progress");
        // Forward data is reordered; the reverse (acknowledgement) direction stays in order.
        let sent = pump_reordering(&mut sender, &mut receiver, &mut channel);
        for id in receiver.recv_stream_ids() {
          received.entry(id).or_default().extend(receiver.read_stream(id));
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
        prop_assert_eq!(received.get(id), Some(content));
      }
    }
  }
}
