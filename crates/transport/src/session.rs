//! The session plane's frame codec (§4.10a §8; design: `docs/wip/fleet-transport.md`). The session
//! plane is slates's owned RFC 9000/9002-shaped QUIC dialect, adapting hecate-quic's ordered streams
//! and absolute-offset flow-control law but with a **TLS 1.3** handshake (`rustls::quic`, D-15, not
//! hecate's Noise) and **no warden/pod frame classes** (slates has no pods, so a host touches
//! payloads directly and the dialect needs only ordinary QUIC frames). This module is the pure
//! foundation: the frames that ride inside a TLS-1.3-protected packet's payload, and their codec.
//!
//! The frames (fixed-layout little-endian, slates's wire style per D-15 — not QUIC's varints):
//! - `Stream`  — ordered stream data at an **absolute** offset (idempotent under loss/reorder), with
//!   a `fin` marking the stream's end (the ordered-log archetype).
//! - `Ack`     — acknowledges packet numbers `[largest - range, largest]`.
//! - `MaxData` / `MaxStreamData` — the connection's and a stream's **absolute** flow-control credit
//!   (the ratified dual-level credit law).
//! - `ResetStream` / `StopSending` — a sender abandoning its half of a stream, and a receiver asking the
//!   sender to stop (RFC 9000 §19.4, §19.5): how a multiplexed session abandons one exchange without
//!   disturbing the others.
//!
//! Owed (later sub-slices): the connection state machine (packet numbers, ack/loss recovery, the
//! credit accounting), the `rustls::quic` handshake, and the wiring onto `rt`'s UDP driver. This is a
//! parser of external bytes, so every length is bounds-checked before it is read, an unknown frame
//! kind and a set reserved flag are typed refusals, and no input panics.

use crate::Reader;

/// A refusal decoding a session-plane frame sequence: the closed set of malformations. Never a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionError {
  /// The bytes ended before a frame field could be read.
  Truncated,
  /// A frame kind byte named no known frame.
  UnknownFrame(u8),
  /// A reserved flag bit was set on a `Stream` frame.
  FlagsSet,
}

impl std::fmt::Display for SessionError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      SessionError::Truncated => f.write_str("the frame sequence ended early"),
      SessionError::UnknownFrame(k) => write!(f, "unknown frame kind {k}"),
      SessionError::FlagsSet => f.write_str("a reserved stream-frame flag bit was set"),
    }
  }
}

impl std::error::Error for SessionError {}

/// One additional ACK Range below an ACK frame's first range (RFC 9000 §19.3.1), encoded relative to
/// the previous (higher) range. `gap` acknowledges nothing — it is the count of contiguous
/// unacknowledged packets between this run and the previous one, minus one; `len` is the count of
/// contiguous acknowledged packets in this run, minus one. So if the previous run's smallest
/// acknowledged packet number is `s`, this run acknowledges `[s - gap - 2 - len, s - gap - 2]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AckRange {
  /// The count of contiguous unacknowledged packets before this run, minus one ("Gap").
  pub gap: u64,
  /// The count of contiguous acknowledged packets in this run, minus one ("ACK Range Length").
  pub len: u64,
}

