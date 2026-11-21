//! Volume geometry and configuration.

use hyper_block::buf::{Alignment, MAX_BUFFER};

use crate::error::ChunkError;
use crate::frame::{DELETE_LEN, FRAME_HEADER, PUT_LEN, SEGMENT_LEN};
use crate::superblock::{OFFSET_B_COMPACT, OFFSET_B_STANDARD, Superblock};

/// The largest index frame, and so the most memory reading or writing one frame takes. It is
/// one bound for every frame (audit S13): recovery reads no larger frame, a checkpoint packs
/// its records into frames within it, `Config::check` refuses limits whose largest batch
/// frame would pass it, and the writer refuses to write a frame past it. A frame is padded
/// to the volume's block, and every block divides it (`Geometry::plan`), so a frame whose
/// header and records fit it stays within it once padded.
pub const MAX_FRAME_BYTES: u64 = 4 << 20;
/// Bytes of records one frame holds.
pub const FRAME_PAYLOAD: u64 = MAX_FRAME_BYTES - FRAME_HEADER as u64;

/// The block of a volume on a device whose transfers align to `align`: the larger of 4 KiB
/// [HL23 §2.2] and the alignment, the device's logical and physical blocks and its write unit
/// (docs/design/chunk-store.md §2).
pub fn block(align: Alignment) -> Option<u64> {
    let least = Alignment::new(4096).ok()?;
    u64::try_from(align.max(least).get()).ok()
}

/// The largest payload one record of a segment of `segment_size` bytes holds on a device of
/// `block`-byte blocks: the segment less its header block, the record's header, its checksum
/// table at `checksum_shift` and its padding, at most what a record's length field holds.
pub fn record_room(segment_size: u64, block: u64, checksum_shift: u8) -> u64 {
    let room = segment_size.saturating_sub(block);
    let table = room
        .checked_shr(u32::from(checksum_shift))
        .unwrap_or(0)
        .saturating_add(1)
        .saturating_mul(4);
    room.saturating_sub(crate::record::HEADER_LEN as u64)
        .saturating_sub(table)
        .saturating_sub(crate::record::RECORD_ALIGN as u64)
        .min(u64::from(u32::MAX))
}

/// The least segment, a multiple of `block` and four of them at least, one record of which holds
/// a payload of `payload` bytes ([`record_room`]); `None` past what one I/O buffer reads, the
/// most a segment may be ([`Layout::plan`]).
pub fn least_segment(block: u64, checksum_shift: u8, payload: u64) -> Option<u64> {
    let most = u64::try_from(MAX_BUFFER).ok()?.min(u64::from(u32::MAX));
    let fixed =
        (crate::record::HEADER_LEN as u64).checked_add(crate::record::RECORD_ALIGN as u64)?;
    // A segment of no less than the payload, its record's fixed bytes and the header block, then
    // a block more at a time while its table, a word a checksum block, leaves the payload out.
    let mut segment = payload
        .checked_add(fixed)?
        .checked_add(block)?
        .checked_next_multiple_of(block)?
        .max(block.checked_mul(4)?);
    while record_room(segment, block, checksum_shift) < payload {
        segment = segment.checked_add(block)?;
        if segment > most {
            return None;
        }
    }
    if segment <= most { Some(segment) } else { None }
}

/// Limits on the writer's queue and batches. Each bounds memory, not throughput: a batch
/// is everything that arrived while the previous flush ran, up to these limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Requests in one group commit.
    pub batch_requests: usize,
    /// Payload bytes in one group commit.
    pub batch_bytes: usize,
    /// Fragments one appended chunk may have before it must be sealed.
    pub fragments_per_chunk: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            batch_requests: 1024,
            batch_bytes: 32 << 20,
            fragments_per_chunk: 4096,
        }
    }
}

impl Limits {
    /// Client requests the writer's queue holds: the next batch while one is written, and one
    /// more to absorb a burst. By Little's law a longer queue only adds waiting, since the
    /// writer takes no more than a batch at a time: at this bound a request waits about two
    /// batches at most (docs/research/11 §4).
    pub fn queue_requests(&self) -> usize {
        self.batch_requests.saturating_mul(2)
    }

