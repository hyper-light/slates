//! The volume's superblock: identity, geometry, and where recovery starts.
//!
//! Two copies sit at fixed offsets far apart, because latent sector errors cluster within
//! about 10 MB (Bairavasundaram et al., SIGMETRICS 2007, §5), and are written alternately
//! with an increasing sequence; recovery takes the valid copy with the higher sequence, as
//! LFS takes the newer of its two checkpoint regions (Rosenblum and Ousterhout, TOCS 1992,
//! §4.1). Each copy is one block ending in its CRC-32C.

use mantle_codec::{Reader, Writer};

pub const MAGIC: [u8; 8] = *b"MNTLVOL1";
pub const VERSION: u32 = 3;

/// Superblock A's offset.
pub const OFFSET_A: u64 = 0;
/// Superblock B's offset in a standard volume: 16 MiB from A.
pub const OFFSET_B_STANDARD: u64 = 16 << 20;
/// Superblock B's offset in a compact volume (small test volumes), still clear of A.
pub const OFFSET_B_COMPACT: u64 = 64 << 10;

/// The fields of one superblock copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub block: u32,
    pub volume: u128,
    pub sequence: u64,
    pub created_ns: u64,
    pub segment_size: u64,
    pub segments: u32,
    /// log2 of the checksum block size of data records.
    pub checksum_shift: u8,
    pub offset_b: u64,
    pub log_offset: u64,
    pub log_size: u64,
    pub data_offset: u64,
    /// The LSN of the frame recovery starts from; frames before it are free space.
    pub start_lsn: u64,
    /// That frame's byte position within the log region.
    pub start_pos: u64,
    /// The LSN after the checkpoint's last frame. The superblock is written only once the
    /// whole checkpoint is durable, so a replay that stops short of it met damage, not a torn
    /// tail: the checkpoint's frames share one flush group, and no later group need follow
    /// them to show it.
    pub end_lsn: u64,
    /// Every record sequence and segment incarnation issued so far is at most these. They
    /// are raised here before any record uses a higher number, and recovery resumes above
    /// them, so no number that might still be on the device is issued twice.
    pub sequence_limit: u64,
    pub incarnation_limit: u64,
}

impl Superblock {
    /// Encodes into exactly `block` bytes, the last four being the CRC-32C of the rest.
    pub fn encode(&self) -> Vec<u8> {
        let size = usize::try_from(self.block).unwrap_or(4096);
        let mut w = Writer::with_capacity(size);
        w.bytes(&MAGIC);
        w.u32(VERSION);
        w.u32(self.block);
        w.u128(self.volume);
        w.u64(self.sequence);
        w.u64(self.created_ns);
        w.u64(self.segment_size);
        w.u32(self.segments);
        w.u8(self.checksum_shift);
        w.zeros(3);
        w.u64(self.offset_b);
        w.u64(self.log_offset);
        w.u64(self.log_size);
        w.u64(self.data_offset);
        w.u64(self.start_lsn);
        w.u64(self.start_pos);
        w.u64(self.end_lsn);
        w.u64(self.sequence_limit);
        w.u64(self.incarnation_limit);
        let body = size.saturating_sub(4);
        w.zeros(body.saturating_sub(w.len()));
        let crc = mantle_crc::crc32c(w.as_slice());
        w.u32(crc);
        w.into_vec()
    }

    /// Decodes one copy; `None` for anything that is not a valid, intact superblock.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let body_len = bytes.len().checked_sub(4)?;
        let (body, crc) = bytes.split_at(body_len);
        let crc = u32::from_le_bytes(crc.try_into().ok()?);
        if mantle_crc::crc32c(body) != crc {
            return None;
        }
        let mut r = Reader::new(body);
        if r.take(8)? != MAGIC || r.u32()? != VERSION {
            return None;
        }
        let block = r.u32()?;
        if usize::try_from(block).ok()? != bytes.len() {
            return None;
        }
        let sb = Self {
            block,
            volume: r.u128()?,
            sequence: r.u64()?,
            created_ns: r.u64()?,
            segment_size: r.u64()?,
            segments: r.u32()?,
            checksum_shift: {
                let s = r.u8()?;
                r.take(3)?;
                s
            },
            offset_b: r.u64()?,
            log_offset: r.u64()?,
            log_size: r.u64()?,
            data_offset: r.u64()?,
            start_lsn: r.u64()?,
            start_pos: r.u64()?,
            end_lsn: r.u64()?,
            sequence_limit: r.u64()?,
            incarnation_limit: r.u64()?,
        };
        Some(sb)
    }

    /// The offset of copy `which` (0 = A, 1 = B).
    pub fn offset_of(&self, which: u8) -> u64 {
        if which == 0 { OFFSET_A } else { self.offset_b }
    }

    /// Superblock sequences alternate copies: even sequences go to A, odd to B.
    pub fn slot(&self) -> u8 {
        u8::try_from(self.sequence & 1).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Superblock {
        Superblock {
            block: 4096,
            volume: 0x1234,
            sequence: 7,
            created_ns: 99,
            segment_size: 1 << 20,
            segments: 16,
            checksum_shift: 16,
            offset_b: OFFSET_B_COMPACT,
            log_offset: 128 << 10,
            log_size: 1 << 20,
            data_offset: 2 << 20,
            start_lsn: 1,
            start_pos: 0,
            end_lsn: 3,
            sequence_limit: 1 << 24,
            incarnation_limit: 1 << 16,
        }
    }

    #[test]
    fn round_trips_and_fills_one_block() {
        let sb = sample();
        let bytes = sb.encode();
        assert_eq!(bytes.len(), 4096);
        assert_eq!(Superblock::decode(&bytes), Some(sb));
    }

    #[test]
    fn any_flipped_bit_invalidates_it() {
        let bytes = sample().encode();
        for i in [0usize, 8, 40, 100, 4000, 4095] {
            for bit in 0..8 {
                let mut copy = bytes.clone();
                copy[i] ^= 1 << bit;
                assert_eq!(Superblock::decode(&copy), None, "byte {i} bit {bit}");
            }
        }
    }

    #[test]
    fn a_block_of_another_size_is_refused() {
        let mut sb = sample();
        sb.block = 8192;
        let mut bytes = sb.encode();
        bytes.truncate(4096);
        assert_eq!(Superblock::decode(&bytes), None);
    }
}
