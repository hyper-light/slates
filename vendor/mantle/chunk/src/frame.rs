//! Index log frames: the second, separately written copy of every record's identity and
//! location (docs/design/chunk-store.md §3.2).
//!
//! One frame holds one group-commit batch's index updates, or one piece of a checkpoint.
//! Frames carry consecutive LSNs and the volume's id, so recovery can tell the end of the
//! log (nothing valid with the next LSN) from a frame left by an earlier volume on the
//! same device, and a torn tail from corruption in the middle (Alagappan et al., FAST 2018,
//! §3.3.3).

use crate::key::ChunkKey;
use mantle_codec::{Reader, Writer};

pub const FRAME_MAGIC: [u8; 4] = *b"MNIX";
pub const FRAME_VERSION: u16 = 2;
pub const FRAME_HEADER: usize = 48;
const CRC_AT: usize = 44;

pub const KIND_BATCH: u16 = 1;
/// The rest of the log region is unused this lap; the next frame is at position 0.
pub const KIND_WRAP: u16 = 2;
pub const KIND_CHECKPOINT_BEGIN: u16 = 3;
pub const KIND_CHECKPOINT_CHUNK: u16 = 4;
pub const KIND_CHECKPOINT_END: u16 = 5;

const TAG_PUT: u8 = 1;
const TAG_DELETE: u8 = 2;
const TAG_SEGMENT: u8 = 3;

pub const PUT_LEN: usize = 1 + ChunkKey::ENCODED_LEN + 4 + 8 + 4 + 4 + 8 + 4 + 4 + 8 + 8 + 1;
pub const DELETE_LEN: usize = 1 + ChunkKey::ENCODED_LEN + 8 + 8;
pub const SEGMENT_LEN: usize = 1 + 4 + 8 + 1 + 4;

/// A chunk fragment written at a location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutRecord {
    pub key: ChunkKey,
    pub segment: u32,
    pub incarnation: u64,
    /// Byte offset of the data record within its segment.
    pub offset: u32,
    /// Bytes of the whole data record, header to padding.
    pub record_len: u32,
    pub chunk_offset: u64,
    pub payload_len: u32,
    /// CRC-32C of the fragment's payload.
    pub payload_crc: u32,
    pub sequence: u64,
    pub time_ns: u64,
    /// `record::FLAG_FINAL` when the fragment seals the chunk.
    pub flags: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleteRecord {
    pub key: ChunkKey,
    pub sequence: u64,
    pub time_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentState {
    Free,
    Open,
    Sealed,
}

impl SegmentState {
    fn code(self) -> u8 {
        match self {
            Self::Free => 0,
            Self::Open => 1,
            Self::Sealed => 2,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Free),
            1 => Some(Self::Open),
            2 => Some(Self::Sealed),
            _ => None,
        }
    }
}

/// A segment's state change, or (in a checkpoint) its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentRecord {
    pub segment: u32,
    pub incarnation: u64,
    pub state: SegmentState,
    /// Bytes of the segment written in this incarnation.
    pub write_pos: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogRecord {
    Put(PutRecord),
    Delete(DeleteRecord),
    Segment(SegmentRecord),
}

