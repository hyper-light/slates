//! Data records and segment headers: the bytes that hold chunk payloads in segments.
//!
//! A record identifies the chunk it belongs to, where in the chunk its payload goes, the
//! volume and segment incarnation it was written in, and a CRC-32C per checksum block of
//! its payload; the header and checksum table carry their own CRC-32C (docs/design/
//! chunk-store.md §3.1). Identity with the data catches misdirected writes and stale data
//! (Bairavasundaram et al., FAST 2008, §2.2); per-block checksums let a read verify only
//! the blocks it returns (Ghemawat et al., SOSP 2003, §5.2).

use hyper_block::buf::AlignedBuf;

use crate::key::ChunkKey;
use mantle_codec::{Reader, Writer};

pub const RECORD_MAGIC: [u8; 4] = *b"MNRC";
pub const SEGMENT_MAGIC: [u8; 4] = *b"MNSG";
/// Bytes of a record header before its checksum table.
pub const HEADER_LEN: usize = 96;
/// Byte offset of the header CRC within the header.
const CRC_AT: usize = 92;
/// Records start 8-byte aligned within a batch.
pub const RECORD_ALIGN: usize = 8;

pub const KIND_PAYLOAD: u8 = 1;
/// The chunk is sealed at the end of this record's payload.
pub const FLAG_FINAL: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordHeader {
    pub flags: u8,
    pub checksum_shift: u8,
    pub volume: u128,
    pub segment: u32,
    pub incarnation: u64,
    pub sequence: u64,
    pub key: ChunkKey,
    pub chunk_offset: u64,
    pub payload_len: u32,
    pub time_ns: u64,
}

impl RecordHeader {
    pub fn is_final(&self) -> bool {
        self.flags & FLAG_FINAL != 0
    }

    /// Checksum blocks covering the payload.
    pub fn blocks(&self) -> Option<u32> {
        blocks(self.payload_len, self.checksum_shift)
    }

    /// Header plus checksum table.
    pub fn prefix_len(&self) -> Option<usize> {
        prefix_len(self.payload_len, self.checksum_shift)
    }

    /// The whole record on disk: prefix, payload, and padding to `RECORD_ALIGN`.
    pub fn record_len(&self) -> Option<usize> {
        record_len(self.payload_len, self.checksum_shift)
    }
}

pub fn blocks(payload_len: u32, shift: u8) -> Option<u32> {
    let size = 1u64.checked_shl(u32::from(shift))?;
    u32::try_from(u64::from(payload_len).div_ceil(size)).ok()
}

pub fn prefix_len(payload_len: u32, shift: u8) -> Option<usize> {
    let table = usize::try_from(blocks(payload_len, shift)?)
        .ok()?
        .checked_mul(4)?;
    HEADER_LEN.checked_add(table)
}

pub fn record_len(payload_len: u32, shift: u8) -> Option<usize> {
    let unpadded =
        prefix_len(payload_len, shift)?.checked_add(usize::try_from(payload_len).ok()?)?;
    unpadded.checked_next_multiple_of(RECORD_ALIGN)
}

/// A payload and its checksums, computed by the thread that submits it: the source computes
/// the checksum, the store verifies it on ingest and on every read (docs/research/03 rule
/// 10), and the single writer thread is spared the work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    pub data: Vec<u8>,
    /// CRC-32C of each checksum block of `data`.
    pub table: Vec<u32>,
    /// CRC-32C of all of `data`.
    pub crc: u32,
}

impl Payload {
    /// Checksums `data` in blocks of 2^`shift` bytes; `None` for a shift past 30.
    pub fn new(data: Vec<u8>, shift: u8) -> Option<Self> {
        let (table, crc) = checksums(&data, shift)?;
        Some(Self { data, table, crc })
    }
}

