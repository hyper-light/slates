//! The flow-control credit law (§4.10a §8, slice 4d — hecate's ratified dual-level credit law, in
//! the transport). The receiver advertises **absolute-offset** credit — a ceiling the sender may
//! send up to — kept a bounded window *ahead of what the application has consumed*, per stream and
//! for the connection. As the app reads, the ceiling rises and the receiver sends `MaxStreamData` /
//! `MaxData`; the sender ([`crate::stream::StreamSender::grant_credit`]) frames only within it. The
//! window stays `window_ahead` beyond consumed, so a bulk stream **never has the whole object in
//! credit** (the permanent invariant): the sender can never race more than one window ahead of the
//! reader, so its buffer and the receiver's are both bounded and a bulk transfer never starves a
//! control frame.
//!
//! `window_ahead` is derived by the caller (the design's `k × frame_cap`, BDP-autotuned — owed), not
//! a constant here. Absolute offsets make a credit update idempotent under loss/reorder (a lower
//! grant is ignored). This is the pure accounting; the connection loop that ships the credit frames
//! and applies received ones is where it plugs in.

use std::collections::BTreeMap;

use crate::session::Frame;

/// The receive-side flow-control accounting: how far the application has consumed, per stream and for
/// the connection, and the credit ceiling to advertise (consumed + a bounded window ahead).
#[derive(Debug)]
pub struct FlowController {
  window_ahead: u64,
  connection_consumed: u64,
  stream_consumed: BTreeMap<u64, u64>,
}

impl FlowController {
  /// A controller advertising `window_ahead` bytes of credit beyond what the app has consumed.
  pub fn new(window_ahead: u64) -> FlowController {
    FlowController {
      window_ahead,
      connection_consumed: 0,
      stream_consumed: BTreeMap::new(),
    }
  }

  /// Forgets a completed stream's per-stream credit watermark, so a later stream that reuses the id
  /// starts its flow control fresh (its offsets begin at zero, below the finished stream's watermark).
  /// The connection-wide consumed total is a separate running sum and is **kept** — the forgotten
  /// stream's bytes were already counted and must not be un-counted, or the peer's connection credit
  /// would regress. Without this, reusing a stream id would leave a stale watermark that both
  /// over-credits the reused stream and (via the connection total's delta) stalls it.
  pub fn forget_stream(&mut self, stream_id: u64) {
    self.stream_consumed.remove(&stream_id);
  }

  /// Records that the application has consumed up to absolute offset `consumed` on `stream_id` (the
  /// stream's read cursor, monotonic), advancing the connection-wide consumed total by the same amount.
  /// The connection total is held as a running *sum* of every stream's advance — never recomputed from
  /// the live streams — so a completed stream that the connection later forgets does not un-count its
  /// bytes (the sender's connection credit must only rise, never regress).
  pub fn on_stream_consumed(&mut self, stream_id: u64, consumed: u64) {
    let entry = self.stream_consumed.entry(stream_id).or_insert(0);
    let advanced = consumed.saturating_sub(*entry); // 0 for a reordered or duplicate lower report
    *entry = (*entry).max(consumed);
    self.connection_consumed = self.connection_consumed.saturating_add(advanced);
  }

  /// The stream's credit ceiling to advertise: consumed + the window. Never the whole object — it
  /// stays exactly `window_ahead` beyond what has been read.
  pub fn stream_max(&self, stream_id: u64) -> u64 {
    self.stream_consumed.get(&stream_id).copied().unwrap_or(0) + self.window_ahead
  }

  /// The connection's credit ceiling to advertise.
  pub fn connection_max(&self) -> u64 {
    self.connection_consumed + self.window_ahead
  }

  /// The `MaxStreamData` frame advertising this stream's current credit ceiling.
  pub fn stream_credit_frame(&self, stream_id: u64) -> Frame {
    Frame::MaxStreamData {
      stream_id,
      max: self.stream_max(stream_id),
    }
  }

