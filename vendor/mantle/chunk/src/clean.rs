//! Reclaiming the space of deleted chunks (docs/design/chunk-store.md §8).
//!
//! A segment whose chunks are all deleted is freed by the writer without copying anything.
//! One that is partly dead is cleaned: its live fragments are read back, verified, and handed
//! to the writer as relocations into the cleaner's own stream; once nothing live is left in
//! it, the writer frees it. Cleaning gains space only by writing the live data back to fewer
//! segments than it came from (Rosenblum and Ousterhout, TOCS 1992, §3.3), and a record never
//! spans segments, so space a segment leaves beside a record too large to share it is not
//! space cleaning can give back. A pass therefore lays out its victims' relocations first,
//! from the index's record lengths and by the writer's own rule, and cleans only victims that
//! pack into fewer segments than they free: one that cannot gain moves nothing. Victims are
//! chosen by LFS's cost-benefit ratio
//! `(1 − u) · age / (1 + u)`, where `u` is the fraction still live and `age` the time since
//! its youngest live record was written (Rosenblum and Ousterhout, TOCS 1992, §3.6): old,
//! mostly dead segments first, since cold data that survived is likely to stay.
//!
//! The cleaner runs, woken by the writer after each batch, when free segments fall below a
//! runway (`runway`): enough to take writes at the fastest rate seen while the cleaner reacts
//! and cleans one victim. LFS chose its thresholds without study and found performance
//! insensitive to them, and a fraction of the volume scales with the disk rather than the
//! write rate (docs/research/11 §10).

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use hyper_block::DiskError;
use hyper_block::block::BlockFile;

use crate::error::ChunkError;
use crate::frame::SegmentState;
use crate::record::{self, FLAG_FINAL, Payload};
use crate::recover::{Identity, ReadPool, verify_at};
use crate::scrub::damaged_owner;
use crate::writer::{CLEANER_RESERVE, Move, Op, Reply, Request, Shared};

pub(crate) struct Cleaner<F> {
    pub shared: Arc<Shared<F>>,
    pub submit: SyncSender<Request>,
    pub wake: Receiver<()>,
    /// Payload bytes and fragments per relocation request: one writer batch each.
    pub batch_bytes: usize,
    pub batch_moves: usize,
}

/// Free segments to hold before cleaning starts: the client's reserve (writer.rs) and the
/// runway, `W·(t_react + t_clean) + D` bytes, where `W` is the fastest payload rate seen,
/// `t_react` a batch (the writer wakes the cleaner after each), `t_clean` the longest one
/// victim has taken, and `D` one batch arriving between wakes (docs/research/11 §10.3). Until
/// a victim has been cleaned, `W·t_clean` is taken as two segments: reading and rewriting a
/// wholly live victim at the rate writes arrive.
pub(crate) fn runway(
    rate: u64,
    react_ns: u64,
    clean_ns: u64,
    batch_bytes: u64,
    segment_size: u64,
) -> usize {
    let per_ns = |ns: u64| {
        u128::from(rate)
            .saturating_mul(u128::from(ns))
            .checked_div(1_000_000_000)
            .unwrap_or(0)
    };
    let cleaning = if clean_ns == 0 {
        u128::from(segment_size).saturating_mul(2)
    } else {
        per_ns(clean_ns)
    };
    let bytes = per_ns(react_ns)
        .saturating_add(cleaning)
        .saturating_add(u128::from(batch_bytes));
    let segments = bytes.div_ceil(u128::from(segment_size.max(1)));
    usize::try_from(segments)
        .unwrap_or(usize::MAX)
        .saturating_add(CLEANER_RESERVE)
}

/// What one cleaning pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CleanReport {
    pub segments: u32,
    /// Free segments gained.
    pub gained: u32,
    pub relocated: u64,
    /// Live fragments that failed verification and were left in place for repair.
    pub corrupt: u64,
}

/// The cleaner's stream as relocations would leave it, laid out by the writer's rule
/// (`Layout::place`): a record goes where the open segment's next one would if it fits there,
/// or seals the segment and opens another, and a batch ends padded to a block, where the
/// segment's next batch starts.
struct Packing {
    /// Where the open segment's next record goes; none before the stream opens one.
    at: Option<u64>,
    /// Segments opened.
    opened: u64,
    size: u64,
    block: u64,
}

