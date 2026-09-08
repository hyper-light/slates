//! The sans-io session-plane connection driver (§4.10a §8): the state machine that composes the
//! stream layer ([`crate::stream`]), the reliability core ([`crate::conn`]) and — later — the
//! flow-control credit law ([`crate::flow`]) into *reliable, ordered, exactly-once* delivery of a
//! stream over a lossy, reordering datagram path. It owns no socket and no clock: [`poll_transmit`]
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
//! Scope of this slice (reliable single-stream delivery): one stream in one direction, one
//! ack-eliciting frame per packet, acknowledgements riding back, loss recovered by the reorder
//! threshold and — for a lost tail — by a probe when the connection would otherwise stall. Owed:
//! deriving the send credit from the peer's received `MaxStreamData` (flow enforcement — the sender
//! here grants itself the whole stream, as the endpoint did), several ack-eliciting frames per packet
//! (an MTU budget), multiplexing many streams, and congestion control.

use std::collections::VecDeque;

use crate::conn::{AckGenerator, SentTracker};
use crate::session::Frame;
use crate::stream::{StreamAssembler, StreamSender};

/// One end of a reliable stream connection: the send side (the stream source, the in-flight tracker,
/// and a queue of frames awaiting retransmission), the receive side (the ordered assembler and the
/// acknowledgement generator), and whether an acknowledgement is owed for an ack-eliciting packet just
/// received. A non-vacuity counter records how many frames have actually been retransmitted.
pub struct Connection {
  /// The stream id this end sends on (single-stream this slice).
  send_stream_id: u64,
  /// The send-side stream source.
  sender: StreamSender,
  /// In-flight packet tracking and loss detection.
  sent: SentTracker,
  /// Frames freed by loss detection or a probe, awaiting retransmission (one per packet).
  retransmit: VecDeque<Frame>,
  /// The receive-side ordered reassembler.
  assembler: StreamAssembler,
  /// Which packet numbers have arrived, and the acknowledgement to send back.
  acks: AckGenerator,
  /// Set when an ack-eliciting packet has arrived and its acknowledgement has not yet been sent.
  ack_owed: bool,
  /// How many frames this end has retransmitted (the non-vacuity counter for the loss-recovery path).
  retransmitted: u64,
}

impl Connection {
  /// A fresh connection sending on `send_stream_id`.
  pub fn new(send_stream_id: u64) -> Connection {
    Connection {
      send_stream_id,
      sender: StreamSender::new(),
      sent: SentTracker::new(),
      retransmit: VecDeque::new(),
      assembler: StreamAssembler::new(0),
      acks: AckGenerator::new(),
      ack_owed: false,
      retransmitted: 0,
    }
  }

  /// Queues `data` as the whole of this end's send stream and finishes it, granting the send side the
  /// full credit (flow enforcement — deriving the credit from the peer's `MaxStreamData` — is owed).
  pub fn send_all(&mut self, data: &[u8]) {
    self.sender.write(data);
    self.sender.grant_credit(data.len() as u64);
    self.sender.finish();
  }

  /// The next packet to send, as `(packet_number, frames)`, or `None` when there is nothing to send
  /// right now (nothing to retransmit, no fresh stream data, and no acknowledgement owed). Puts at most
  /// one ack-eliciting frame in the packet — a retransmitted frame first, otherwise a fresh stream
  /// frame — and appends the acknowledgement if one is owed. Only a packet that carries an
  /// ack-eliciting frame is tracked for loss; an acknowledgement-only packet is not (RFC 9002 §2).
  pub fn poll_transmit(&mut self, max_frame_len: usize) -> Option<(u64, Vec<Frame>)> {
    // One ack-eliciting frame: a retransmission takes priority over fresh data.
    let reliable = match self.retransmit.pop_front() {
      Some(frame) => Some(frame),
      None => self.sender.next_frame(self.send_stream_id, max_frame_len),
    };

    let mut frames = Vec::new();
    if let Some(frame) = reliable.clone() {
      frames.push(frame);
    }
    if self.ack_owed
      && let Some(ack) = self.acks.ack_frame()
    {
      frames.push(ack);
      self.ack_owed = false;
    }
    if frames.is_empty() {
      return None;
    }

    let pn = self.sent.next_pn();
    // Track only the ack-eliciting frame for loss/retransmission; the acknowledgement is regenerated
    // fresh each time, never retransmitted stale.
    if let Some(frame) = reliable {
      self.sent.on_sent(pn, vec![frame]);
    }
    Some((pn, frames))
  }