/// One session-plane frame — the payload of a TLS-1.3-protected packet is a sequence of these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
  /// Ordered stream data at an absolute offset; `fin` marks the stream's final byte.
  Stream {
    /// The stream this data belongs to (per (session, subject)).
    stream_id: u64,
    /// The absolute byte offset of `data` within the stream (idempotent under loss/reorder).
    offset: u64,
    /// Whether this frame carries the stream's final byte.
    fin: bool,
    /// The stream bytes at `offset`.
    data: Vec<u8>,
  },
  /// Acknowledges received packets (RFC 9000 §19.3): the first (highest) run is
  /// `[largest - range, largest]`, and each entry of `ranges` is a further run below it, encoded
  /// relative to the previous one (RFC 9000 §19.3.1). Multi-range, so a packet received *below* a gap
  /// is acknowledged too — not left for the sender to retransmit spuriously.
  Ack {
    /// The largest packet number acknowledged.
    largest: u64,
    /// How many packet numbers below `largest` are also acknowledged (the "First ACK Range").
    range: u64,
    /// Further acknowledged runs below the first, each relative to the previous (RFC 9000 §19.3.1).
    ranges: Vec<AckRange>,
  },
  /// The connection's absolute flow-control credit (bytes the peer may send across all streams).
  MaxData {
    /// The absolute byte ceiling.
    max: u64,
  },
  /// A stream's absolute flow-control credit.
  MaxStreamData {
    /// The stream.
    stream_id: u64,
    /// The absolute byte ceiling for that stream.
    max: u64,
  },
  /// The sender abandons its half of a stream (RFC 9000 §19.4): nothing more will be sent on it, and the
  /// receiver discards what it holds. `final_size` is how far the sender had framed, so the receiver's
  /// connection credit accounts for the bytes that will never be read.
  ResetStream {
    /// The stream.
    stream_id: u64,
    /// The bytes the sender had framed on it.
    final_size: u64,
  },
  /// The receiver no longer wants a stream (RFC 9000 §19.5): the sender stops sending on it — an
  /// abandoned exchange's reply stops costing the path.
  StopSending {
    /// The stream.
    stream_id: u64,
  },
  /// The cumulative stream credit (RFC 9000 §19.11 `MAX_STREAMS`): the peer may open streams whose
  /// sequence is below `max`. Raised as this end closes the peer's streams, so a sender never opens more
  /// streams than the receiver holds state for (`crate::streams`).
  MaxStreams {
    /// One past the highest stream sequence the peer may open.
    max: u64,
  },
  /// The sender is blocked by the connection's credit at `limit` (RFC 9000 §19.12 `DATA_BLOCKED`).
  /// Ack-eliciting and retransmitted on loss, so the receiver always answers with an acknowledgement,
  /// which carries its current credit — the credit reaches a blocked sender even when the acknowledgement
  /// that first carried it was lost.
  DataBlocked {
    /// The connection credit the sender is blocked at.
    limit: u64,
  },
  /// The sender has streams waiting past the peer's stream credit at `limit` (RFC 9000 §19.14
  /// `STREAMS_BLOCKED`); the receiver answers with an acknowledgement, which carries its current stream
  /// credit (`MaxStreams`).
  StreamsBlocked {
    /// The stream credit the sender is blocked at.
    limit: u64,
  },
  /// The sender is blocked by stream `stream_id`'s credit at `limit` (RFC 9000 §19.13
  /// `STREAM_DATA_BLOCKED`); the receiver answers with that stream's current credit.
  StreamDataBlocked {
    /// The blocked stream.
    stream_id: u64,
    /// The stream credit the sender is blocked at.
    limit: u64,
  },
  /// Asks for an acknowledgement and carries nothing else (RFC 9000 §19.2 `PING`): a path-MTU probe's one
  /// frame (`crate::pmtud`), its packet padded to the size under test. Never retransmitted — a lost probe
  /// is an answer, not data to recover.
  Ping,
}

/// Format: a `Stream` frame's bytes before its data — kind (1), stream id (8), offset (8), flags (1),
/// data length (4).
pub const STREAM_FRAME_HEADER_BYTES: usize = 1 + 8 + 8 + 1 + 4;
/// Format: an `Ack` frame's bytes before its additional ranges — kind (1), largest (8), first range (8),
/// range count (2).
pub const ACK_FRAME_BASE_BYTES: usize = 1 + 8 + 8 + 2;
/// Format: the bytes of each additional ACK range — gap (8), length (8).
pub const ACK_RANGE_BYTES: usize = 8 + 8;
/// Format: a frame of a kind byte and one `u64` (`MaxData`, `StopSending`, `MaxStreams`, `DataBlocked`).
const ONE_WORD_FRAME_BYTES: usize = 1 + 8;
/// Format: a frame of a kind byte and two `u64`s (`MaxStreamData`, `ResetStream`, `StreamDataBlocked`).
const TWO_WORD_FRAME_BYTES: usize = 1 + 8 + 8;
/// Format: a `Ping` frame's bytes (the kind alone).
const PING_FRAME_BYTES: usize = 1;