  /// The `MaxData` frame advertising the connection's current credit ceiling.
  pub fn connection_credit_frame(&self) -> Frame {
    Frame::MaxData {
      max: self.connection_max(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::stream::{StreamAssembler, StreamSender};

  /// Applies the receiver's advertised stream credit to both its own window and the sender's credit.
  fn advertise(
    flow: &FlowController,
    stream_id: u64,
    assembler: &mut StreamAssembler,
    source: &mut StreamSender,
  ) {
    if let Frame::MaxStreamData { max, .. } = flow.stream_credit_frame(stream_id) {
      assembler.grant_window(max);
      source.grant_credit(max);
    }
  }

  /// The credit ceiling stays a fixed window ahead of what is consumed, never the whole object.
  #[test]
  fn credit_stays_a_window_ahead_of_consumed() {
    let mut flow = FlowController::new(40);
    assert_eq!(flow.stream_max(1), 40, "initial credit is one window");
    flow.on_stream_consumed(1, 100);
    assert_eq!(flow.stream_max(1), 140, "credit tracks consumed + window");
    flow.on_stream_consumed(1, 50); // a reordered lower report is ignored (monotonic)
    assert_eq!(flow.stream_max(1), 140);
  }

  /// The connection-wide credit tracks the *sum* of every stream's consumption and only rises. Held as
  /// a running sum (not recomputed from live streams), so a finished stream the connection later forgets
  /// never un-counts its bytes — the credit the sender relies on must not regress.
  #[test]
  fn connection_credit_sums_streams_and_never_regresses() {
    let mut flow = FlowController::new(40);
    assert_eq!(
      flow.connection_max(),
      40,
      "initial connection credit is one window"
    );
    flow.on_stream_consumed(1, 100);
    flow.on_stream_consumed(2, 50);
    assert_eq!(
      flow.connection_max(),
      190,
      "the sum of both cursors (150) + the window (40)"
    );
    flow.on_stream_consumed(1, 30); // a reordered lower report adds nothing
    assert_eq!(
      flow.connection_max(),
      190,
      "a lower report does not move the total"
    );
    flow.on_stream_consumed(1, 130); // advances stream 1 by 30 from its high-water 100
    assert_eq!(
      flow.connection_max(),
      220,
      "only the 30-byte advance is added (180 + 40)"
    );
  }

  /// Forgetting a completed stream resets its per-stream watermark (so a later stream reusing the id
  /// starts fresh) while keeping the connection-wide total (its bytes stay counted). Without the reset,
  /// a reused id's lower offsets would sit below the stale watermark and never advance the total —
  /// stalling the connection-level credit ratchet (the bug the cluster's connection-reuse test caught).
  #[test]
  fn forgetting_a_stream_resets_its_watermark_but_keeps_the_connection_total() {
    let mut flow = FlowController::new(40);
    flow.on_stream_consumed(1, 100);
    assert_eq!(
      flow.stream_max(1),
      140,
      "the stream's credit reflects its 100 consumed"
    );
    assert_eq!(
      flow.connection_max(),
      140,
      "the connection total is 100 + window"
    );
    flow.forget_stream(1);
    assert_eq!(
      flow.stream_max(1),
      40,
      "a reused id starts fresh (the watermark is gone)"
    );
    assert_eq!(
      flow.connection_max(),
      140,
      "the connection total is kept (the 100 is still counted)"
    );
    // Reusing id 1: its offsets start at zero and advance the connection total from where it stood, with
    // no stale watermark to block them.
    flow.on_stream_consumed(1, 30);
    assert_eq!(
      flow.connection_max(),
      170,
      "the reused stream's 30 bytes advance the total (130 + 40)"
    );
  }

  /// The whole flow-control loop: a large stream flows through a window far smaller than the object,
  /// the sender never racing more than a window ahead of the reader (never-whole-object), and every
  /// byte still arrives. Do X, expect Y.
  #[test]
  fn a_stream_flows_through_a_bounded_window() {
    const STREAM_ID: u64 = 1;
    const FRAME_CAP: usize = 8;
    const WINDOW_AHEAD: u64 = 40;
    let content: Vec<u8> = (0..200u16)
      .map(|i| u8::try_from(i % 251).unwrap_or(0))
      .collect();
    assert!(
      WINDOW_AHEAD < content.len() as u64,
      "the window is smaller than the object"
    );

    let mut source = StreamSender::new();
    source.write(&content);
    source.finish();
    let flow = FlowController::new(WINDOW_AHEAD);
    let mut assembler = StreamAssembler::new(flow.stream_max(STREAM_ID));
    source.grant_credit(flow.stream_max(STREAM_ID));

    let mut flow = flow;
    let mut received = Vec::new();
    let mut guard = 0;
    while !assembler.is_complete() {
      guard += 1;
      assert!(guard < 10_000, "the loop must make progress");
      // The sender frames everything its current credit allows.
      while let Some(Frame::Stream {
        offset, fin, data, ..
      }) = source.next_frame(STREAM_ID, FRAME_CAP)
      {
        assembler.offer(offset, &data, fin).unwrap();
      }
      // Never-whole-object: the sender is at most one window ahead of the reader.
      assert!(
        source.send_offset() <= assembler.read_offset() + WINDOW_AHEAD,
        "the sender raced more than a window ahead of the reader"
      );
      // The application consumes, and the receiver advertises fresh credit.
      received.extend_from_slice(&assembler.read());
      flow.on_stream_consumed(STREAM_ID, assembler.read_offset());
      advertise(&flow, STREAM_ID, &mut assembler, &mut source);
    }

    assert_eq!(
      received, content,
      "the whole stream flowed through the bounded window"
    );
  }
}
