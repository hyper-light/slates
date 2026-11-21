//! Opening a volume after a clean shutdown or a crash (docs/design/chunk-store.md §6).
//!
//! Recovery replays the index log from the checkpoint the superblock names, frame by frame
//! in LSN order, to the first frame that is not there. It then decides whether that frame is
//! the torn tail of a crash or damage to acknowledged state: if it lies within the checkpoint,
//! whose end the superblock records, or any valid frame of a later flush group exists anywhere
//! in the log, the log was damaged, which a crash cannot cause, and the volume refuses to open
//! (Alagappan et al., FAST 2018, §3.3.3). The records of the last
//! replayed batch are read back, because that batch's flush may not have completed and its
//! frame can reach the disk before its data. Finally it rolls forward through each open
//! segment past the last indexed record, adding every record that verifies (Rosenblum and
//! Ousterhout, TOCS 1992, §4.2): a batch whose data reached the disk but whose frame did not.

use hyper_block::DiskError;
use hyper_block::block::BlockFile;
use hyper_block::buf::{AlignedBuf, Pool};

use crate::error::ChunkError;
use crate::frame::{KIND_BATCH, KIND_WRAP, LogRecord, PutRecord, SegmentState};
use crate::index::{Fragment, Index, Inserted, SegmentInfo};
use crate::key::ChunkKey;
use crate::layout::{Config, Geometry};
use crate::log::{Cursor, Frames};
use crate::record::{self, Prefix};
use crate::superblock::Superblock;
use crate::writer::distance;

/// What recovery found, for the operator and for tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Index frames replayed.
    pub frames: u64,
    /// Chunks whose record in the last batch did not verify. Its flush may never have
    /// completed, or its data may have been damaged since it was acknowledged: the volume
    /// cannot tell the two apart, so each is kept, reads of it answer that its bytes do not
    /// verify, and repair or the reconciler decides (AGL+18 §3.3.3; audit S05).
    pub damaged: Vec<ChunkKey>,
    /// Relocations of the last batch whose new copy did not verify, put back to the copy they
    /// moved, which is intact whether or not the batch was acknowledged.
    pub restored: u64,
    /// Records found past the index log's end in open segments and indexed.
    pub rolled_forward: u64,
}

pub(crate) struct Recovered {
    pub index: Index,
    pub segments: Vec<SegmentInfo>,
    pub cursor: Cursor,
    pub sequence: u64,
    pub incarnation: u64,
    pub fragments: u64,
    /// Open segments, newest first: the first three continue the writer's three streams, the
    /// rest are sealed when the volume starts.
    pub open: Vec<u32>,
    pub report: RecoveryReport,
}