/// The CRC-32C of each 2^`shift`-byte block of `payload`, and of the whole payload combined
/// from them without a second pass over the bytes.
pub fn checksums(payload: &[u8], shift: u8) -> Option<(Vec<u32>, u32)> {
    if shift > 30 {
        return None;
    }
    let size = usize::try_from(1u64.checked_shl(u32::from(shift))?).ok()?;
    let table: Vec<u32> = payload.chunks(size).map(mantle_crc::crc32c).collect();
    // Every block but the last is the same size, so one shift combines them all.
    let full = mantle_crc::Crc32cShift::new(u64::try_from(size).ok()?);
    let mut whole: Option<u32> = None;
    for (c, block) in table.iter().zip(payload.chunks(size)) {
        whole = Some(match whole {
            None => *c,
            Some(acc) if block.len() == size => full.combine(acc, *c),
            Some(acc) => mantle_crc::crc32c_combine(acc, *c, u64::try_from(block.len()).ok()?),
        });
    }
    Some((table, whole.unwrap_or_else(|| mantle_crc::crc32c(&[]))))
}

/// Where records are encoded: a vector, or the aligned buffer the writer sends to the device.
pub trait Sink {
    fn put(&mut self, bytes: &[u8]) -> Option<()>;
}

impl Sink for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) -> Option<()> {
        self.extend_from_slice(bytes);
        Some(())
    }
}

impl Sink for AlignedBuf {
    fn put(&mut self, bytes: &[u8]) -> Option<()> {
        self.extend_from_slice(bytes).ok()
    }
}

/// Appends a record for `payload`, whose checksum table is `table`, to `out`.
pub fn encode(
    header: &RecordHeader,
    table: &[u32],
    payload: &[u8],
    out: &mut impl Sink,
) -> Option<()> {
    if usize::try_from(header.payload_len).ok()? != payload.len()
        || usize::try_from(header.blocks()?).ok()? != table.len()
    {
        return None;
    }
    let mut w = Writer::with_capacity(header.prefix_len()?);
    w.bytes(&RECORD_MAGIC);
    w.u8(KIND_PAYLOAD);
    w.u8(header.flags);
    w.u8(header.checksum_shift);
    w.u8(0);
    w.u128(header.volume);
    w.u32(header.segment);
    w.u64(header.incarnation);
    w.u64(header.sequence);
    header.key.encode(&mut w);
    w.u16(0);
    w.u64(header.chunk_offset);
    w.u32(header.payload_len);
    w.u32(u32::try_from(table.len()).ok()?);
    w.u64(header.time_ns);
    // The CRC covers the header before it and the table after it.
    let mut crc = mantle_crc::Crc32c::new();
    crc.update(w.as_slice());
    let mut table_bytes = Writer::with_capacity(table.len().saturating_mul(4));
    for &c in table {
        table_bytes.u32(c);
    }
    crc.update(table_bytes.as_slice());
    w.u32(crc.finish());
    w.bytes(table_bytes.as_slice());

    out.put(w.as_slice())?;
    out.put(payload)?;
    let written = w.len().saturating_add(payload.len());
    let pad = written
        .checked_next_multiple_of(RECORD_ALIGN)?
        .saturating_sub(written);
    out.put([0u8; RECORD_ALIGN].get(..pad)?)
}

/// A decoded header and checksum table, with the header CRC verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix {
    pub header: RecordHeader,
    pub table: Vec<u32>,
}

/// The length fields of a record header, read without verifying it: enough to know how many
/// bytes to read before verifying the whole prefix with `decode_prefix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lengths {
    pub payload_len: u32,
    pub checksum_shift: u8,
}

pub fn peek_lengths(bytes: &[u8]) -> Option<Lengths> {
    let mut r = Reader::new(bytes);
    if r.take(4)? != RECORD_MAGIC || r.u8()? != KIND_PAYLOAD {
        return None;
    }
    r.u8()?;
    let checksum_shift = r.u8()?;
    if checksum_shift > 30 {
        return None;
    }
    r.take(1 + 16 + 4 + 8 + 8 + ChunkKey::ENCODED_LEN + 2 + 8)?;
    Some(Lengths {
        payload_len: r.u32()?,
        checksum_shift,
    })
}