    /// Payload bytes the writer's queue holds, by the same rule.
    pub fn queue_bytes(&self) -> u64 {
        u64::try_from(self.batch_bytes)
            .unwrap_or(u64::MAX)
            .saturating_mul(2)
    }
}

/// How many client reads the volume holds at the device, and how many more may wait for a
/// turn (docs/design/chunk-store.md §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reads {
    /// Reads at the device at once: the depth where calibration finds throughput stops
    /// growing (`mantle_disk::calibrate::Calibration::random_read_saturation`). Past it every
    /// read added only waits, so holding the device there costs no throughput
    /// (docs/measurements/2026-09-29-read-depth.md).
    pub depth: usize,
    /// Reads that may wait for a turn; a read past them is refused with `Busy`, and the caller
    /// reads another copy.
    pub waiting: usize,
    /// Bytes the reads at the device may buffer at once: the bytes in flight where calibration
    /// finds large sequential reads stop gaining throughput
    /// (`mantle_disk::calibrate::Calibration::sequential_read_saturation`), past which bytes
    /// added only wait, as reads added past `depth` do. A read that buffers more alone is taken
    /// when no other read is at the device, as the writer's queue takes a payload larger than
    /// its bound, so the reads' buffers never exceed this or one read's (audit S07).
    pub bytes: u64,
    /// Bytes of payload a read goes past rather than ask the device a second time: what the
    /// device reads sequentially in the time one small random read takes
    /// (`mantle_disk::calibrate::Calibration::read_gap`). A range that starts further into a
    /// record's payload is read apart from the record's header and checksum table (audit P01).
    pub gap: u64,
}

impl Reads {
    /// The measured depth, bytes and gap, with as many reads let wait: by Little's law each
    /// then waits about as long as a read takes at that depth, beyond which another copy
    /// serves it sooner.
    pub fn measured(depth: usize, bytes: u64, gap: u64) -> Self {
        let depth = depth.max(1);
        Self {
            depth,
            waiting: depth,
            bytes,
            gap,
        }
    }
}

impl Default for Reads {
    /// A device not measured reads one at a time, each read alone: no device's depth of
    /// greatest power is below one. It reads no payload it was not asked for.
    fn default() -> Self {
        Self::measured(1, 0, 0)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Bytes per segment, fixed at format. 256 MiB is the zone size of host-managed SMR
    /// drives and in the range of ZNS zone capacities, though the volume has no zone backend
    /// and a host-managed device is refused (docs/design/chunk-store.md §2).
    pub segment_size: u64,
    /// log2 of the checksum block size of data records; 16 is 64 KiB (Ghemawat et al.,
    /// SOSP 2003, §5.2).
    pub checksum_shift: u8,
    /// The most fragments the volume indexes. It bounds index memory and sizes the log at
    /// format so three full checkpoints fit.
    pub max_fragments: u64,
    /// Place superblock B and the log near the start: for small volumes and tests.
    pub compact: bool,
    /// How long one background scrub of the whole volume takes; `None` scrubs only when
    /// asked. Seven days is the target the literature supports, fourteen the most it allows
    /// (docs/research/03 §15.9).
    pub scrub_period: Option<std::time::Duration>,
    pub limits: Limits,
    /// Write the whole volume once with zeros at format, before first use, so appends
    /// overwrite written blocks: set where calibration measures a first-write penalty
    /// (`mantle_disk::calibrate::Calibration::first_write_penalty`; docs/design/chunk-store.md
    /// §2).
    pub prewrite: bool,
    pub reads: Reads,
}

impl Config {
    /// Refuses settings no volume can run with, before any I/O or thread (audit S08): a
    /// read gate of no depth admits no read and parks every one for good, and a batch, queue
    /// or chunk of no room takes nothing.
    pub fn check(&self) -> Result<(), ChunkError> {
        let bad = |m: &str| Err(ChunkError::Config(m.to_owned()));
        if self.reads.depth == 0 {
            return bad("a read depth of none admits no read");
        }
        if self.limits.batch_requests == 0 || self.limits.batch_bytes == 0 {
            return bad("a batch of no requests or no bytes takes nothing");
        }
        if self.limits.fragments_per_chunk == 0 {
            return bad("a chunk of no fragments holds nothing");
        }
        if self.max_fragments == 0 {
            return bad("a volume of no fragments indexes nothing");
        }
        if !(9..=24).contains(&self.checksum_shift) {
            return bad("checksum block must be between 512 B and 16 MiB");
        }
        // A batch is one frame, which recovery must read back (audit S13). The cleaner moves
        // as many fragments in one relocation as a batch takes requests (`Volume::start`).
        let largest = batch_frame_payload(self.limits.batch_requests, self.limits.batch_requests);
        if largest.is_none_or(|bytes| bytes > FRAME_PAYLOAD) {
            return bad(
                "a batch of this many requests could make an index frame larger than recovery reads",
            );
        }
        Ok(())
    }