impl Frame {
  /// The bytes this frame encodes to — exactly the length [`encode_frames`] writes for it, so a packet is
  /// filled to its datagram budget and never past it.
  pub fn encoded_len(&self) -> usize {
    match self {
      Frame::Stream { data, .. } => STREAM_FRAME_HEADER_BYTES.saturating_add(data.len()),
      Frame::Ack { ranges, .. } => {
        let count = ranges.len().min(usize::from(u16::MAX));
        ACK_FRAME_BASE_BYTES.saturating_add(count.saturating_mul(ACK_RANGE_BYTES))
      }
      Frame::MaxData { .. }
      | Frame::StopSending { .. }
      | Frame::MaxStreams { .. }
      | Frame::DataBlocked { .. }
      | Frame::StreamsBlocked { .. } => ONE_WORD_FRAME_BYTES,
      Frame::MaxStreamData { .. } | Frame::ResetStream { .. } | Frame::StreamDataBlocked { .. } => {
        TWO_WORD_FRAME_BYTES
      }
      Frame::Ping => PING_FRAME_BYTES,
    }
  }
}

/// Format: RFC 9000 §19.1 — the PADDING frame is a single zero byte with no content; the decoder
/// consumes it and moves on. slates uses it to pad a packet up to the length header protection needs
/// to sample (RFC 9001 §5.4.2), so a small packet (an acknowledgement alone) is still protectable.
const KIND_PADDING: u8 = 0;
/// Format: the frame-kind tags on the wire.
const KIND_STREAM: u8 = 1;
/// Format: the acknowledgement frame kind.
const KIND_ACK: u8 = 2;
/// Format: the connection-credit frame kind.
const KIND_MAX_DATA: u8 = 3;
/// Format: the stream-credit frame kind.
const KIND_MAX_STREAM_DATA: u8 = 4;
/// Format: the stream-reset frame kind (RFC 9000 §19.4 `RESET_STREAM`).
const KIND_RESET_STREAM: u8 = 5;
/// Format: the stop-sending frame kind (RFC 9000 §19.5 `STOP_SENDING`).
const KIND_STOP_SENDING: u8 = 6;
/// Format: the stream-credit frame kind (RFC 9000 §19.11 `MAX_STREAMS`).
const KIND_MAX_STREAMS: u8 = 7;
/// Format: the connection-blocked frame kind (RFC 9000 §19.12 `DATA_BLOCKED`).
const KIND_DATA_BLOCKED: u8 = 8;
/// Format: the stream-blocked frame kind (RFC 9000 §19.13 `STREAM_DATA_BLOCKED`).
const KIND_STREAM_DATA_BLOCKED: u8 = 9;
/// Format: the streams-blocked frame kind (RFC 9000 §19.14 `STREAMS_BLOCKED`).
const KIND_STREAMS_BLOCKED: u8 = 10;
/// Format: the ping frame kind (RFC 9000 §19.2 `PING`), added with dialect 2 (`crate::params`).
const KIND_PING: u8 = 11;

/// Format: the `Stream` frame's `fin` flag bit; every other bit of the flags byte must be zero.
const STREAM_FIN: u8 = 0b0000_0001;

