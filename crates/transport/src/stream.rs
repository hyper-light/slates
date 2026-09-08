//! Ordered stream reassembly (§4.10a §8, slice 4b — the ordered-log archetype). A stream's `Stream`
//! frames arrive at **absolute** offsets, possibly reordered, duplicated, or overlapping (a
//! retransmit), and this buffers them under a bounded receive window and yields the stream's bytes
//! **contiguously, in order, once each** — the "strictly ordered, no gaps" delivery the design names.
//!
//! Absolute offsets make it idempotent under loss and reorder (D-15's flow-control clause): a frame
//! already covered is dropped, a gap is buffered until filled, and `read` drains only the prefix that
//! is contiguous from the read cursor. The window bounds the buffered bytes — an offer past it is a
//! typed refusal, never unbounded growth (the never-whole-object-in-credit invariant lives here). The
//! credit accounting that *sets* the window from the peer's `MaxStreamData`, and the connection state
//! machine that feeds frames in, are the owed sub-slices; this is the pure receive buffer they use.

use std::collections::BTreeMap;

use crate::session::Frame;

/// The send side of one ordered stream (§4.10a §8): buffered application bytes framed into
/// `Stream` frames **only within the flow-control credit** the peer grants (its `MaxStreamData`),
/// never more than a frame cap per frame. This is where the **never-whole-object-in-credit**
/// invariant is enforced on send: a 2 GB write with a small credit emits only credit-worth of
/// frames, the rest waiting for more credit, so a bulk stream never claims the whole object's worth
/// of buffer and never head-of-line-blocks a control frame. Retransmission (re-framing acked-then-
/// lost ranges) and buffer trimming on ack are owed with the connection's loss recovery; this slice
/// frames each byte once, which is exactly right over the lossless sim and the base for loss recovery.
#[derive(Debug, Default)]
pub struct StreamSender {
  buffered: Vec<u8>,
  send_offset: u64,
  credit: u64,
  finished: bool,
  fin_framed: bool,
}

impl StreamSender {
  /// A fresh sender with no credit (nothing sends until the peer grants some).
  pub fn new() -> StreamSender {
    StreamSender::default()
  }

  /// Buffers application bytes to send, in order.
  pub fn write(&mut self, data: &[u8]) {
    self.buffered.extend_from_slice(data);
  }

  /// Raises the send credit to the peer's absolute `MaxStreamData` ceiling (monotonic; a lower grant
  /// is ignored, so a reordered credit frame never shrinks the window).
  pub fn grant_credit(&mut self, max: u64) {
    self.credit = self.credit.max(max);
  }

  /// Marks the stream finished; the frame that carries its last byte (or an empty frame at the end)
  /// will set `fin`.
  pub fn finish(&mut self) {
    self.finished = true;
  }

  /// The next `Stream` frame to send on `stream_id`, at most `max_frame_len` bytes, bounded by the
  /// credit — or `None` when nothing is sendable yet (no bytes within credit, and no `fin` to mark).
  pub fn next_frame(&mut self, stream_id: u64, max_frame_len: usize) -> Option<Frame> {
    let sendable_end = (self.buffered.len() as u64).min(self.credit);
    if self.send_offset >= sendable_end {
      // No data within credit. Emit the terminating empty fin frame once, when every buffered byte
      // has been framed and the credit covers the final offset.
      if self.finished
        && !self.fin_framed
        && self.send_offset == self.buffered.len() as u64
        && self.send_offset <= self.credit
      {
        self.fin_framed = true;
        return Some(Frame::Stream {
          stream_id,
          offset: self.send_offset,
          fin: true,
          data: Vec::new(),
        });
      }
      return None;
    }
    let start = self.send_offset;
    let want = (sendable_end - start).min(max_frame_len as u64);
    let lo = usize::try_from(start)
      .unwrap_or(usize::MAX)
      .min(self.buffered.len());
    let hi = lo
      .saturating_add(usize::try_from(want).unwrap_or(0))
      .min(self.buffered.len());
    let data = self.buffered[lo..hi].to_vec();
    self.send_offset = start + (hi - lo) as u64;
    // The fin rides the frame that carries the final byte (when the credit reaches the end).
    let fin = self.finished && self.send_offset == self.buffered.len() as u64;
    if fin {
      self.fin_framed = true;
    }
    Some(Frame::Stream {
      stream_id,
      offset: start,
      fin,
      data,
    })
  }