    /// The chunk exchanges a volume of this configuration takes at once: its writer's queue
    /// (`Limits::queue_requests`) and its reads at the device and waiting, past which it refuses
    /// `Busy`.
    pub fn transfers(&self) -> usize {
        self.limits
            .queue_requests()
            .saturating_add(self.reads.depth)
            .saturating_add(self.reads.waiting)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            segment_size: 256 << 20,
            checksum_shift: 16,
            max_fragments: 1 << 22,
            compact: false,
            scrub_period: Some(std::time::Duration::from_secs(7 * 24 * 3600)),
            limits: Limits::default(),
            prewrite: false,
            reads: Reads::default(),
        }
    }
}

/// Where everything is in a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub block: u64,
    pub segment_size: u64,
    pub segments: u32,
    pub offset_b: u64,
    pub log_offset: u64,
    pub log_size: u64,
    pub data_offset: u64,
}

impl Geometry {
    /// Where everything is in the volume `superblock` describes, as it was laid out.
    pub fn of(superblock: &Superblock) -> Self {
        Self {
            block: u64::from(superblock.block),
            segment_size: superblock.segment_size,
            segments: superblock.segments,
            offset_b: superblock.offset_b,
            log_offset: superblock.log_offset,
            log_size: superblock.log_size,
            data_offset: superblock.data_offset,
        }
    }

    /// Lays out a volume of `size` bytes whose transfers align to `align`.
    pub fn plan(size: u64, align: Alignment, config: &Config) -> Result<Self, ChunkError> {
        let bad = |m: &str| ChunkError::Config(m.to_owned());
        let block = block(align).ok_or_else(|| bad("block"))?;
        if !config.segment_size.is_multiple_of(block)
            || config.segment_size < block.saturating_mul(4)
        {
            return Err(bad(
                "segment size must be a multiple of the block, at least four blocks",
            ));
        }
        if config.segment_size > u64::from(u32::MAX) {
            return Err(bad("segment size must be below 4 GiB"));
        }
        // A record may fill its segment, and is written and read in one buffer.
        if config.segment_size > u64::try_from(MAX_BUFFER).unwrap_or(u64::MAX) {
            return Err(bad(
                "segment size must fit one I/O buffer (hyper_block::buf::MAX_BUFFER)",
            ));
        }
        if !(9..=24).contains(&config.checksum_shift) {
            return Err(bad("checksum block must be between 512 B and 16 MiB"));
        }
        let (offset_b, log_offset, min_log) = if config.compact {
            (
                OFFSET_B_COMPACT,
                OFFSET_B_COMPACT.saturating_mul(2),
                1u64 << 20,
            )
        } else {
            (
                OFFSET_B_STANDARD,
                OFFSET_B_STANDARD.saturating_mul(2),
                64u64 << 20,
            )
        };
        if !offset_b.is_multiple_of(block) {
            return Err(bad("block too large for the superblock layout"));
        }
        if block > MAX_FRAME_BYTES {
            return Err(bad("block larger than an index frame"));
        }
        let segments_upper = size.checked_div(config.segment_size).unwrap_or(0);
        let log_size = log_bytes(config, segments_upper, block)
            .map(|c| c.max(min_log))
            .and_then(|c| c.checked_next_multiple_of(block))
            .ok_or_else(|| bad("log too large"))?;
        let data_offset = log_offset
            .checked_add(log_size)
            .and_then(|e| e.checked_next_multiple_of(block))
            .ok_or_else(|| bad("volume too large"))?;
        let segments = size
            .checked_sub(data_offset)
            .and_then(|d| d.checked_div(config.segment_size))
            .ok_or_else(|| bad("volume too small for its log"))?;
        if segments < 2 {
            return Err(bad(
                "volume too small: it needs at least two segments after the log",
            ));
        }
        Ok(Self {
            block,
            segment_size: config.segment_size,
            segments: u32::try_from(segments).map_err(|_| bad("too many segments"))?,
            offset_b,
            log_offset,
            log_size,
            data_offset,
        })
    }