/// Decodes the header and checksum table at the start of `bytes`. `None` if the bytes are
/// not an intact record prefix: a torn, stale, or corrupt record reads as absent.
pub fn decode_prefix(bytes: &[u8]) -> Option<Prefix> {
    let mut r = Reader::new(bytes);
    if r.take(4)? != RECORD_MAGIC || r.u8()? != KIND_PAYLOAD {
        return None;
    }
    let flags = r.u8()?;
    let checksum_shift = r.u8()?;
    r.u8()?;
    let volume = r.u128()?;
    let segment = r.u32()?;
    let incarnation = r.u64()?;
    let sequence = r.u64()?;
    let key = ChunkKey::decode(&mut r)?;
    r.u16()?;
    let chunk_offset = r.u64()?;
    let payload_len = r.u32()?;
    let count = r.u32()?;
    let time_ns = r.u64()?;
    let crc = r.u32()?;
    if checksum_shift > 30 || blocks(payload_len, checksum_shift)? != count {
        return None;
    }
    let table_len = usize::try_from(count).ok()?.checked_mul(4)?;
    let table_bytes = r.take(table_len)?;
    let mut check = mantle_crc::Crc32c::new();
    check.update(bytes.get(..CRC_AT)?);
    check.update(table_bytes);
    if check.finish() != crc {
        return None;
    }
    let mut t = Reader::new(table_bytes);
    let mut table = Vec::with_capacity(usize::try_from(count).ok()?);
    for _ in 0..count {
        table.push(t.u32()?);
    }
    Some(Prefix {
        header: RecordHeader {
            flags,
            checksum_shift,
            volume,
            segment,
            incarnation,
            sequence,
            key,
            chunk_offset,
            payload_len,
            time_ns,
        },
        table,
    })
}

/// Verifies the payload bytes `data`, which begin at checksum block `first` of the record,
/// against the table. Every block `data` covers must be whole, except the record's last.
pub fn verify(prefix: &Prefix, first: u32, data: &[u8]) -> bool {
    let Some(size) = 1u64
        .checked_shl(u32::from(prefix.header.checksum_shift))
        .and_then(|s| usize::try_from(s).ok())
    else {
        return false;
    };
    let first = usize::try_from(first).unwrap_or(usize::MAX);
    for (i, block) in data.chunks(size).enumerate() {
        let Some(expected) = first.checked_add(i).and_then(|j| prefix.table.get(j)) else {
            return false;
        };
        if mantle_crc::crc32c(block) != *expected {
            return false;
        }
    }
    true
}

/// The first block of every segment incarnation names the volume, segment and incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    pub volume: u128,
    pub segment: u32,
    pub incarnation: u64,
    pub time_ns: u64,
}