pub(crate) fn recover<F: BlockFile>(
    file: &F,
    pool: &mut ReadPool,
    sb: &Superblock,
    geometry: &Geometry,
    config: &Config,
) -> Result<Recovered, ChunkError> {
    let per_chunk = config.limits.fragments_per_chunk;
    let mut index = Index::default();
    let segment_count = usize::try_from(geometry.segments).map_err(|_| ChunkError::Full)?;
    let mut segments = vec![SegmentInfo::FREE; segment_count];
    let mut report = RecoveryReport::default();
    // Every number the device might hold is within the superblock's reservations.
    let mut sequence = sb.sequence_limit;
    let mut incarnation = sb.incarnation_limit;

    // Replay.
    let mut pos = sb.start_pos;
    let mut lsn = sb.start_lsn;
    // The last batch's Puts, each with the fragment it displaced when it was a relocation.
    let mut last_batch: Vec<(PutRecord, Option<Fragment>)> = Vec::new();
    let max_frames = geometry
        .log_size
        .checked_div(geometry.block)
        .unwrap_or(0)
        .saturating_add(2);
    let mut frames = 0u64;
    let mut log = Frames::new(file, geometry, sb.volume).map_err(ChunkError::Device)?;
    loop {
        if frames > max_frames {
            return Err(ChunkError::CorruptLog { lsn });
        }
        let Some((header, records, len)) = log.frame(pos).map_err(ChunkError::Device)? else {
            break;
        };
        if header.lsn != lsn {
            break;
        }
        frames = frames.saturating_add(1);
        lsn = lsn.saturating_add(1);
        if header.kind == KIND_WRAP {
            pos = 0;
            continue;
        }
        if header.kind == KIND_BATCH {
            last_batch.clear();
        }
        for record in &records {
            match record {
                LogRecord::Put(p) => {
                    let outcome = index
                        .insert(p.key, p, per_chunk)
                        .map_err(|_| ChunkError::CorruptLog { lsn: header.lsn })?;
                    sequence = sequence.max(p.sequence);
                    if header.kind == KIND_BATCH {
                        let displaced = match outcome {
                            Inserted::Replaced(old) => Some(old),
                            Inserted::Added | Inserted::Unchanged => None,
                        };
                        last_batch.push((*p, displaced));
                    }
                    // The segment's high-water mark counts every record the log placed there,
                    // including ones a later delete removes from the index: roll-forward must
                    // start past all of them, or it would bring deleted chunks back.
                    if let Some(info) =
                        segments.get_mut(usize::try_from(p.segment).unwrap_or(usize::MAX))
                        && info.incarnation == p.incarnation
                    {
                        let end = u64::from(p.offset).saturating_add(u64::from(p.record_len));
                        let end = end.checked_next_multiple_of(geometry.block).unwrap_or(end);
                        info.write_pos = info.write_pos.max(u32::try_from(end).unwrap_or(u32::MAX));
                    }
                }
                LogRecord::Delete(d) => {
                    index.remove(&d.key);
                    sequence = sequence.max(d.sequence);
                }
                LogRecord::Segment(s) => {
                    let slot = segments
                        .get_mut(usize::try_from(s.segment).unwrap_or(usize::MAX))
                        .ok_or(ChunkError::CorruptLog { lsn: header.lsn })?;
                    *slot = SegmentInfo {
                        state: s.state,
                        incarnation: s.incarnation,
                        write_pos: s.write_pos,
                        live: 0,
                        youngest_ns: 0,
                    };
                    incarnation = incarnation.max(s.incarnation);
                }
            }
        }
        pos = pos.saturating_add(len);
        if pos >= geometry.log_size {
            pos = 0;
        }
    }
    report.frames = frames;
    if lsn < sb.end_lsn || later_frame_exists(&mut log, geometry, lsn)? {
        return Err(ChunkError::CorruptLog { lsn });
    }

    // The last batch's flush may not have completed, and a record whose data does not
    // verify may be torn. It may as well be an acknowledged record damaged since, or read
    // wrong: nothing after the batch tells, as a later frame would for an earlier one. So a
    // record is never dropped on that evidence (audit S05). A relocation whose new copy does
    // not verify goes back to the copy it moved, which is intact either way: that segment is
    // freed only after a relocation is durable. Any other record stays, reported damaged.
    for (put, displaced) in last_batch.iter().rev() {
        let at = u64::from(put.offset);
        let verified = verify_at(
            file,
            pool,
            geometry,
            Identity::of(sb),
            put.segment,
            at,
            None,
        )?
        .is_some_and(|v| v.matches(put));
        if verified {
            continue;
        }
        match displaced {
            Some(old) => {
                if index.restore_fragment(&put.key, old) {
                    report.restored = report.restored.saturating_add(1);
                }
            }
            None => report.damaged.push(put.key),
        }
    }

    // Roll forward through open segments.
    let mut open = Vec::new();
    for (i, info) in segments.iter_mut().enumerate() {
        if info.state != SegmentState::Open {
            continue;
        }
        let segment = u32::try_from(i).map_err(|_| ChunkError::Full)?;
        open.push((info.incarnation, segment));
        let mut at = u64::from(info.write_pos).max(geometry.block);
        while at < geometry.segment_size {
            let Some(found) = verify_at(file, pool, geometry, Identity::of(sb), segment, at, None)?
            else {
                break;
            };
            if found.prefix.header.incarnation != info.incarnation {
                break;
            }
            let put = found.as_put(segment, at);
            if index.insert(put.key, &put, per_chunk).is_err() {
                break;
            }
            sequence = sequence.max(put.sequence);
            report.rolled_forward = report.rolled_forward.saturating_add(1);
            at = at.saturating_add(u64::from(put.record_len));
        }
        let end = at.checked_next_multiple_of(geometry.block).unwrap_or(at);
        info.write_pos = u32::try_from(end).unwrap_or(u32::MAX).max(info.write_pos);
    }

    // Live bytes from the index.
    let mut fragments = 0u64;
    for (_, entry) in index.iter() {
        for f in &entry.fragments {
            fragments = fragments.saturating_add(1);
            if let Some(info) = segments.get_mut(usize::try_from(f.segment).unwrap_or(usize::MAX)) {
                info.live = info.live.saturating_add(u64::from(f.record_len));
                info.youngest_ns = info.youngest_ns.max(entry.time_ns);
            }
        }
    }

    open.sort_unstable_by(|a, b| b.cmp(a));
    let open: Vec<u32> = open.into_iter().map(|(_, s)| s).collect();
    let used = distance(sb.start_pos, pos, geometry.log_size);
    Ok(Recovered {
        index,
        segments,
        cursor: Cursor {
            pos,
            lsn,
            start_pos: sb.start_pos,
            start_lsn: sb.start_lsn,
            used,
        },
        sequence,
        incarnation,
        fragments,
        open,
        report,
    })
}