  /// The absolute offset framed so far (bytes handed to `next_frame`).
  pub fn send_offset(&self) -> u64 {
    self.send_offset
  }
}

/// A refusal offering a segment to a stream (§4.10a §8): the closed set. Never a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamError {
  /// The segment reaches past the receive window (the flow-control credit); refused, not buffered.
  BeyondWindow {
    /// The segment's end offset.
    end: u64,
    /// The window ceiling.
    window: u64,
  },
  /// A `fin` was offered at a different final length than a prior `fin` — a protocol violation.
  FinChanged {
    /// The final length seen before.
    was: u64,
    /// The conflicting final length now.
    now: u64,
  },
}

/// The receive side of one ordered stream: a read cursor, the out-of-order segments buffered above
/// it (non-overlapping, keyed by start offset), the final length once `fin` is seen, and the window.
#[derive(Debug)]
pub struct StreamAssembler {
  read_offset: u64,
  buffered: BTreeMap<u64, Vec<u8>>,
  fin: Option<u64>,
  window: u64,
}

impl StreamAssembler {
  /// A receiver whose buffered bytes may reach absolute offset `window` (the flow-control credit).
  pub fn new(window: u64) -> StreamAssembler {
    StreamAssembler {
      read_offset: 0,
      buffered: BTreeMap::new(),
      fin: None,
      window,
    }
  }

  /// Offers a segment at absolute `offset`. Bytes below the read cursor and bytes already buffered
  /// are dropped (idempotent under reorder/duplicate/overlap); a `fin` records the stream's final
  /// length. Refuses a segment reaching past the window, or a `fin` conflicting with an earlier one.
  pub fn offer(&mut self, offset: u64, data: &[u8], fin: bool) -> Result<(), StreamError> {
    let end = offset.saturating_add(data.len() as u64);
    if end > self.window {
      return Err(StreamError::BeyondWindow {
        end,
        window: self.window,
      });
    }
    if fin {
      match self.fin {
        Some(was) if was != end => return Err(StreamError::FinChanged { was, now: end }),
        _ => self.fin = Some(end),
      }
    }
    // Drop the prefix at or below the read cursor (already delivered).
    let start = offset.max(self.read_offset);
    if start >= end {
      return Ok(());
    }
    // Fill only the gaps in [start, end) not already buffered, so overlaps dedup (QUIC guarantees
    // consistent bytes on overlap, so keeping the first copy is correct).
    let overlaps: Vec<(u64, u64)> = self
      .buffered
      .iter()
      .map(|(&s, v)| (s, s + v.len() as u64))
      .filter(|&(s, e)| s < end && e > start)
      .collect();
    let mut pos = start;
    for (seg_start, seg_end) in overlaps {
      if pos < seg_start {
        let gap_end = seg_start.min(end);
        self.buffer_gap(offset, pos, gap_end, data);
      }
      pos = pos.max(seg_end);
      if pos >= end {
        break;
      }
    }
    if pos < end {
      self.buffer_gap(offset, pos, end, data);
    }
    Ok(())
  }

  /// Buffers `data`'s bytes for the absolute range `[from, to)` (a gap), keyed by `from`. `from` and
  /// `to` are within `[data_offset, data_offset + data.len()]` by construction, so the offsets into
  /// `data` fit `usize` (clamped for i686 safety — the u64→usize conversion is checked, D-24).
  fn buffer_gap(&mut self, data_offset: u64, from: u64, to: u64, data: &[u8]) {
    let lo = usize::try_from(from - data_offset)
      .unwrap_or(0)
      .min(data.len());
    let hi = usize::try_from(to - data_offset)
      .unwrap_or(data.len())
      .min(data.len());
    if lo < hi {
      self.buffered.insert(from, data[lo..hi].to_vec());
    }
  }