    /// The byte just past the last segment: the volume's extent.
    pub fn end(&self) -> Option<u64> {
        self.segment_offset(self.segments)
    }

    /// The byte offset of a segment.
    pub fn segment_offset(&self, segment: u32) -> Option<u64> {
        u64::from(segment)
            .checked_mul(self.segment_size)
            .and_then(|o| o.checked_add(self.data_offset))
    }

    pub fn block_usize(&self) -> usize {
        usize::try_from(self.block).unwrap_or(4096)
    }

    /// Refuses, when a volume is opened, limits its log was not sized for at format (audit
    /// S13): a batch or checkpoint larger than the log leaves room for would fill it with
    /// nothing able to free it.
    pub fn holds(&self, config: &Config) -> Result<(), ChunkError> {
        let bad = |m: &str| Err(ChunkError::Config(m.to_owned()));
        if self.block > MAX_FRAME_BYTES {
            return bad("block larger than an index frame");
        }
        match log_bytes(config, u64::from(self.segments), self.block) {
            Some(need) if need <= self.log_size => Ok(()),
            _ => {
                bad("the index log, sized at format, cannot hold a batch or checkpoint this large")
            }
        }
    }
}

/// The most bytes a volume of `config`, laid out as `geometry`, holds beside its index (whose
/// bytes `index::most_bytes` states): each owner's free buffers at its pool's limit, and what it
/// holds besides, the largest record a segment holds standing for a record of any size.
///
/// - The writer's queue, two batches of payloads with their checksum tables; and the cleaner's
///   one relocation, a batch and the record that crossed it, gathered or queued.
/// - The writer: two batches' buffers kept free; the batch it writes, a relocation's at most,
///   encoded into its buffers, and its frame; and a batch's reads kept free and a record read.
/// - The cleaner, and the scrubber where a period is set: a batch's reads kept free and a record
///   read.
/// - The volume's reader: a batch kept free, and its reads at the device, its measured bytes or
///   one record alone.
/// - The larger of a checkpoint's largest frame (`checkpoint_frame_bytes`), encoded to measure it
///   and to write it and copied to its alignment, and recovery's window on the log.
/// - Each segment's state, as the writer, the volume's usage and the index keep it.
pub fn most_bytes(config: &Config, geometry: &Geometry) -> Option<u64> {
    let batch = u64::try_from(config.limits.batch_bytes).ok()?;
    let record = geometry.segment_size.checked_sub(geometry.block)?;
    let table = |len: u64| {
        len.checked_shr(u32::from(config.checksum_shift))?
            .checked_add(1)?
            .checked_mul(4)
    };
    let queued = batch
        .checked_mul(2)?
        .checked_add(table(batch)?.checked_mul(2)?)?;
    let relocation = batch.checked_add(record)?.checked_add(table(batch)?)?;
    let frame = batch_frame_bytes(
        config.limits.batch_requests,
        config.limits.batch_requests,
        geometry.block,
    )?;
    let reading = batch.checked_add(record)?;
    let writer = batch
        .checked_mul(2)?
        .checked_add(relocation)?
        .checked_add(frame)?
        .checked_add(reading)?;
    let scrubber = if config.scrub_period.is_some() {
        reading
    } else {
        0
    };
    let reader = batch.checked_add(config.reads.bytes.max(record))?;
    let checkpoint = checkpoint_frame_bytes(
        config.max_fragments,
        u64::from(geometry.segments),
        geometry.block,
    )?
    .checked_mul(3)?
    .max(MAX_FRAME_BYTES.min(geometry.log_size));
    let states = u64::try_from(std::mem::size_of::<crate::index::SegmentInfo>())
        .ok()?
        .checked_mul(u64::from(geometry.segments))?
        .checked_mul(3)?;
    queued
        .checked_add(relocation)?
        .checked_add(writer)?
        .checked_add(reading)?
        .checked_add(scrubber)?
        .checked_add(reader)?
        .checked_add(checkpoint)?
        .checked_add(states)
}