/// Whether the log shows that the missing frame `lsn` had been flushed: a valid frame of this
/// volume from a flush group that began after it. A new group is written only after the
/// previous group's flush completes, so such a frame proves the missing one was durable and
/// has since been damaged. Frames of the same group (a wrap and its batch, the frames of a
/// checkpoint) can survive a crash that tore an earlier one, and prove nothing.
///
/// Every block of the log is examined, through the reader's window; only a frame whose header
/// names such a group is verified whole, since the header it is verified with is the same.
fn later_frame_exists<F: BlockFile>(
    log: &mut Frames<'_, F>,
    geometry: &Geometry,
    lsn: u64,
) -> Result<bool, ChunkError> {
    let mut pos = 0u64;
    while pos < geometry.log_size {
        if let Some((header, _)) = log.header(pos).map_err(ChunkError::Device)?
            && header.lsn >= lsn
            && header.group > lsn
            && log.frame(pos).map_err(ChunkError::Device)?.is_some()
        {
            return Ok(true);
        }
        pos = pos.saturating_add(geometry.block);
    }
    Ok(false)
}

/// A data record read back and verified in full.
pub(crate) struct Verified {
    pub prefix: Prefix,
    pub payload_crc: u32,
    pub record_len: u32,
}

impl Verified {
    /// Whether this is the record of `key` that fragment `f` names.
    pub(crate) fn is(&self, key: &ChunkKey, f: &Fragment) -> bool {
        let h = &self.prefix.header;
        h.key == *key
            && h.incarnation == f.incarnation
            && h.sequence == f.sequence
            && h.chunk_offset == f.chunk_offset
            && h.payload_len == f.payload_len
            && self.payload_crc == f.payload_crc
    }

    fn matches(&self, put: &PutRecord) -> bool {
        let h = &self.prefix.header;
        h.key == put.key
            && h.incarnation == put.incarnation
            && h.sequence == put.sequence
            && h.chunk_offset == put.chunk_offset
            && h.payload_len == put.payload_len
            && self.payload_crc == put.payload_crc
    }

    fn as_put(&self, segment: u32, offset: u64) -> PutRecord {
        let h = &self.prefix.header;
        PutRecord {
            key: h.key,
            segment,
            incarnation: h.incarnation,
            offset: u32::try_from(offset).unwrap_or(u32::MAX),
            record_len: self.record_len,
            chunk_offset: h.chunk_offset,
            payload_len: h.payload_len,
            payload_crc: self.payload_crc,
            sequence: h.sequence,
            time_ns: h.time_ns,
            flags: h.flags,
        }
    }
}

/// What every record of a volume carries: the volume's ID and its checksum block size.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Identity {
    pub volume: u128,
    pub checksum_shift: u8,
}

impl Identity {
    pub fn of(sb: &Superblock) -> Self {
        Self {
            volume: sb.volume,
            checksum_shift: sb.checksum_shift,
        }
    }
}

/// Reads the data record at `offset` in `segment` and verifies its header, identity and
/// every checksum block, appending its payload to `payload` when given one. `None` if no
/// intact record of this volume and segment is there.
pub(crate) fn verify_at<F: BlockFile>(
    file: &F,
    pool: &mut ReadPool,
    geometry: &Geometry,
    identity: Identity,
    segment: u32,
    offset: u64,
    payload_out: Option<&mut Vec<u8>>,
) -> Result<Option<Verified>, ChunkError> {
    let Some(base) = geometry.segment_offset(segment) else {
        return Ok(None);
    };
    // The header first: it says how long the record is.
    let Some(header_span) = read_span(
        file,
        pool,
        geometry,
        base,
        offset,
        record::HEADER_LEN as u64,
    )?
    else {
        return Ok(None);
    };
    let record_len = record::peek_lengths(header_span.bytes())
        .and_then(|first| record::record_len(first.payload_len, first.checksum_shift));
    pool.give(header_span);
    let Some(record_len) = record_len else {
        return Ok(None);
    };
    let record_len64 = u64::try_from(record_len).unwrap_or(u64::MAX);
    if offset.saturating_add(record_len64) > geometry.segment_size {
        return Ok(None);
    }
    let Some(span) = read_span(file, pool, geometry, base, offset, record_len64)? else {
        return Ok(None);
    };
    let verified = verify_record(span.bytes(), identity, segment, record_len, payload_out);
    pool.give(span);
    Ok(verified)
}