impl SegmentHeader {
    pub fn encode(&self, block: usize) -> Vec<u8> {
        let mut w = Writer::with_capacity(block);
        w.bytes(&SEGMENT_MAGIC);
        w.u128(self.volume);
        w.u32(self.segment);
        w.u64(self.incarnation);
        w.u64(self.time_ns);
        let crc = mantle_crc::crc32c(w.as_slice());
        w.u32(crc);
        w.zeros(block.saturating_sub(w.len()));
        w.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader::new(bytes);
        if r.take(4)? != SEGMENT_MAGIC {
            return None;
        }
        let header = Self {
            volume: r.u128()?,
            segment: r.u32()?,
            incarnation: r.u64()?,
            time_ns: r.u64()?,
        };
        let covered = r.position();
        let crc = r.u32()?;
        if mantle_crc::crc32c(bytes.get(..covered)?) == crc {
            Some(header)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn header(len: u32, shift: u8) -> RecordHeader {
        RecordHeader {
            flags: FLAG_FINAL,
            checksum_shift: shift,
            volume: 42,
            segment: 3,
            incarnation: 9,
            sequence: 1000,
            key: ChunkKey {
                block: 0xABCD,
                epoch: 2,
                index: 5,
            },
            chunk_offset: 0,
            payload_len: len,
            time_ns: 77,
        }
    }

    proptest! {
        #[test]
        fn records_round_trip_and_verify(payload in proptest::collection::vec(any::<u8>(), 0..20_000),
                                         shift in 9u8..=16) {
            let h = header(payload.len() as u32, shift);
            let mut out = Vec::new();
            let p = Payload::new(payload.clone(), shift).unwrap();
            prop_assert_eq!(p.crc, mantle_crc::crc32c(&payload));
            encode(&h, &p.table, &payload, &mut out).unwrap();
            prop_assert_eq!(out.len(), h.record_len().unwrap());
            prop_assert_eq!(out.len() % RECORD_ALIGN, 0);
            let prefix = decode_prefix(&out).unwrap();
            prop_assert_eq!(&prefix.header, &h);
            let start = h.prefix_len().unwrap();
            prop_assert!(verify(&prefix, 0, &out[start..start + payload.len()]));
        }

        #[test]
        fn a_flipped_payload_bit_fails_verification(payload in proptest::collection::vec(any::<u8>(), 1..5000),
                                                     at in any::<usize>(), bit in 0u8..8) {
            let h = header(payload.len() as u32, 10);
            let mut out = Vec::new();
            encode(&h, &Payload::new(payload.clone(), 10).unwrap().table, &payload, &mut out).unwrap();
            let prefix = decode_prefix(&out).unwrap();
            let start = h.prefix_len().unwrap();
            let mut data = out[start..start + payload.len()].to_vec();
            let i = at % data.len();
            data[i] ^= 1 << bit;
            prop_assert!(!verify(&prefix, 0, &data));
        }
    }

    #[test]
    fn lengths_can_be_read_from_the_header_alone() {
        let h = header(3000, 10);
        let mut out = Vec::new();
        let table = Payload::new(vec![7u8; 3000], 10).unwrap().table;
        encode(&h, &table, &[7u8; 3000], &mut out).unwrap();
        assert_eq!(
            peek_lengths(&out[..HEADER_LEN]),
            Some(Lengths {
                payload_len: 3000,
                checksum_shift: 10
            })
        );
    }

    #[test]
    fn a_flipped_header_bit_makes_the_record_absent() {
        let h = header(3000, 10);
        let mut out = Vec::new();
        let table = Payload::new(vec![7u8; 3000], 10).unwrap().table;
        encode(&h, &table, &[7u8; 3000], &mut out).unwrap();
        for i in 0..h.prefix_len().unwrap() {
            let mut copy = out.clone();
            copy[i] ^= 0x10;
            assert_eq!(decode_prefix(&copy), None, "byte {i}");
        }
    }

    #[test]
    fn a_partial_range_verifies_from_its_first_block() {
        let payload: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let h = header(5000, 10);
        let mut out = Vec::new();
        let table = Payload::new(payload.clone(), 10).unwrap().table;
        encode(&h, &table, &payload, &mut out).unwrap();
        let prefix = decode_prefix(&out).unwrap();
        let start = h.prefix_len().unwrap();
        // Blocks 2..5 (bytes 2048..5000), the last one short.
        assert!(verify(&prefix, 2, &out[start + 2048..start + 5000]));
        assert!(!verify(&prefix, 1, &out[start + 2048..start + 5000]));
    }

    #[test]
    fn segment_headers_round_trip_and_reject_damage() {
        let s = SegmentHeader {
            volume: 1,
            segment: 2,
            incarnation: 3,
            time_ns: 4,
        };
        let mut bytes = s.encode(4096);
        assert_eq!(bytes.len(), 4096);
        assert_eq!(SegmentHeader::decode(&bytes), Some(s));
        bytes[20] ^= 1;
        assert_eq!(SegmentHeader::decode(&bytes), None);
    }
}