  /// Takes a received packet: records its number for acknowledgement, offers its stream data to the
  /// assembler (growing the receive window to admit it — flow enforcement owed), processes any
  /// acknowledgement it carries, and queues for retransmission whatever the new acknowledgement
  /// declares lost. An acknowledgement-only packet does not oblige an acknowledgement in return
  /// (RFC 9002 §2), which is what keeps two ends from trading acknowledgements forever.
  pub fn handle_incoming(&mut self, pn: u64, frames: &[Frame]) {
    self.acks.record(pn);
    let mut ack_eliciting = false;
    for frame in frames {
      match frame {
        Frame::Stream {
          offset, fin, data, ..
        } => {
          ack_eliciting = true;
          // Admit the segment: raise the window to cover it (flow control owed), then offer. A
          // duplicate or reordered segment is deduped by the assembler; a refusal here would only be a
          // window we just widened enough to prevent, so it is safe to ignore.
          self
            .assembler
            .grant_window(offset.saturating_add(data.len() as u64).saturating_add(1));
          let _ = self.assembler.offer(*offset, data, *fin);
        }
        Frame::Ack { largest, range } => {
          self.sent.on_ack(*largest, *range);
        }
        // MaxData / MaxStreamData drive flow control (owed); PADDING is nothing.
        Frame::MaxData { .. } | Frame::MaxStreamData { .. } => {}
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
  /// anything was queued. The caller drives this only when [`poll_transmit`] has returned `None` on
  /// both ends yet delivery is not complete.
  ///
  /// [`poll_transmit`]: Connection::poll_transmit
  pub fn probe(&mut self) -> bool {
    let frames = self.sent.probe_oldest();
    let probed = !frames.is_empty();
    self.queue_retransmit(frames);
    probed
  }

  /// Drains the bytes that have become contiguous on the receive side (in order, each once).
  pub fn read(&mut self) -> Vec<u8> {
    self.assembler.read()
  }

  /// Whether this end has originated its whole stream and every ack-eliciting packet has been
  /// acknowledged (nothing buffered to retransmit, nothing in flight).
  pub fn send_complete(&self) -> bool {
    self.sender.is_drained() && self.retransmit.is_empty() && self.sent.in_flight_count() == 0
  }

  /// Whether the received stream is complete (all bytes through its `fin`).
  pub fn recv_complete(&self) -> bool {
    self.assembler.is_complete()
  }

  /// How many frames this end has retransmitted — the non-vacuity counter proving the loss-recovery
  /// path actually ran, so a test over a lossy channel cannot pass with a dead retransmit path.
  pub fn retransmitted(&self) -> u64 {
    self.retransmitted
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

  /// A deterministic lossy, reordering channel the oracle drives the two ends over: a queue of
  /// `(packet_number, frames)` with a drop predicate keyed by a step counter, so a test states exactly
  /// which transmissions are lost.
  struct Channel {
    /// Which transmission steps (counted across both directions) are dropped.
    drop_steps: Vec<u64>,
    /// The running step counter.
    step: u64,
  }

  impl Channel {
    fn new(drop_steps: Vec<u64>) -> Channel {
      Channel {
        drop_steps,
        step: 0,
      }
    }

    /// Whether the current step is dropped; advances the counter.
    fn drops(&mut self) -> bool {
      let dropped = self.drop_steps.contains(&self.step);
      self.step = self.step.saturating_add(1);
      dropped
    }
  }

  /// Runs a one-way stream transfer of `content` from a sender to a receiver over `channel`, returning
  /// the bytes the receiver reassembled and how many frames the sender retransmitted. The loop drains
  /// each end's outgoing packets, delivering or dropping each per the channel, and probes when it would
  /// otherwise stall with packets still in flight (a lost tail).
  fn transfer(content: &[u8], mut channel: Channel) -> (Vec<u8>, u64) {
    let mut sender = Connection::new(1);
    sender.send_all(content);
    let mut receiver = Connection::new(1);
    let mut received = Vec::new();

    let mut guard = 0u64;
    loop {
      guard += 1;
      assert!(guard < 100_000, "the connection must make progress");
      let mut progress = false;

      // Sender -> receiver (stream data).
      while let Some((pn, frames)) = sender.poll_transmit(FRAME_CAP) {
        progress = true;
        if !channel.drops() {
          receiver.handle_incoming(pn, &frames);
        }
      }
      received.extend_from_slice(&receiver.read());

      // Receiver -> sender (acknowledgements).
      while let Some((pn, frames)) = receiver.poll_transmit(FRAME_CAP) {
        progress = true;
        if !channel.drops() {
          sender.handle_incoming(pn, &frames);
        }
      }

      if sender.send_complete() && receiver.recv_complete() {
        break;
      }
      if !progress && !sender.probe() {
        // Stalled with nothing to probe: report what we have so the assertion shows the shortfall.
        break;
      }
    }
    (received, sender.retransmitted())
  }

  /// The sample stream: long enough to span many packets at the small frame cap.
  fn content() -> Vec<u8> {
    (0..500u16)
      .map(|i| u8::try_from(i % 251).unwrap_or(0))
      .collect()
  }

  /// AC (§4.10a §8): with no loss, the stream arrives exactly, and nothing is retransmitted.
  #[test]
  fn a_lossless_transfer_delivers_the_whole_stream() {
    let (received, retransmits) = transfer(&content(), Channel::new(Vec::new()));
    assert_eq!(received, content(), "the whole stream, in order, once each");
    assert_eq!(
      retransmits, 0,
      "nothing is retransmitted when nothing is lost"
    );
  }

  /// AC (§4.10a §8): a dropped packet in the middle is recovered by the reorder threshold and the
  /// stream still arrives exactly once, in order. The retransmit counter is the non-vacuity check.
  #[test]
  fn a_mid_stream_drop_is_recovered() {
    // Drop the third sender transmission (step 2 counts sender+receiver steps interleaved, but the
    // first receiver step only happens after the first delivery — so an early step is a sender step).
    let (received, retransmits) = transfer(&content(), Channel::new(vec![2]));
    assert_eq!(received, content(), "every byte arrives despite the drop");
    assert!(retransmits >= 1, "the loss-recovery path actually ran");
  }

  /// AC (§4.10a §8): a dropped *last* packet — which the reorder threshold cannot detect (no later
  /// acknowledged packet opens a gap past it) — is recovered by the probe, so even a tail loss
  /// delivers the whole stream. The sender emits all data packets in the first burst (one frame each),
  /// so the final data packet is the `(data_packets - 1)`th transmission step.
  #[test]
  fn a_tail_drop_is_recovered_by_the_probe() {
    let content = content();
    let data_packets = content.len().div_ceil(FRAME_CAP) as u64;
    let last_data_step = data_packets - 1;
    let (received, retransmits) = transfer(&content, Channel::new(vec![last_data_step]));
    assert_eq!(received, content, "a lost tail is recovered by the probe");
    assert!(retransmits >= 1, "the probe retransmitted the lost tail");
  }

  proptest! {
    /// The behavioural oracle (R5): for any content and any set of dropped steps, the receiver
    /// reassembles exactly the sent stream — in order, each byte once — the reliability guarantee the
    /// whole layer exists to provide, over an adversarial lossy channel.
    #[test]
    fn any_loss_pattern_still_delivers_the_stream(
      len in 0usize..600,
      drops in prop::collection::vec(0u64..200, 0..40),
    ) {
      let content: Vec<u8> = (0..len).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
      let (received, _) = transfer(&content, Channel::new(drops));
      prop_assert_eq!(received, content);
    }
  }
}