/// The record in `bytes`, if it is one of this volume and segment whose every checksum block
/// verifies; its payload is appended to `payload_out` when given one.
fn verify_record(
    bytes: &[u8],
    identity: Identity,
    segment: u32,
    record_len: usize,
    payload_out: Option<&mut Vec<u8>>,
) -> Option<Verified> {
    let prefix = record::decode_prefix(bytes)?;
    let h = &prefix.header;
    if h.volume != identity.volume
        || h.segment != segment
        || h.checksum_shift != identity.checksum_shift
    {
        return None;
    }
    let start = h.prefix_len()?;
    let payload =
        bytes.get(start..start.saturating_add(usize::try_from(h.payload_len).unwrap_or(0)))?;
    if !record::verify(&prefix, 0, payload) {
        return None;
    }
    let payload_crc = mantle_crc::crc32c(payload);
    if let Some(out) = payload_out {
        out.extend_from_slice(payload);
    }
    Some(Verified {
        prefix,
        payload_crc,
        record_len: u32::try_from(record_len).unwrap_or(u32::MAX),
    })
}

/// Buffers for reading records, each pool owned by the one thread that reads with it: the
/// writer, the cleaner and the scrubber their own, recovery its own, and each caller of a read
/// its own (`ReadBuffers`), as hyper-block's pool has one owner, the thread that issues the I/O
/// its buffers carry (hyper-block buf.rs). Each buffer is lent for one read and given back
/// once the span read into it has been looked at.
pub(crate) struct ReadPool(Pool);

impl ReadPool {
    pub(crate) fn new(pool: Pool) -> Self {
        Self(pool)
    }

    /// The pool each reader of a volume keeps: at most one batch's bytes free, in no buffer
    /// larger than a batch, which holds the largest record the writer lays out. A reader keeps
    /// what its own reads took, at most that.
    pub(crate) fn of_batch(align: hyper_block::buf::Alignment, batch_bytes: usize) -> Self {
        Self(Pool::new(align, batch_bytes, batch_bytes))
    }

    /// An empty buffer of at least `capacity` bytes, lent until its span is given back.
    fn take(&mut self, capacity: usize) -> Result<AlignedBuf, ChunkError> {
        self.0
            .take(capacity)
            .map_err(|e| ChunkError::Device(e.into()))
    }

    /// Takes back the buffer of a span read through this pool. A span dropped instead, on a
    /// path that gives up on its read, frees its buffer.
    pub(crate) fn give(&mut self, span: Span) {
        self.0.give(span.buf);
    }
}

/// Bytes read from the device into a buffer lent from a [`ReadPool`], given back with
/// [`ReadPool::give`].
pub(crate) struct Span {
    buf: AlignedBuf,
    skip: usize,
    len: usize,
}

impl Span {
    pub fn bytes(&self) -> &[u8] {
        self.buf
            .as_slice()
            .get(self.skip..self.skip.saturating_add(self.len))
            .unwrap_or_default()
    }
}

/// Reads `len` bytes at `offset` within the segment at `base`, through an aligned buffer
/// from `pool`. `None` if the span runs past the end of the file.
pub(crate) fn read_span<F: BlockFile>(
    file: &F,
    pool: &mut ReadPool,
    geometry: &Geometry,
    base: u64,
    offset: u64,
    len: u64,
) -> Result<Option<Span>, ChunkError> {
    let block = geometry.block;
    let start = base.checked_add(offset).ok_or(ChunkError::Full)?;
    let end = start.checked_add(len).ok_or(ChunkError::Full)?;
    let aligned_start = start
        .checked_div(block)
        .and_then(|b| b.checked_mul(block))
        .ok_or(ChunkError::Full)?;
    let aligned_end = end
        .checked_next_multiple_of(block)
        .ok_or(ChunkError::Full)?;
    let span_len =
        usize::try_from(aligned_end.saturating_sub(aligned_start)).map_err(|_| ChunkError::Full)?;
    let mut buf = pool.take(span_len)?;
    let read = buf
        .set_len(span_len)
        .map_err(|e| ChunkError::Device(e.into()))
        .and_then(
            |()| match file.read_exact_at(buf.as_mut_slice(), aligned_start) {
                Ok(()) => Ok(true),
                Err(DiskError::ShortRead { .. }) => Ok(false),
                Err(e) => Err(ChunkError::Device(e)),
            },
        );
    let skip = usize::try_from(start.saturating_sub(aligned_start)).unwrap_or(0);
    let len = usize::try_from(len).unwrap_or(0);
    let span = Span { buf, skip, len };
    match read {
        Ok(true) if skip.saturating_add(len) <= span_len => Ok(Some(span)),
        Ok(_) => {
            pool.give(span);
            Ok(None)
        }
        Err(e) => {
            pool.give(span);
            Err(e)
        }
    }
}