  /// Drains and returns the bytes now contiguous from the read cursor (empty when the next byte is
  /// still missing), advancing the cursor past them. Adjacent buffered segments are consumed across
  /// their boundaries.
  pub fn read(&mut self) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(segment) = self.buffered.remove(&self.read_offset) {
      self.read_offset += segment.len() as u64;
      out.extend_from_slice(&segment);
    }
    out
  }

  /// Raises the receive window to `max` (monotonic) — the receiver granting itself more buffer as it
  /// advertises more flow-control credit to the peer (§4.10a §8). A lower value is ignored, so a
  /// reordered credit decision never shrinks the window.
  pub fn grant_window(&mut self, max: u64) {
    self.window = self.window.max(max);
  }

  /// The next byte offset the reader expects (bytes delivered so far).
  pub fn read_offset(&self) -> u64 {
    self.read_offset
  }

  /// Whether every byte through `fin` has been delivered by `read`.
  pub fn is_complete(&self) -> bool {
    self.fin == Some(self.read_offset)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use proptest::prelude::*;

  /// A generous window for the tests (a real window is the flow-control credit).
  const WINDOW: u64 = 1 << 20;

  /// In-order segments deliver immediately and in order, and `fin` completes the stream.
  #[test]
  fn in_order_segments_deliver_and_complete() {
    let mut s = StreamAssembler::new(WINDOW);
    s.offer(0, b"hello ", false).unwrap();
    assert_eq!(s.read(), b"hello ");
    s.offer(6, b"world", true).unwrap();
    assert_eq!(s.read(), b"world");
    assert!(s.is_complete(), "fin at 11 reached by the read cursor");
  }

  /// A gap is buffered until filled, then the whole contiguous prefix drains at once.
  #[test]
  fn a_gap_holds_until_filled() {
    let mut s = StreamAssembler::new(WINDOW);
    s.offer(6, b"world", false).unwrap(); // arrives first, out of order
    assert_eq!(s.read(), b"", "nothing contiguous yet");
    s.offer(0, b"hello ", false).unwrap();
    assert_eq!(
      s.read(),
      b"hello world",
      "the gap filled, all drains in order"
    );
  }

  /// Duplicate and overlapping segments are idempotent: the stream's bytes are delivered once.
  #[test]
  fn duplicates_and_overlaps_are_idempotent() {
    let mut s = StreamAssembler::new(WINDOW);
    s.offer(0, b"hello ", false).unwrap();
    s.offer(0, b"hello ", false).unwrap(); // exact duplicate
    s.offer(3, b"lo wor", false).unwrap(); // overlaps [3,6) and extends to 9
    s.offer(6, b"world", false).unwrap(); // overlaps [6,9), extends to 11
    assert_eq!(s.read(), b"hello world");
  }

  /// A segment past the window is refused, not buffered; a conflicting fin is refused.
  #[test]
  fn beyond_window_and_conflicting_fin_refuse() {
    let mut s = StreamAssembler::new(8);
    assert_eq!(
      s.offer(6, b"world", false),
      Err(StreamError::BeyondWindow { end: 11, window: 8 })
    );
    let mut s = StreamAssembler::new(WINDOW);
    s.offer(0, b"hello", true).unwrap();
    assert_eq!(
      s.offer(0, b"hello world", true),
      Err(StreamError::FinChanged { was: 5, now: 11 })
    );
  }

  /// A sender with no credit sends nothing; credit lets it send, bounded by that credit.
  #[test]
  fn credit_bounds_what_the_sender_emits() {
    let mut sender = StreamSender::new();
    sender.write(&[0u8; 100]);
    assert!(
      sender.next_frame(1, 4096).is_none(),
      "no credit, nothing to send"
    );
    sender.grant_credit(30);
    let mut sent = 0u64;
    while let Some(Frame::Stream { data, .. }) = sender.next_frame(1, 4096) {
      sent += data.len() as u64;
    }
    assert_eq!(
      sent, 30,
      "only the credit-worth is sent (never-whole-object)"
    );
    assert_eq!(sender.send_offset(), 30);
    sender.grant_credit(100);
    let mut more = 0u64;
    while let Some(Frame::Stream { data, .. }) = sender.next_frame(1, 4096) {
      more += data.len() as u64;
    }
    assert_eq!(more, 70, "the rest sends once credit is raised");
  }

  /// The frame cap bounds a single frame regardless of how much credit and data are available.
  #[test]
  fn the_frame_cap_bounds_a_frame() {
    let mut sender = StreamSender::new();
    sender.write(&[7u8; 100]);
    sender.grant_credit(100);
    let frame = sender.next_frame(1, 10).unwrap();
    let Frame::Stream { data, offset, .. } = frame else {
      panic!("expected a stream frame");
    };
    assert_eq!(offset, 0);
    assert_eq!(data.len(), 10, "the frame cap bounds the frame");
  }

  /// The send framer and the receive assembler compose into reliable ordered delivery: bytes written
  /// on one side, framed within credit at a small frame cap, reassemble to exactly those bytes, and
  /// the stream completes on the fin.
  #[test]
  fn send_and_receive_round_trip() {
    let content: Vec<u8> = (0..250u16)
      .map(|i| u8::try_from(i % 251).unwrap_or(0))
      .collect();
    let mut sender = StreamSender::new();
    sender.write(&content);
    sender.grant_credit(content.len() as u64);
    sender.finish();

    let mut assembler = StreamAssembler::new(content.len() as u64);
    let mut received = Vec::new();
    while let Some(Frame::Stream {
      offset, fin, data, ..
    }) = sender.next_frame(1, 7)
    {
      assembler.offer(offset, &data, fin).unwrap();
      received.extend_from_slice(&assembler.read());
    }
    assert_eq!(received, content, "send→receive reproduces the bytes");
    assert!(assembler.is_complete(), "the fin completed the stream");
  }

  proptest! {
    /// The reassembly oracle: a known byte string split into segments offered in an arbitrary order,
    /// with arbitrary duplicates, reassembles to exactly the original — proof of ordered, gapless,
    /// once-each delivery under reorder and duplication.
    #[test]
    fn reassembly_reproduces_the_stream(
      content in prop::collection::vec(any::<u8>(), 1..600),
      cuts in prop::collection::vec(0usize..600, 0..20),
      order_seed in any::<u64>(),
      dup_seed in any::<u64>(),
    ) {
      // Cut the content into contiguous segments at the sorted, in-bounds cut points.
      let mut points: Vec<usize> = cuts.into_iter().filter(|&c| c < content.len()).collect();
      points.push(0);
      points.push(content.len());
      points.sort_unstable();
      points.dedup();
      let mut segments: Vec<(u64, Vec<u8>)> = points
        .windows(2)
        .map(|w| (w[0] as u64, content[w[0]..w[1]].to_vec()))
        .filter(|(_, d)| !d.is_empty())
        .collect();
      // Duplicate some segments, and shuffle, with a simple deterministic PRNG over the seeds.
      let mut rng = order_seed ^ 0x9E37_79B9_7F4A_7C15;
      let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
      };
      let dups: Vec<(u64, Vec<u8>)> = segments
        .iter()
        .filter(|_| dup_seed.count_ones() % 2 == 0 || next() % 2 == 0)
        .cloned()
        .collect();
      segments.extend(dups);
      for i in (1..segments.len()).rev() {
        let j = usize::try_from(next() % (i as u64 + 1)).unwrap_or(0);
        segments.swap(i, j);
      }

      let mut s = StreamAssembler::new(content.len() as u64);
      let mut out = Vec::new();
      for (offset, data) in &segments {
        s.offer(*offset, data, false).unwrap();
        out.extend_from_slice(&s.read());
      }
      out.extend_from_slice(&s.read());
      prop_assert_eq!(out, content, "reassembly must reproduce the stream exactly");
    }
  }
}