/// Bytes the log needs: three checkpoints of the full fragment budget, one batch frame, and
/// the wraps before two checkpoints: the one being written and the one before it, whose
/// skipped tail stays in the live log until the next. With it, a full index leaves a
/// checkpoint's worth of frames between checkpoints (docs/design/chunk-store.md §5).
pub fn log_bytes(config: &Config, segments: u64, block: u64) -> Option<u64> {
    let checkpoint = checkpoint_bytes(config.max_fragments, segments, block)?;
    let batch = batch_frame_bytes(
        config.limits.batch_requests,
        config.limits.batch_requests,
        block,
    )?;
    let largest = largest_frame(config, segments, block)?;
    checkpoint
        .checked_mul(3)?
        .checked_add(batch)?
        .checked_add(largest.checked_mul(2)?)
}

/// Bytes of records the largest batch frame holds, whatever its requests. A request's worst
/// is a record placed with a seal of the full segment before it and an open of the next, and
/// one empty segment the batch frees (the writer frees at most one per request); a delete's
/// record is shorter than a put's and places nothing. At most one request is a relocation,
/// since one cleaning pass runs at a time and waits for each relocation it sends, and each
/// of its `moves` is placed as a put is.
pub fn batch_frame_payload(requests: usize, moves: usize) -> Option<u64> {
    let placed = PUT_LEN
        .max(DELETE_LEN)
        .checked_add(SEGMENT_LEN.checked_mul(2)?)?;
    let request = u64::try_from(placed.checked_add(SEGMENT_LEN)?).ok()?;
    let requests = u64::try_from(requests.max(1)).ok()?;
    let moves = u64::try_from(moves).ok()?;
    requests
        .checked_mul(request)?
        .checked_add(moves.checked_mul(u64::try_from(placed).ok()?)?)
}

/// Bytes of the largest batch frame, padded to the block.
pub fn batch_frame_bytes(requests: usize, moves: usize, block: u64) -> Option<u64> {
    (FRAME_HEADER as u64)
        .checked_add(batch_frame_payload(requests, moves)?)?
        .checked_next_multiple_of(block)
}

/// The largest frame the log holds, and so the most a wrap skips at the end of the log region:
/// a wrap happens when the next frame does not fit in what remains.
pub fn largest_frame(config: &Config, segments: u64, block: u64) -> Option<u64> {
    let batch = batch_frame_bytes(
        config.limits.batch_requests,
        config.limits.batch_requests,
        block,
    )?;
    let checkpoint = checkpoint_frame_bytes(config.max_fragments, segments, block)?;
    Some(batch.max(checkpoint))
}