impl Packing {
    fn place(&mut self, len: u64) {
        self.at = match self.at {
            Some(at) if at.saturating_add(len) <= self.size => Some(at.saturating_add(len)),
            _ => {
                self.opened = self.opened.saturating_add(1);
                Some(self.block.saturating_add(len))
            }
        };
    }

    fn end_batch(&mut self) {
        self.at = self
            .at
            .map(|at| at.checked_next_multiple_of(self.block).unwrap_or(at));
    }
}

impl<F: BlockFile> Cleaner<F> {
    pub fn run(self) {
        // The cleaner thread's own read buffers.
        let mut reads = self.reads();
        // Every change to free space runs a writer batch, and every batch below the runway
        // wakes the cleaner, so it waits for nothing else.
        while self.wake.recv().is_ok() {
            if self.shared.stopping.load(Ordering::Acquire) {
                return;
            }
            if self.shared.fenced.load(Ordering::Acquire) {
                continue;
            }
            // Until more data dies, another pass would copy the same live data around for
            // nothing (`Shared::futile_at`).
            let dead = self.shared.dead_bytes.load(Ordering::Relaxed);
            if self.shared.futile_at.load(Ordering::Relaxed) == dead {
                continue;
            }
            // A failed pass (the volume fenced, or it closed) ends this round; the next wake
            // retries if there is still work.
            // A pass that ran and gained nothing, having found no victim worth cleaning or
            // cleaned some to no gain, is futile until more data dies.
            if let Ok(Some(report)) = self.pass(&mut reads) {
                let futile = if report.gained == 0 { dead } else { u64::MAX };
                self.shared.futile_at.store(futile, Ordering::Relaxed);
                // The writes the writer holds for this verdict run again after its next batch:
                // one now. A volume closing refuses it, and its writer answers them itself.
                let _ = self.flush();
            }
        }
    }

    /// Read buffers for one cleaning thread, a batch's at most: a pass reads one record at a
    /// time and relocates a batch at a time.
    pub fn reads(&self) -> ReadPool {
        ReadPool::of_batch(self.shared.file.alignment(), self.batch_bytes)
    }

    fn free(&self) -> usize {
        self.shared
            .usage
            .read()
            .map_or(0, |usage| usage.free_count())
    }

    /// Cleans victims until free segments are one past the runway, or none is worth it;
    /// `None` when free segments are at the runway already.
    pub fn pass(&self, reads: &mut ReadPool) -> Result<Option<CleanReport>, ChunkError> {
        let low = self.shared.low_water.load(Ordering::Relaxed);
        if self.free() >= low {
            return Ok(None);
        }
        self.clean_until(reads, u32::MAX, |cleaner| {
            cleaner.free() <= cleaner.shared.low_water.load(Ordering::Relaxed)
        })
        .map(Some)
    }

    /// Cleans at most the `n` best victims, whatever the free space, as many as gain from it.
    pub fn clean_best(&self, reads: &mut ReadPool, n: u32) -> Result<CleanReport, ChunkError> {
        self.clean_until(reads, n, |_| true)
    }

