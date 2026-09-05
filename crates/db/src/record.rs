//! The log record over a ring region of the anchor segment (§4.8 `OpLog`: "contiguous chunked
//! ring in the anchor segment"; §4.9's rules: length checked against the cap before anything
//! is read, checksum verified before decode, the schema hash in front of the body).
//!
//! A record is a 32-byte header then the operation's canonical encoding. The ring's head and
//! tail are monotonic byte offsets (position = offset modulo capacity; a record may wrap); the
//! single writer copies the bytes, then publishes the tail with a release store, so a reader
//! that sees the tail sees the record. A crash inside a copy leaves a record whose header or
//! checksum does not verify; replay stops there and the writer resumes at that offset, so the
//! torn record, which was never acknowledged, is simply overwritten.

use std::sync::atomic::Ordering;

use slates_anchor::{AnchorSegment, RegionKind};
use slates_wire::Wire;
use slates_wire::crc32c::crc32c;

use crate::error::DbError;
use crate::op::Op;

/// Format: the record magic, `SLRC` in little-endian ASCII.
pub const RECORD_MAGIC: u32 = 0x4352_4C53;
/// Format: the header's size: magic (4), body length (4), sequence (8), checksum (4), padding
/// (4), schema hash (8).
pub const RECORD_HEADER: usize = 32;
/// Format: the magic's offset.
const AT_MAGIC: usize = 0;
/// Format: the body length's offset.
const AT_LEN: usize = 4;
/// Format: the sequence's offset.
const AT_SEQ: usize = 8;
/// Format: the checksum's offset (CRC32C over the sequence, the schema hash and the body).
const AT_CRC: usize = 16;
/// Format: the schema hash's offset.
const AT_SCHEMA: usize = 24;

/// What a replay found.
#[derive(Debug)]
pub struct Replayed {
  /// The records in sequence order, from the first at or after the requested sequence.
  pub ops: Vec<(u64, Op)>,
  /// The bytes walked.
  pub bytes: u64,
  /// Whether the walk ended at a record that did not verify (the torn tail); the ring's tail
  /// is reset to that offset by [`LogRing::truncate_to_verified`].
  pub torn: bool,
  /// The offset of the first byte after the last verified record.
  pub verified_end: u64,
  /// The sequence the next record takes.
  pub next_seq: u64,
}

/// The ring words, read once.
#[derive(Clone, Copy, Debug)]
struct Words {
  head: u64,
  tail: u64,
  capacity: u64,
  seq_base: u64,
}

/// A log ring: a log region or the audit region.
pub struct LogRing {
  kind: RegionKind,
}

impl LogRing {
  /// The ring over `kind` (a `Log(partition)` or `Audit` region).
  pub fn new(kind: RegionKind) -> LogRing {
    LogRing { kind }
  }

  fn words(&self, segment: &AnchorSegment) -> Result<Words, DbError> {
    let w = segment.ring_words(self.kind)?;
    Ok(Words {
      head: w[0].load(Ordering::Acquire),
      tail: w[1].load(Ordering::Acquire),
      capacity: w[2].load(Ordering::Acquire),
      seq_base: w[3].load(Ordering::Acquire),
    })
  }

  /// Bytes used, free and the capacity.
  pub fn usage(&self, segment: &AnchorSegment) -> Result<(u64, u64, u64), DbError> {
    let w = self.words(segment)?;
    let used = w.tail.saturating_sub(w.head);
    Ok((used, w.capacity.saturating_sub(used), w.capacity))
  }

  /// The bytes a record for `op` takes.
  pub fn record_bytes(op: &Op) -> u64 {
    let body = u64::try_from(op.to_bytes().len()).unwrap_or(u64::MAX);
    body.saturating_add(u64::try_from(RECORD_HEADER).unwrap_or(u64::MAX))
  }