impl Frame {
  /// Appends this frame's canonical bytes to `out`.
  fn encode_into(&self, out: &mut Vec<u8>) {
    match self {
      Frame::Stream {
        stream_id,
        offset,
        fin,
        data,
      } => {
        out.push(KIND_STREAM);
        out.extend_from_slice(&stream_id.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.push(if *fin { STREAM_FIN } else { 0 });
        // A frame is frame-cap-bounded, so the length fits a u32; the builder refuses an oversize
        // frame first, so a saturating cast is only reachable by a bug.
        out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(data);
      }
      Frame::Ack {
        largest,
        range,
        ranges,
      } => {
        out.push(KIND_ACK);
        out.extend_from_slice(&largest.to_le_bytes());
        out.extend_from_slice(&range.to_le_bytes());
        // The additional-range count is a u16. The generator bounds the ranges to a frame-size budget
        // (far below u16::MAX), so the wire cap is never the binding limit; `take` keeps the written
        // count and body consistent even for a hand-built oversize frame.
        let count = u16::try_from(ranges.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&count.to_le_bytes());
        for r in ranges.iter().take(usize::from(count)) {
          out.extend_from_slice(&r.gap.to_le_bytes());
          out.extend_from_slice(&r.len.to_le_bytes());
        }
      }
      Frame::MaxData { max } => {
        out.push(KIND_MAX_DATA);
        out.extend_from_slice(&max.to_le_bytes());
      }
      Frame::MaxStreamData { stream_id, max } => {
        out.push(KIND_MAX_STREAM_DATA);
        out.extend_from_slice(&stream_id.to_le_bytes());
        out.extend_from_slice(&max.to_le_bytes());
      }
      Frame::ResetStream {
        stream_id,
        final_size,
      } => {
        out.push(KIND_RESET_STREAM);
        out.extend_from_slice(&stream_id.to_le_bytes());
        out.extend_from_slice(&final_size.to_le_bytes());
      }
      Frame::StopSending { stream_id } => {
        out.push(KIND_STOP_SENDING);
        out.extend_from_slice(&stream_id.to_le_bytes());
      }
      Frame::MaxStreams { max } => {
        out.push(KIND_MAX_STREAMS);
        out.extend_from_slice(&max.to_le_bytes());
      }
      Frame::DataBlocked { limit } => {
        out.push(KIND_DATA_BLOCKED);
        out.extend_from_slice(&limit.to_le_bytes());
      }
      Frame::StreamsBlocked { limit } => {
        out.push(KIND_STREAMS_BLOCKED);
        out.extend_from_slice(&limit.to_le_bytes());
      }
      Frame::StreamDataBlocked { stream_id, limit } => {
        out.push(KIND_STREAM_DATA_BLOCKED);
        out.extend_from_slice(&stream_id.to_le_bytes());
        out.extend_from_slice(&limit.to_le_bytes());
      }
      Frame::Ping => out.push(KIND_PING),
    }
  }
}

/// Encodes a packet payload's frame sequence to bytes.
pub fn encode_frames(frames: &[Frame]) -> Vec<u8> {
  let mut out = Vec::new();
  for frame in frames {
    frame.encode_into(&mut out);
  }
  out
}

/// Decodes a packet payload's frame sequence. Every length is bounds-checked before it is read (a
/// wild stream length never allocates past the input), an unknown kind and a set reserved flag are
/// typed refusals, and any malformation is a typed [`SessionError`], never a panic.
pub fn decode_frames(bytes: &[u8]) -> Result<Vec<Frame>, SessionError> {
  let mut reader = Reader::new(bytes);
  let mut frames = Vec::new();
  while !reader.is_empty() {
    let kind = reader.u8().map_err(|_| SessionError::Truncated)?;
    // PADDING (RFC 9000 §19.1): a lone zero byte, consumed with no frame produced.
    if kind == KIND_PADDING {
      continue;
    }
    let frame = match kind {
      KIND_STREAM => {
        let stream_id = reader.u64().map_err(|_| SessionError::Truncated)?;
        let offset = reader.u64().map_err(|_| SessionError::Truncated)?;
        let flags = reader.u8().map_err(|_| SessionError::Truncated)?;
        if flags & !STREAM_FIN != 0 {
          return Err(SessionError::FlagsSet);
        }
        let len = reader.u32().map_err(|_| SessionError::Truncated)? as usize;
        let data = reader
          .bytes(len)
          .map_err(|_| SessionError::Truncated)?
          .to_vec();
        Frame::Stream {
          stream_id,
          offset,
          fin: flags & STREAM_FIN != 0,
          data,
        }
      }
      KIND_ACK => {
        let largest = reader.u64().map_err(|_| SessionError::Truncated)?;
        let range = reader.u64().map_err(|_| SessionError::Truncated)?;
        let count = reader.u16().map_err(|_| SessionError::Truncated)?;
        // Read exactly `count` ranges. Each `u64` is bounds-checked against the remaining input and
        // nothing is pre-sized to `count`, so a hostile count truncates within the packet rather than
        // allocating past it (the parser-checks-length-before-reading discipline of this decoder).
        let mut ranges = Vec::new();
        for _ in 0..count {
          let gap = reader.u64().map_err(|_| SessionError::Truncated)?;
          let len = reader.u64().map_err(|_| SessionError::Truncated)?;
          ranges.push(AckRange { gap, len });
        }
        Frame::Ack {
          largest,
          range,
          ranges,
        }
      }
      KIND_MAX_DATA => Frame::MaxData {
        max: reader.u64().map_err(|_| SessionError::Truncated)?,
      },
      KIND_MAX_STREAM_DATA => Frame::MaxStreamData {
        stream_id: reader.u64().map_err(|_| SessionError::Truncated)?,
        max: reader.u64().map_err(|_| SessionError::Truncated)?,
      },
      KIND_RESET_STREAM => Frame::ResetStream {
        stream_id: reader.u64().map_err(|_| SessionError::Truncated)?,
        final_size: reader.u64().map_err(|_| SessionError::Truncated)?,
      },
      KIND_STOP_SENDING => Frame::StopSending {
        stream_id: reader.u64().map_err(|_| SessionError::Truncated)?,
      },
      KIND_MAX_STREAMS => Frame::MaxStreams {
        max: reader.u64().map_err(|_| SessionError::Truncated)?,
      },
      KIND_DATA_BLOCKED => Frame::DataBlocked {
        limit: reader.u64().map_err(|_| SessionError::Truncated)?,
      },
      KIND_STREAMS_BLOCKED => Frame::StreamsBlocked {
        limit: reader.u64().map_err(|_| SessionError::Truncated)?,
      },
      KIND_STREAM_DATA_BLOCKED => Frame::StreamDataBlocked {
        stream_id: reader.u64().map_err(|_| SessionError::Truncated)?,
        limit: reader.u64().map_err(|_| SessionError::Truncated)?,
      },
      KIND_PING => Frame::Ping,
      other => return Err(SessionError::UnknownFrame(other)),
    };
    frames.push(frame);
  }
  Ok(frames)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample() -> Vec<Frame> {
    vec![
      Frame::Stream {
        stream_id: 0x0102_0304_0506_0708,
        offset: 4096,
        fin: false,
        data: b"ordered stream bytes".to_vec(),
      },
      Frame::Ack {
        largest: 42,
        range: 7,
        ranges: vec![AckRange { gap: 0, len: 2 }, AckRange { gap: 4, len: 0 }],
      },
      Frame::MaxData { max: 1 << 20 },
      Frame::MaxStreamData {
        stream_id: 9,
        max: 1 << 16,
      },
      Frame::Stream {
        stream_id: 9,
        offset: 0,
        fin: true,
        data: Vec::new(),
      },
      Frame::ResetStream {
        stream_id: 11,
        final_size: 300,
      },
      Frame::StopSending { stream_id: 12 },
      Frame::MaxStreams { max: 64 },
      Frame::Ping,
      Frame::DataBlocked { limit: 1 << 18 },
      Frame::StreamsBlocked { limit: 70 },
      Frame::StreamDataBlocked {
        stream_id: 11,
        limit: 4096,
      },
    ]
  }

  /// A mixed frame sequence round-trips through encode/decode exactly, order and `fin` preserved.
  #[test]
  fn a_frame_sequence_round_trips() {
    let frames = sample();
    assert_eq!(decode_frames(&encode_frames(&frames)), Ok(frames));
  }

  /// Golden vectors for the wire format (CLAUDE.md §4 "golden vectors for everything in a format"): the
  /// exact bytes each frame kind encodes to, fixed little-endian per D-15. A change to the wire layout —
  /// which every peer and every future version must agree on — breaks this, not just a silent re-encode.
  #[test]
  fn the_frames_encode_to_their_golden_bytes() {
    // Ping: the kind byte alone (11), and it decodes back — even inside padding, as a probe carries it.
    assert_eq!(encode_frames(&[Frame::Ping]), vec![11]);
    assert_eq!(decode_frames(&[11, 0, 0, 0]), Ok(vec![Frame::Ping]));
    // MaxData: kind 3, then the ceiling as u64 LE.
    assert_eq!(
      encode_frames(&[Frame::MaxData { max: 5 }]),
      vec![3, 5, 0, 0, 0, 0, 0, 0, 0]
    );
    // MaxStreamData: kind 4, stream id (1) u64 LE, ceiling (256 = 0x0100) u64 LE.
    assert_eq!(
      encode_frames(&[Frame::MaxStreamData {
        stream_id: 1,
        max: 256
      }]),
      vec![4, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]
    );
    // Ack: kind 2, largest (2) u64 LE, first range (1) u64 LE, additional-range count (1) u16 LE, then
    // that range's gap (0) and length (3) as u64 LE each.
    assert_eq!(
      encode_frames(&[Frame::Ack {
        largest: 2,
        range: 1,
        ranges: vec![AckRange { gap: 0, len: 3 }],
      }]),
      vec![
        2, // KIND_ACK
        2, 0, 0, 0, 0, 0, 0, 0, // largest
        1, 0, 0, 0, 0, 0, 0, 0, // first range
        1, 0, // one additional range
        0, 0, 0, 0, 0, 0, 0, 0, // gap
        3, 0, 0, 0, 0, 0, 0, 0, // len
      ]
    );
    // Stream: kind 1, stream id (7) u64 LE, offset (4) u64 LE, fin flag (1), data length (2) u32 LE, data.
    assert_eq!(
      encode_frames(&[Frame::Stream {
        stream_id: 7,
        offset: 4,
        fin: true,
        data: vec![0xAA, 0xBB],
      }]),
      vec![
        1, // KIND_STREAM
        7, 0, 0, 0, 0, 0, 0, 0, // stream id
        4, 0, 0, 0, 0, 0, 0, 0, // offset
        1, // STREAM_FIN
        2, 0, 0, 0, // data length
        0xAA, 0xBB, // data
      ]
    );
  }

  /// Golden vectors for the stream-control frames (the same contract as the data frames above): the exact
  /// bytes each encodes to, little-endian per D-15.
  #[test]
  fn the_stream_control_frames_encode_to_their_golden_bytes() {
    // ResetStream: kind 5, stream id (3) u64 LE, final size (258 = 0x0102) u64 LE.
    assert_eq!(
      encode_frames(&[Frame::ResetStream {
        stream_id: 3,
        final_size: 258
      }]),
      vec![5, 3, 0, 0, 0, 0, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0]
    );
    // StopSending: kind 6, stream id (4) u64 LE.
    assert_eq!(
      encode_frames(&[Frame::StopSending { stream_id: 4 }]),
      vec![6, 4, 0, 0, 0, 0, 0, 0, 0]
    );
    // MaxStreams: kind 7, max (0x0105) u64 LE.
    assert_eq!(
      encode_frames(&[Frame::MaxStreams { max: 0x0105 }]),
      vec![7, 5, 1, 0, 0, 0, 0, 0, 0]
    );
    // DataBlocked: kind 8, limit (0x0203) u64 LE.
    assert_eq!(
      encode_frames(&[Frame::DataBlocked { limit: 0x0203 }]),
      vec![8, 3, 2, 0, 0, 0, 0, 0, 0]
    );
    // StreamsBlocked: kind 10, limit (0x0405) u64 LE.
    assert_eq!(
      encode_frames(&[Frame::StreamsBlocked { limit: 0x0405 }]),
      vec![10, 5, 4, 0, 0, 0, 0, 0, 0]
    );
    // StreamDataBlocked: kind 9, stream id (6) u64 LE, limit (0x0304) u64 LE.
    assert_eq!(
      encode_frames(&[Frame::StreamDataBlocked {
        stream_id: 6,
        limit: 0x0304
      }]),
      vec![9, 6, 0, 0, 0, 0, 0, 0, 0, 4, 3, 0, 0, 0, 0, 0, 0]
    );
    // PADDING is a single zero byte the decoder consumes with no frame produced.
    assert_eq!(decode_frames(&[0]), Ok(Vec::new()));
  }

  /// A truncated reset or stop-sending frame is a typed truncation, never a panic or a partial frame.
  #[test]
  fn truncated_stream_control_frames_refuse() {
    let reset = encode_frames(&[Frame::ResetStream {
      stream_id: 3,
      final_size: 258,
    }]);
    let stop = encode_frames(&[Frame::StopSending { stream_id: 4 }]);
    let credit = encode_frames(&[Frame::MaxStreams { max: 0x0105 }]);
    for blocked in [
      encode_frames(&[Frame::DataBlocked { limit: 0x0203 }]),
      encode_frames(&[Frame::StreamsBlocked { limit: 0x0405 }]),
      encode_frames(&[Frame::StreamDataBlocked {
        stream_id: 6,
        limit: 0x0304,
      }]),
    ] {
      for cut in 1..blocked.len() {
        assert_eq!(
          decode_frames(&blocked[..cut]),
          Err(SessionError::Truncated),
          "blocked cut at {cut}"
        );
      }
    }
    for cut in 1..credit.len() {
      assert_eq!(
        decode_frames(&credit[..cut]),
        Err(SessionError::Truncated),
        "stream credit cut at {cut}"
      );
    }
    for cut in 1..reset.len() {
      assert_eq!(
        decode_frames(&reset[..cut]),
        Err(SessionError::Truncated),
        "reset cut at {cut}"
      );
    }
    for cut in 1..stop.len() {
      assert_eq!(
        decode_frames(&stop[..cut]),
        Err(SessionError::Truncated),
        "stop cut at {cut}"
      );
    }
  }

  /// Every frame's `encoded_len` is exactly the bytes `encode_frames` writes for it — the packet builder
  /// fills a datagram by these lengths, so a mismatch would overrun the path's floor (T-transport: a
  /// packet that grew past its budget was truncated at the receiver and read as a loss, 2026-09-28).
  #[test]
  fn encoded_len_is_exactly_what_encoding_writes() {
    let mut frames = sample();
    frames.push(Frame::Stream {
      stream_id: 1,
      offset: 0,
      fin: true,
      data: Vec::new(),
    });
    frames.push(Frame::Ack {
      largest: 9,
      range: 0,
      ranges: Vec::new(),
    });
    for frame in &frames {
      assert_eq!(
        frame.encoded_len(),
        encode_frames(std::slice::from_ref(frame)).len(),
        "{frame:?}"
      );
    }
  }

  /// An empty payload decodes to no frames.
  #[test]
  fn an_empty_payload_decodes_to_no_frames() {
    assert_eq!(decode_frames(&[]), Ok(Vec::new()));
  }

  /// PADDING bytes (zeros) around a real frame are skipped, leaving the real frame intact — the
  /// property header protection relies on to pad a small packet up to a sampleable length.
  #[test]
  fn padding_bytes_are_skipped() {
    let real = Frame::MaxData { max: 5 };
    let mut wire = vec![0u8, 0, 0]; // leading PADDING
    wire.extend_from_slice(&encode_frames(std::slice::from_ref(&real)));
    wire.extend_from_slice(&[0u8; 5]); // trailing PADDING
    assert_eq!(decode_frames(&wire), Ok(vec![real]));
    // Padding alone decodes to nothing.
    assert_eq!(decode_frames(&[0u8; 16]), Ok(Vec::new()));
  }

  /// Hostile inputs are typed refusals, never panics (§4.10a hostile-input rule).
  #[test]
  fn hostile_frames_refuse_by_type() {
    let good = encode_frames(&sample());

    // An unknown frame kind is named.
    assert_eq!(
      decode_frames(&[0xFF]),
      Err(SessionError::UnknownFrame(0xFF))
    );

    // A truncated tail (drop the last bytes of the trailing stream data) is truncated, not a panic.
    assert_eq!(
      decode_frames(&good[..good.len() - 1]),
      Err(SessionError::Truncated)
    );

    // A stream frame with a reserved flag bit set is refused. Its flags byte is at
    // kind(1) + stream_id(8) + offset(8) = offset 17 of the first (Stream) frame.
    let mut flagged = good.clone();
    flagged[1 + size_of::<u64>() + size_of::<u64>()] = 0b0000_0010;
    assert_eq!(decode_frames(&flagged), Err(SessionError::FlagsSet));

    // A wild stream length (u32::MAX) cannot fit the remaining bytes.
    let mut wild = good.clone();
    let len_at = 1 + size_of::<u64>() + size_of::<u64>() + size_of::<u8>();
    wild[len_at..len_at + size_of::<u32>()].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(decode_frames(&wild), Err(SessionError::Truncated));
  }

  /// A multi-range ACK round-trips exactly, and a hostile ACK that claims more ranges than its bytes
  /// carry is a typed truncation, never an over-allocation or a panic (RFC 9000 §19.3.1).
  #[test]
  fn a_multi_range_ack_round_trips_and_a_wild_count_truncates() {
    let ack = Frame::Ack {
      largest: 1000,
      range: 3,
      ranges: vec![
        AckRange { gap: 1, len: 4 },
        AckRange { gap: 0, len: 0 },
        AckRange { gap: 9, len: 2 },
      ],
    };
    let wire = encode_frames(std::slice::from_ref(&ack));
    assert_eq!(decode_frames(&wire), Ok(vec![ack]));

    // Overwrite the u16 range count (at kind(1) + largest(8) + range(8)) with u16::MAX: the decoder
    // must exhaust the input reading ranges and refuse, not allocate a huge vector.
    let mut wild = wire.clone();
    let count_at = 1 + size_of::<u64>() + size_of::<u64>();
    wild[count_at..count_at + size_of::<u16>()].copy_from_slice(&u16::MAX.to_le_bytes());
    assert_eq!(decode_frames(&wild), Err(SessionError::Truncated));
  }
}