    /// Cleans the victims each plan (`plan`) gains from, at most `most`, while `more` holds.
    fn clean_until(
        &self,
        reads: &mut ReadPool,
        most: u32,
        more: impl Fn(&Self) -> bool,
    ) -> Result<CleanReport, ChunkError> {
        // One pass at a time, background or asked for, so two never take the same victims.
        let _cleaning = self
            .shared
            .cleaning
            .lock()
            .map_err(|_| ChunkError::Fenced)?;
        let mut report = CleanReport::default();
        let mut tried = HashSet::new();
        // What the pass gains: the victims it frees, less the segments its relocations open.
        // Not the change in free segments, which client writes during the pass spend as well:
        // a pass that freed space would look futile, and futility answers writes `Full`.
        let spent_before = self.shared.clean_opened.load(Ordering::Relaxed);
        let mut freed = 0u64;
        let gained = |freed: u64| {
            let spent = self
                .shared
                .clean_opened
                .load(Ordering::Relaxed)
                .saturating_sub(spent_before);
            freed.saturating_sub(spent)
        };
        'pass: while more(self) {
            let victims = self.plan(&mut tried, most.saturating_sub(report.segments))?;
            if victims.is_empty() {
                break;
            }
            for (victim, incarnation) in victims {
                // A victim is the segment its plan chose, that incarnation: one whose records all
                // died since was freed by the writer, which the plan counted on, and may have
                // been opened again under its number for this pass's own relocations. Cleaned
                // as the victim, it was the pass's own output, moved again into the last free
                // segment and never freed, every later pass finding no segment to move into.
                if self.incarnation(victim) != Some(incarnation) {
                    freed = freed.saturating_add(1);
                    continue;
                }
                let started = Instant::now();
                let places = self.places(victim)?;
                let (relocated, corrupt) = match self.clean(reads, victim, incarnation, places) {
                    Ok(cleaned) => cleaned,
                    // No segment left for its relocations: the pass ends with what it gained,
                    // its verdict given, rather than failing with none and holding the writes
                    // that wait on it.
                    Err(ChunkError::Full) => break 'pass,
                    Err(e) => return Err(e),
                };
                report.segments = report.segments.saturating_add(1);
                report.relocated = report.relocated.saturating_add(relocated);
                report.corrupt = report.corrupt.saturating_add(corrupt);
                // The writer frees a segment in the batch after the one that emptied it.
                self.flush()?;
                // Freed, and perhaps opened again since by another batch.
                if self.incarnation(victim) != Some(incarnation) {
                    freed = freed.saturating_add(1);
                }
                self.learn(started.elapsed());
            }
        }
        report.gained = u32::try_from(gained(freed)).unwrap_or(u32::MAX);
        Ok(report)
    }

    /// The victims to clean next, each with the incarnation it was chosen in, in the order
    /// `victim` chooses them: the fewest, at most `most`, whose live records, laid out as the
    /// writer will lay out their relocations, open fewer segments than cleaning them frees; none
    /// when no such victims come before their gains (`gain`) are due a segment. Laid out from the
    /// index's record lengths, a plan reads and writes nothing, so a pass that could only move
    /// data around moves none.
    fn plan(&self, tried: &mut HashSet<u32>, most: u32) -> Result<Vec<(u32, u64)>, ChunkError> {
        let geometry = self.shared.geometry;
        let mut packing = Packing {
            at: self.clean_position()?,
            opened: 0,
            size: geometry.segment_size,
            block: geometry.block,
        };
        let mut victims = Vec::new();
        // Space the chosen victims should give back once their live data is packed.
        let mut due = 0u64;
        while u32::try_from(victims.len()).is_ok_and(|chosen| chosen < most) {
            let Some((victim, incarnation)) = self.victim(tried) else {
                break;
            };
            tried.insert(victim);
            victims.push((victim, incarnation));
            self.pack(&mut packing, victim)?;
            if u64::try_from(victims.len()).is_ok_and(|freed| freed > packing.opened) {
                return Ok(victims);
            }
            // Victims of live fraction u free 1 − u segments each (Rosenblum and Ousterhout,
            // TOCS 1992, §3.4), so a net gain is due once they have held a segment's worth of
            // dead space, after ⌈1/(1 − ū)⌉ of them (docs/research/11 §10.3), less what packing
            // their live data costs. None by then means it packs no tighter.
            due = due.saturating_add(self.gain(self.live(victim)));
            if due >= geometry.segment_size {
                break;
            }
        }
        Ok(Vec::new())
    }

    /// Lays `segment`'s live records into `packing` as cleaning it relocates them: in the order
    /// they lie, in batches of `batch_moves` records or `batch_bytes` of payload and one of
    /// what is left (`clean`).
    fn pack(&self, packing: &mut Packing, segment: u32) -> Result<(), ChunkError> {
        let index = self.shared.index.read().map_err(|_| ChunkError::Fenced)?;
        let (mut moves, mut bytes) = (0usize, 0usize);
        for (_, payload) in index.records_in(segment) {
            let len = record::record_len(payload, self.shared.checksum_shift)
                .and_then(|len| u64::try_from(len).ok())
                .ok_or(ChunkError::Internal(
                    "an indexed record's length overflowed",
                ))?;
            packing.place(len);
            moves = moves.saturating_add(1);
            bytes = bytes.saturating_add(usize::try_from(payload).unwrap_or(usize::MAX));
            if moves >= self.batch_moves || bytes >= self.batch_bytes {
                packing.end_batch();
                (moves, bytes) = (0, 0);
            }
        }
        if moves > 0 {
            packing.end_batch();
        }
        Ok(())
    }

    /// Where the cleaner's stream puts its next record, as of the last durable batch: in its
    /// open segment, or none before it opens one.
    fn clean_position(&self) -> Result<Option<u64>, ChunkError> {
        let Ok(open) = u32::try_from(self.shared.clean_open.load(Ordering::Relaxed)) else {
            return Ok(None);
        };
        let usage = self.shared.usage.read().map_err(|_| ChunkError::Fenced)?;
        Ok(usage
            .get(open)
            .filter(|s| s.state == SegmentState::Open)
            .map(|s| u64::from(s.write_pos).max(self.shared.geometry.block)))
    }

    /// Space cleaning a segment holding `live` live bytes gives back: its dead bytes, less
    /// what packing its live data costs, a block of padding per relocation batch and a share
    /// of a segment header.
    fn gain(&self, live: u64) -> u64 {
        let (size, block) = (
            self.shared.geometry.segment_size,
            self.shared.geometry.block,
        );
        let batch = u64::try_from(self.batch_bytes).unwrap_or(u64::MAX).max(1);
        let packing = live.div_ceil(batch).saturating_add(1).saturating_mul(block);
        size.saturating_sub(live).saturating_sub(packing)
    }

    /// The incarnation of `segment`, or none while it is free.
    fn incarnation(&self, segment: u32) -> Option<u64> {
        self.shared.usage.read().ok().and_then(|usage| {
            usage
                .get(segment)
                .filter(|s| s.state != SegmentState::Free)
                .map(|s| s.incarnation)
        })
    }

    /// Live bytes of `segment`.
    fn live(&self, segment: u32) -> u64 {
        self.shared
            .usage
            .read()
            .map_or(0, |usage| usage.get(segment).map_or(0, |s| s.live))
    }

    /// Records how long a victim took and moves the runway to match.
    fn learn(&self, took: Duration) {
        let took = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        let shared = &self.shared;
        let clean_ns = shared.clean_ns.fetch_max(took, Ordering::Relaxed).max(took);
        let low = runway(
            shared.peak_rate.load(Ordering::Relaxed),
            shared.service_ns.load(Ordering::Relaxed),
            clean_ns,
            u64::try_from(self.batch_bytes).unwrap_or(u64::MAX),
            shared.geometry.segment_size,
        );
        shared.low_water.store(low, Ordering::Relaxed);
    }

    /// The sealed segment with the best cost-benefit ratio of those cleaning gains space from.
    /// A segment whose dead bytes do not cover packing its live data is none of them: cleaning
    /// it only moves its data, and a pass over such segments alone, as a volume filled in
    /// chunks of nearly a segment each leaves, would move every one and gain nothing. Scores
    /// change with the clock, so each victim is chosen over every segment: one pass of the
    /// table against the victim's relocation, which reads and writes what is live in it.
    fn victim(&self, tried: &HashSet<u32>) -> Option<(u32, u64)> {
        let usage = self.shared.usage.read().ok()?;
        let size = self.shared.geometry.segment_size;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        usage
            .iter()
            .filter(|(i, s)| {
                s.state == SegmentState::Sealed && self.gain(s.live) > 0 && !tried.contains(i)
            })
            .map(|(i, s)| {
                // Scores are compared, not reported: f64's rounding does not change the order
                // that matters.
                let u = s.live as f64 / size as f64;
                let age = now.saturating_sub(s.youngest_ns) as f64;
                ((1.0 - u) * age / (1.0 + u), i, s.incarnation)
            })
            .filter(|(score, _, _)| score.is_finite())
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, i, incarnation)| (i, incarnation))
    }

    /// Where `segment`'s live records lie now, by the index's record places: what a clean of it
    /// reads ([`Self::clean`]).
    pub(crate) fn places(&self, segment: u32) -> Result<Vec<u32>, ChunkError> {
        Ok(self
            .shared
            .index
            .read()
            .map_err(|_| ChunkError::Fenced)?
            .records_in(segment)
            .map(|(offset, _)| offset)
            .collect())
    }

    /// Relocates every live record of `segment` in its `incarnation`, the one its plan chose,
    /// among those at `offsets`, its places as listed before ([`Self::places`]), each checked
    /// against the index as it is read; returns (relocated, corrupt).
    pub(crate) fn clean(
        &self,
        reads: &mut ReadPool,
        segment: u32,
        incarnation: u64,
        offsets: Vec<u32>,
    ) -> Result<(u64, u64), ChunkError> {
        let identity = Identity {
            volume: self.shared.volume,
            checksum_shift: self.shared.checksum_shift,
        };
        let (mut relocated, mut corrupt) = (0u64, 0u64);
        let mut moves: Vec<Move> = Vec::new();
        let mut bytes = 0usize;
        for offset in offsets {
            let mut data = Vec::new();
            let read = verify_at(
                &self.shared.file,
                reads,
                &self.shared.geometry,
                identity,
                segment,
                u64::from(offset),
                Some(&mut data),
            );
            let header = match read {
                Ok(Some(v)) if v.prefix.header.incarnation == incarnation => v.prefix.header,
                // Every record of the victim died while it was being cleaned, and the writer
                // freed it: what lies here now is another incarnation's, or nothing.
                Ok(_) | Err(ChunkError::Device(DiskError::Io { .. }))
                    if self.incarnation(segment) != Some(incarnation) =>
                {
                    break;
                }
                // A record that died since its place was listed is no damage, as the scrubber
                // judges: a batch that frees its segment may open it again in the same frame and
                // write there before the segment table says so (`Writer::layout`, `publish`).
                Ok(_) | Err(ChunkError::Device(DiskError::Io { .. }))
                    if damaged_owner(&self.shared, segment, offset, incarnation)?.is_none() =>
                {
                    continue;
                }
                // Left in place: moving bytes that fail verification would launder them.
                Ok(_) | Err(ChunkError::Device(DiskError::Io { .. })) => {
                    corrupt = corrupt.saturating_add(1);
                    continue;
                }
                Err(e) => return Err(e),
            };
            // Only a record the index still holds at this place is live.
            let found = {
                let index = self.shared.index.read().map_err(|_| ChunkError::Fenced)?;
                index
                    .live(
                        &header.key,
                        header.chunk_offset,
                        (segment, offset),
                        header.incarnation,
                        header.sequence,
                    )
                    .and_then(|from| {
                        let entry = index.get(&header.key)?;
                        let last = entry.fragments.last().map(|f| f.chunk_offset);
                        let flags = if entry.sealed && last == Some(from.chunk_offset) {
                            FLAG_FINAL
                        } else {
                            0
                        };
                        Some((from, flags, entry.time_ns))
                    })
            };
            let Some((from, flags, time_ns)) = found else {
                continue;
            };
            bytes = bytes.saturating_add(data.len());
            let payload = Payload::new(data, self.shared.checksum_shift)
                .ok_or_else(|| ChunkError::Config("checksum block size".into()))?;
            moves.push(Move {
                key: header.key,
                from,
                payload,
                flags,
                time_ns,
            });
            if moves.len() >= self.batch_moves || bytes >= self.batch_bytes {
                relocated = relocated.saturating_add(self.relocate(std::mem::take(&mut moves))?);
                bytes = 0;
            }
        }
        if !moves.is_empty() {
            relocated = relocated.saturating_add(self.relocate(moves)?);
        }
        Ok((relocated, corrupt))
    }

    /// Hands one batch of moves to the writer and waits for it to be durable.
    fn relocate(&self, moves: Vec<Move>) -> Result<u64, ChunkError> {
        let count = u64::try_from(moves.len()).unwrap_or(u64::MAX);
        let (reply, answer) = sync_channel(1);
        self.shared.submitted.fetch_add(1, Ordering::AcqRel);
        self.submit
            .send(Request {
                op: Op::Relocate { moves },
                reply: Reply::new(reply, None),
                queued: None,
            })
            .map_err(|_| ChunkError::Closed)?;
        answer.recv().map_err(|_| ChunkError::Closed)??;
        Ok(count)
    }

    /// Has the writer run a batch that frees the segments left empty, and waits for it.
    fn flush(&self) -> Result<(), ChunkError> {
        let (reply, answer) = sync_channel(1);
        self.shared.submitted.fetch_add(1, Ordering::AcqRel);
        self.submit
            .send(Request {
                op: Op::Free,
                reply: Reply::new(reply, None),
                queued: None,
            })
            .map_err(|_| ChunkError::Closed)?;
        answer.recv().map_err(|_| ChunkError::Closed)?
    }
}