  /// Appends `op` as record `seq`; refuses `LogFull` with nothing written when the ring cannot
  /// hold it. Returns the bytes appended.
  pub fn append(&self, segment: &mut AnchorSegment, seq: u64, op: &Op) -> Result<u64, DbError> {
    let w = self.words(segment)?;
    let body = op.to_bytes();
    let total = u64::try_from(RECORD_HEADER + body.len()).unwrap_or(u64::MAX);
    let free = w.capacity.saturating_sub(w.tail.saturating_sub(w.head));
    if total > free {
      return Err(DbError::LogFull {
        needed: total,
        free,
      });
    }
    let mut header = [0u8; RECORD_HEADER];
    let len = u32::try_from(body.len()).map_err(|_| DbError::Corrupt {
      seq,
      reason: "body longer than the format allows",
    })?;
    put(&mut header, AT_MAGIC, &RECORD_MAGIC.to_le_bytes());
    put(&mut header, AT_LEN, &len.to_le_bytes());
    put(&mut header, AT_SEQ, &seq.to_le_bytes());
    put(&mut header, AT_SCHEMA, &Op::SCHEMA_HASH.to_le_bytes());
    put(
      &mut header,
      AT_CRC,
      &checksum(seq, Op::SCHEMA_HASH, &body).to_le_bytes(),
    );
    let ring = segment.region_bytes_mut(self.kind)?;
    let ring = &mut ring[slates_anchor::layout::RING_BYTES..];
    write_wrapping(ring, w.capacity, w.tail, &header);
    write_wrapping(
      ring,
      w.capacity,
      w.tail
        .saturating_add(u64::try_from(RECORD_HEADER).unwrap_or(u64::MAX)),
      &body,
    );
    let words = segment.ring_words(self.kind)?;
    if w.head == w.tail {
      words[3].store(seq, Ordering::Release);
    }
    words[1].store(w.tail.saturating_add(total), Ordering::Release);
    Ok(total)
  }

  /// Walks the ring from its head, verifying every record, collecting those with a sequence
  /// at or after `from_seq`; stops at the first record that does not verify.
  pub fn replay(&self, segment: &AnchorSegment, from_seq: u64) -> Result<Replayed, DbError> {
    let w = self.words(segment)?;
    let ring = &segment.region_bytes(self.kind)?[slates_anchor::layout::RING_BYTES..];
    let mut at = w.head;
    let mut ops = Vec::new();
    let mut next_seq = w.seq_base;
    let mut torn = false;
    while at < w.tail {
      match self.record_at(ring, w, at, next_seq) {
        Ok((seq, op, size)) => {
          if seq >= from_seq {
            ops.push((seq, op));
          }
          next_seq = seq.saturating_add(1);
          at = at.saturating_add(size);
        }
        Err(_) => {
          torn = true;
          break;
        }
      }
    }
    Ok(Replayed {
      ops,
      bytes: at.saturating_sub(w.head),
      torn,
      verified_end: at,
      next_seq,
    })
  }

  /// Decodes and verifies the record at `at`, expecting sequence `expect_seq`.
  fn record_at(
    &self,
    ring: &[u8],
    w: Words,
    at: u64,
    expect_seq: u64,
  ) -> Result<(u64, Op, u64), DbError> {
    let header_len = u64::try_from(RECORD_HEADER).unwrap_or(u64::MAX);
    if w.tail.saturating_sub(at) < header_len {
      return Err(DbError::Corrupt {
        seq: expect_seq,
        reason: "header past the tail",
      });
    }
    let mut header = [0u8; RECORD_HEADER];
    read_wrapping(ring, w.capacity, at, &mut header);
    if read_u32(&header, AT_MAGIC) != RECORD_MAGIC {
      return Err(DbError::Corrupt {
        seq: expect_seq,
        reason: "wrong magic",
      });
    }
    let len = u64::from(read_u32(&header, AT_LEN));
    if len > w.tail.saturating_sub(at).saturating_sub(header_len) || len > w.capacity {
      return Err(DbError::Corrupt {
        seq: expect_seq,
        reason: "body length past the tail",
      });
    }
    let seq = read_u64(&header, AT_SEQ);
    if seq != expect_seq {
      return Err(DbError::Corrupt {
        seq: expect_seq,
        reason: "sequence out of order",
      });
    }
    let schema = read_u64(&header, AT_SCHEMA);
    if schema != Op::SCHEMA_HASH {
      return Err(DbError::Corrupt {
        seq,
        reason: "schema mismatch",
      });
    }
    let mut body = vec![0u8; usize::try_from(len).unwrap_or(usize::MAX)];
    read_wrapping(ring, w.capacity, at.saturating_add(header_len), &mut body);
    if read_u32(&header, AT_CRC) != checksum(seq, schema, &body) {
      return Err(DbError::Corrupt {
        seq,
        reason: "checksum mismatch",
      });
    }
    let op = Op::from_bytes(&body)?;
    Ok((seq, op, header_len.saturating_add(len)))
  }