impl LogRecord {
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Put(_) => PUT_LEN,
            Self::Delete(_) => DELETE_LEN,
            Self::Segment(_) => SEGMENT_LEN,
        }
    }

    fn encode(&self, w: &mut Writer) {
        match self {
            Self::Put(p) => {
                w.u8(TAG_PUT);
                p.key.encode(w);
                w.u32(p.segment);
                w.u64(p.incarnation);
                w.u32(p.offset);
                w.u32(p.record_len);
                w.u64(p.chunk_offset);
                w.u32(p.payload_len);
                w.u32(p.payload_crc);
                w.u64(p.sequence);
                w.u64(p.time_ns);
                w.u8(p.flags);
            }
            Self::Delete(d) => {
                w.u8(TAG_DELETE);
                d.key.encode(w);
                w.u64(d.sequence);
                w.u64(d.time_ns);
            }
            Self::Segment(s) => {
                w.u8(TAG_SEGMENT);
                w.u32(s.segment);
                w.u64(s.incarnation);
                w.u8(s.state.code());
                w.u32(s.write_pos);
            }
        }
    }

    fn decode(r: &mut Reader<'_>) -> Option<Self> {
        match r.u8()? {
            TAG_PUT => Some(Self::Put(PutRecord {
                key: ChunkKey::decode(r)?,
                segment: r.u32()?,
                incarnation: r.u64()?,
                offset: r.u32()?,
                record_len: r.u32()?,
                chunk_offset: r.u64()?,
                payload_len: r.u32()?,
                payload_crc: r.u32()?,
                sequence: r.u64()?,
                time_ns: r.u64()?,
                flags: r.u8()?,
            })),
            TAG_DELETE => Some(Self::Delete(DeleteRecord {
                key: ChunkKey::decode(r)?,
                sequence: r.u64()?,
                time_ns: r.u64()?,
            })),
            TAG_SEGMENT => Some(Self::Segment(SegmentRecord {
                segment: r.u32()?,
                incarnation: r.u64()?,
                state: SegmentState::from_code(r.u8()?)?,
                write_pos: r.u32()?,
            })),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub kind: u16,
    pub lsn: u64,
    /// The LSN of the first frame of this frame's flush group: frames written between two
    /// device flushes. Stored as the distance back from `lsn`.
    pub group: u64,
    pub volume: u128,
    pub payload_len: u32,
    pub count: u32,
}

impl FrameHeader {
    /// Bytes the frame occupies in the log, padded to `block`.
    pub fn frame_len(&self, block: usize) -> Option<usize> {
        FRAME_HEADER
            .checked_add(usize::try_from(self.payload_len).ok()?)?
            .checked_next_multiple_of(block)
    }
}

/// Encodes a frame padded to `block` bytes. `group` is the LSN of the first frame written
/// since the last completed flush.
pub fn encode(
    kind: u16,
    lsn: u64,
    group: u64,
    volume: u128,
    records: &[LogRecord],
    block: usize,
) -> Option<Vec<u8>> {
    let group_delta = u32::try_from(lsn.checked_sub(group)?).ok()?;
    let payload: usize = records.iter().map(LogRecord::encoded_len).sum();
    let total = FRAME_HEADER
        .checked_add(payload)?
        .checked_next_multiple_of(block)?;
    let mut w = Writer::with_capacity(total);
    w.bytes(&FRAME_MAGIC);
    w.u16(FRAME_VERSION);
    w.u16(kind);
    w.u64(lsn);
    w.u128(volume);
    w.u32(u32::try_from(payload).ok()?);
    w.u32(u32::try_from(records.len()).ok()?);
    w.u32(group_delta);
    let mut body = Writer::with_capacity(payload);
    for record in records {
        record.encode(&mut body);
    }
    let mut crc = mantle_crc::Crc32c::new();
    crc.update(w.as_slice());
    crc.update(body.as_slice());
    w.u32(crc.finish());
    w.bytes(body.as_slice());
    w.zeros(total.saturating_sub(w.len()));
    Some(w.into_vec())
}

/// Reads a frame header without verifying the frame: enough to learn how long it is.
pub fn peek(bytes: &[u8]) -> Option<FrameHeader> {
    let mut r = Reader::new(bytes);
    if r.take(4)? != FRAME_MAGIC || r.u16()? != FRAME_VERSION {
        return None;
    }
    let kind = r.u16()?;
    let lsn = r.u64()?;
    let volume = r.u128()?;
    let payload_len = r.u32()?;
    let count = r.u32()?;
    let group = lsn.checked_sub(u64::from(r.u32()?))?;
    Some(FrameHeader {
        kind,
        lsn,
        group,
        volume,
        payload_len,
        count,
    })
}

/// Decodes and verifies a whole frame of `volume`; `None` if it is torn, corrupt, or not
/// this volume's.
pub fn decode(bytes: &[u8], volume: u128) -> Option<(FrameHeader, Vec<LogRecord>)> {
    let header = peek(bytes)?;
    if header.volume != volume {
        return None;
    }
    let payload_len = usize::try_from(header.payload_len).ok()?;
    let payload = bytes.get(FRAME_HEADER..FRAME_HEADER.checked_add(payload_len)?)?;
    let crc = u32::from_le_bytes(bytes.get(CRC_AT..FRAME_HEADER)?.try_into().ok()?);
    let mut check = mantle_crc::Crc32c::new();
    check.update(bytes.get(..CRC_AT)?);
    check.update(payload);
    if check.finish() != crc {
        return None;
    }
    let mut r = Reader::new(payload);
    let count = usize::try_from(header.count).ok()?;
    // A record is at least SEGMENT_LEN bytes, which bounds the count by the payload.
    if count > payload_len.checked_div(SEGMENT_LEN).unwrap_or(0) {
        return None;
    }
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        records.push(LogRecord::decode(&mut r)?);
    }
    if r.remaining() != 0 {
        return None;
    }
    Some((header, records))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records() -> Vec<LogRecord> {
        let key = ChunkKey {
            block: 7,
            epoch: 1,
            index: 2,
        };
        vec![
            LogRecord::Segment(SegmentRecord {
                segment: 1,
                incarnation: 4,
                state: SegmentState::Open,
                write_pos: 4096,
            }),
            LogRecord::Put(PutRecord {
                key,
                segment: 1,
                incarnation: 4,
                offset: 4096,
                record_len: 1200,
                chunk_offset: 0,
                payload_len: 1000,
                payload_crc: 0xDEAD_BEEF,
                sequence: 9,
                time_ns: 10,
                flags: 1,
            }),
            LogRecord::Delete(DeleteRecord {
                key,
                sequence: 11,
                time_ns: 12,
            }),
        ]
    }

    #[test]
    fn frames_round_trip_padded_to_the_block() {
        let bytes = encode(KIND_BATCH, 5, 3, 99, &records(), 4096).unwrap();
        assert_eq!(bytes.len(), 4096);
        let (header, decoded) = decode(&bytes, 99).unwrap();
        assert_eq!(header.lsn, 5);
        assert_eq!(header.group, 3);
        assert_eq!(header.kind, KIND_BATCH);
        assert_eq!(decoded, records());
        assert_eq!(header.frame_len(4096), Some(4096));
    }

    #[test]
    fn another_volumes_frame_is_not_ours() {
        let bytes = encode(KIND_BATCH, 5, 5, 99, &records(), 4096).unwrap();
        assert!(decode(&bytes, 100).is_none());
    }

    #[test]
    fn any_damage_to_header_or_payload_is_detected() {
        let bytes = encode(KIND_BATCH, 5, 5, 99, &records(), 4096).unwrap();
        let used = FRAME_HEADER + records().iter().map(LogRecord::encoded_len).sum::<usize>();
        for i in 0..used {
            let mut copy = bytes.clone();
            copy[i] ^= 0x01;
            assert!(decode(&copy, 99).is_none(), "byte {i}");
        }
    }

    #[test]
    fn an_absurd_record_count_is_refused_without_allocating() {
        let mut bytes = encode(KIND_BATCH, 5, 5, 99, &[], 4096).unwrap();
        bytes[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&bytes, 99).is_none());
    }
}