/// Bytes of the largest checkpoint frame: all the checkpoint's records, or a full frame.
pub fn checkpoint_frame_bytes(fragments: u64, segments: u64, block: u64) -> Option<u64> {
    let whole = (FRAME_HEADER as u64)
        .checked_add(checkpoint_payload(fragments, segments)?)?
        .checked_next_multiple_of(block)?;
    Some(whole.min(MAX_FRAME_BYTES))
}

/// Bytes of the records a checkpoint of `fragments` fragments and `segments` segments writes.
fn checkpoint_payload(fragments: u64, segments: u64) -> Option<u64> {
    fragments
        .checked_mul(PUT_LEN as u64)?
        .checked_add(segments.checked_mul(SEGMENT_LEN as u64)?)
}

/// Bytes a checkpoint of `fragments` fragments and `segments` segments takes in the log. Its
/// records are packed into frames in order, a frame closing only when the next record does
/// not fit, so every frame but the last holds more than `FRAME_PAYLOAD - PUT_LEN` bytes of
/// them; an end frame follows. Each frame adds its header and at most a block of padding.
pub fn checkpoint_bytes(fragments: u64, segments: u64, block: u64) -> Option<u64> {
    let payload = checkpoint_payload(fragments, segments)?;
    let frames = payload
        .div_ceil(FRAME_PAYLOAD.checked_sub(PUT_LEN as u64)?)
        .checked_add(2)?;
    payload.checked_add(frames.checked_mul((FRAME_HEADER as u64).checked_add(block)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The least segment holding a payload holds it, the segment a block smaller does not, and
    /// an 8 MiB chunk takes the segment its record's header and table add to it on a 4 KiB
    /// device: 8,404,992 bytes at 4 KiB checksum blocks, 8,396,800 at 64 KiB.
    #[test]
    fn the_least_segment_holds_its_payload_and_a_block_less_does_not() {
        let mib8 = 8 << 20;
        assert_eq!(least_segment(4096, 12, mib8), Some(8_404_992));
        assert_eq!(least_segment(4096, 16, mib8), Some(8_396_800));
        for (block, shift, payload) in [(4096, 12, 1), (4096, 12, mib8), (16_384, 14, 300_000)] {
            let segment = least_segment(block, shift, payload).unwrap();
            assert_eq!(segment % block, 0);
            assert!(record_room(segment, block, shift) >= payload);
            if segment - block >= 4 * block {
                assert!(record_room(segment - block, block, shift) < payload);
            }
        }
        assert_eq!(least_segment(4096, 12, 1 << 32), None);
    }

    #[test]
    fn a_compact_volume_fits_its_log_and_segments() {
        let config = Config {
            segment_size: 1 << 20,
            max_fragments: 1000,
            compact: true,
            ..Config::default()
        };
        let g = Geometry::plan(16 << 20, Alignment::new(4096).unwrap(), &config).unwrap();
        assert!(g.segments >= 2);
        assert!(g.data_offset >= g.log_offset + g.log_size);
        assert!(g.segment_offset(g.segments - 1).unwrap() + g.segment_size <= 16 << 20);
        assert!(g.log_size >= 3 * checkpoint_bytes(1000, u64::from(g.segments), 4096).unwrap());
    }

    #[test]
    fn nonsense_is_refused() {
        let a = Alignment::new(4096).unwrap();
        let mut c = Config {
            compact: true,
            ..Config::default()
        };
        c.segment_size = 1000;
        assert!(Geometry::plan(1 << 30, a, &c).is_err());
        let c = Config {
            compact: true,
            segment_size: 1 << 20,
            ..Config::default()
        };
        assert!(Geometry::plan(1 << 20, a, &c).is_err(), "too small");
        // A segment one block past a buffer's bound, on a volume with room for many.
        let c = Config {
            segment_size: MAX_BUFFER as u64 + 4096,
            ..Config::default()
        };
        assert!(Geometry::plan(1 << 40, a, &c).is_err(), "past one buffer");
        let c = Config {
            segment_size: MAX_BUFFER as u64,
            ..Config::default()
        };
        assert!(Geometry::plan(1 << 40, a, &c).is_ok(), "one buffer");
    }
}