  /// After a replay found a torn tail: the tail returns to the last verified byte so the next
  /// append overwrites the torn record.
  pub fn truncate_to_verified(
    &self,
    segment: &AnchorSegment,
    verified_end: u64,
  ) -> Result<(), DbError> {
    let words = segment.ring_words(self.kind)?;
    words[1].store(verified_end, Ordering::Release);
    Ok(())
  }

  /// Releases every record with a sequence below `up_to_seq` (after a snapshot covering them
  /// was published): the head walks forward over verified records.
  pub fn trim(&self, segment: &AnchorSegment, up_to_seq: u64) -> Result<u64, DbError> {
    let w = self.words(segment)?;
    let ring = &segment.region_bytes(self.kind)?[slates_anchor::layout::RING_BYTES..];
    let mut at = w.head;
    let mut seq = w.seq_base;
    let mut released = 0u64;
    while at < w.tail && seq < up_to_seq {
      let (_, _, size) = self.record_at(ring, w, at, seq)?;
      at = at.saturating_add(size);
      seq = seq.saturating_add(1);
      released = released.saturating_add(size);
    }
    let words = segment.ring_words(self.kind)?;
    words[3].store(seq, Ordering::Release);
    words[0].store(at, Ordering::Release);
    Ok(released)
  }
}

fn checksum(seq: u64, schema: u64, body: &[u8]) -> u32 {
  let mut covered = Vec::with_capacity(body.len() + size_of::<u64>() * 2);
  covered.extend_from_slice(&seq.to_le_bytes());
  covered.extend_from_slice(&schema.to_le_bytes());
  covered.extend_from_slice(body);
  crc32c(&covered)
}

fn position(capacity: u64, offset: u64) -> usize {
  usize::try_from(offset % capacity.max(1)).unwrap_or(0)
}

fn write_wrapping(ring: &mut [u8], capacity: u64, at: u64, bytes: &[u8]) {
  let start = position(capacity, at);
  let cap = usize::try_from(capacity)
    .unwrap_or(ring.len())
    .min(ring.len());
  let first = bytes.len().min(cap.saturating_sub(start));
  ring[start..start + first].copy_from_slice(&bytes[..first]);
  if first < bytes.len() {
    ring[..bytes.len() - first].copy_from_slice(&bytes[first..]);
  }
}

fn read_wrapping(ring: &[u8], capacity: u64, at: u64, out: &mut [u8]) {
  let start = position(capacity, at);
  let cap = usize::try_from(capacity)
    .unwrap_or(ring.len())
    .min(ring.len());
  let first = out.len().min(cap.saturating_sub(start));
  out[..first].copy_from_slice(&ring[start..start + first]);
  if first < out.len() {
    let rest = out.len() - first;
    out[first..].copy_from_slice(&ring[..rest]);
  }
}

fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
  bytes[at..at + value.len()].copy_from_slice(value);
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
  let mut word = [0u8; size_of::<u32>()];
  let n = word.len();
  word.copy_from_slice(&bytes[at..at + n]);
  u32::from_le_bytes(word)
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
  let mut word = [0u8; size_of::<u64>()];
  let n = word.len();
  word.copy_from_slice(&bytes[at..at + n]);
  u64::from_le_bytes(word)
}
